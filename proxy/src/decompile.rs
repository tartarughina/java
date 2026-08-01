use crate::{lsp_error, pending::PendingResponses};
use proxy_common::{encode_lsp, path_to_file_uri};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const DECOMPILED_DIR: &str = "jdtls-decompiled";
const FETCH_WORKERS: usize = 2;
const JOB_WORKERS: usize = 2;
const MAX_QUEUED_JOBS: usize = 64;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(2);

pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteMode {
    Locations,
    Strings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Interactive,
    Bulk,
}

pub struct RewriteJob {
    pub token: u64,
    pub message: Value,
    pub mode: RewriteMode,
    pub priority: Priority,
    pub deadline: Instant,
    pub complete: Box<dyn FnOnce(Value) + Send>,
}

#[derive(Clone)]
pub struct DecompileCoordinator {
    inner: Arc<Inner>,
}

struct Inner {
    fetcher: Arc<dyn ClassContentFetcher>,
    owned_id_prefix: String,
    request_counter: AtomicU64,
    max_bulk_jobs: usize,
    state: Mutex<State>,
    work_available: Condvar,
    state_changed: Condvar,
}

#[derive(Default)]
struct State {
    uris: HashMap<String, UriEntry>,
    interactive_uris: VecDeque<String>,
    bulk_uris: VecDeque<String>,
    interactive_jobs: VecDeque<RewriteJob>,
    bulk_jobs: VecDeque<RewriteJob>,
    canceled_jobs: HashSet<u64>,
    latest_bulk_job: Option<u64>,
    bulk_jobs_active: usize,
    bulk_fetches: usize,
    shutdown: bool,
}

struct UriEntry {
    status: UriStatus,
    waiters: HashSet<u64>,
}

enum UriStatus {
    Queued(Priority),
    InFlight { request_id: Value },
    Ready(String),
    Failed(Instant),
}

trait ClassContentFetcher: Send + Sync {
    fn fetch(&self, uri: &str, request_id: Value) -> Option<String>;
    fn cancel(&self, request_id: &Value);
}

struct JdtlsFetcher {
    writer: SharedWriter,
    pending: Arc<PendingResponses>,
}

impl ClassContentFetcher for JdtlsFetcher {
    fn fetch(&self, uri: &str, request_id: Value) -> Option<String> {
        let receiver = self.pending.register(request_id.clone());
        let request = encode_lsp(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "java/classFileContents",
            "params": { "uri": uri }
        }));

        let write_succeeded = {
            let mut writer = self.writer.lock().unwrap();
            writer.write_all(request.as_bytes()).is_ok() && writer.flush().is_ok()
        };
        if !write_succeeded {
            self.pending.remove(&request_id);
            return None;
        }

        match receiver.recv_timeout(FETCH_TIMEOUT) {
            Ok(response) => response
                .get("result")
                .and_then(Value::as_str)
                .filter(|content| !content.is_empty())
                .map(str::to_string),
            Err(_) => {
                self.pending.remove(&request_id);
                None
            }
        }
    }

    fn cancel(&self, request_id: &Value) {
        self.pending.remove(request_id);
        let cancel = encode_lsp(&json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": request_id }
        }));
        let mut writer = self.writer.lock().unwrap();
        let _ = writer.write_all(cancel.as_bytes());
        let _ = writer.flush();
    }
}

impl DecompileCoordinator {
    pub fn new(
        writer: SharedWriter,
        pending: Arc<PendingResponses>,
        owned_id_prefix: String,
    ) -> Self {
        Self::with_fetcher(
            Arc::new(JdtlsFetcher { writer, pending }),
            owned_id_prefix,
            FETCH_WORKERS,
            JOB_WORKERS,
        )
    }

    fn with_fetcher(
        fetcher: Arc<dyn ClassContentFetcher>,
        owned_id_prefix: String,
        fetch_workers: usize,
        job_workers: usize,
    ) -> Self {
        let coordinator = Self {
            inner: Arc::new(Inner {
                fetcher,
                owned_id_prefix,
                request_counter: AtomicU64::new(1),
                max_bulk_jobs: job_workers.saturating_sub(1).max(1),
                state: Mutex::new(State::default()),
                work_available: Condvar::new(),
                state_changed: Condvar::new(),
            }),
        };

        for _ in 0..fetch_workers {
            let inner = Arc::clone(&coordinator.inner);
            thread::spawn(move || fetch_worker(inner));
        }
        for _ in 0..job_workers {
            let inner = Arc::clone(&coordinator.inner);
            thread::spawn(move || job_worker(inner));
        }

        coordinator
    }

    /// Enqueues without blocking the JDTLS stdout router. On saturation the
    /// original job is returned so the caller can forward it unchanged.
    pub fn submit(&self, job: RewriteJob) -> Result<(), RewriteJob> {
        let mut state = self.inner.state.lock().unwrap();
        if state.shutdown || state.interactive_jobs.len() + state.bulk_jobs.len() >= MAX_QUEUED_JOBS
        {
            return Err(job);
        }

        if job.priority == Priority::Bulk {
            if let Some(previous) = state.latest_bulk_job.replace(job.token) {
                if cancel_job_locked(&mut state, previous) {
                    state.canceled_jobs.remove(&previous);
                }
            }
            state.bulk_jobs.push_back(job);
        } else {
            state.interactive_jobs.push_back(job);
        }
        self.inner.work_available.notify_all();
        Ok(())
    }

    pub fn cancel(&self, token: u64) {
        let request_ids = {
            let mut state = self.inner.state.lock().unwrap();
            if cancel_job_locked(&mut state, token) {
                state.canceled_jobs.remove(&token);
            }
            orphaned_request_ids(&state)
        };
        for request_id in request_ids {
            self.inner.fetcher.cancel(&request_id);
        }
        self.inner.state_changed.notify_all();
        self.inner.work_available.notify_all();
    }

    pub fn consume_cancellation(&self, token: u64) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .canceled_jobs
            .remove(&token)
    }

    pub fn is_canceled(&self, token: u64) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .canceled_jobs
            .contains(&token)
    }

    pub fn shutdown(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.shutdown = true;
        state.interactive_jobs.clear();
        state.bulk_jobs.clear();
        self.inner.work_available.notify_all();
        self.inner.state_changed.notify_all();
    }
}

fn cancel_job_locked(state: &mut State, token: u64) -> bool {
    state.canceled_jobs.insert(token);
    let queued_jobs = state.interactive_jobs.len() + state.bulk_jobs.len();
    state
        .interactive_jobs
        .retain(|queued| queued.token != token);
    state.bulk_jobs.retain(|queued| queued.token != token);
    for entry in state.uris.values_mut() {
        entry.waiters.remove(&token);
    }
    queued_jobs != state.interactive_jobs.len() + state.bulk_jobs.len()
}

fn orphaned_request_ids(state: &State) -> Vec<Value> {
    state
        .uris
        .values()
        .filter(|entry| entry.waiters.is_empty())
        .filter_map(|entry| match &entry.status {
            UriStatus::InFlight { request_id, .. } => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

fn job_worker(inner: Arc<Inner>) {
    loop {
        let job = {
            let mut state = inner.state.lock().unwrap();
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(job) = state.interactive_jobs.pop_front() {
                    break job;
                }
                if state.bulk_jobs_active < inner.max_bulk_jobs {
                    if let Some(job) = state.bulk_jobs.pop_front() {
                        state.bulk_jobs_active += 1;
                        break job;
                    }
                }
                state = inner.work_available.wait(state).unwrap();
            }
        };

        if is_canceled(&inner, job.token) {
            finish_job(&inner, job.token, job.priority);
            continue;
        }

        let mut message = job.message;
        rewrite_message(
            &inner,
            job.token,
            &mut message,
            job.mode,
            job.priority,
            job.deadline,
        );
        let canceled = is_canceled(&inner, job.token);
        finish_job(&inner, job.token, job.priority);
        if !canceled {
            (job.complete)(message);
        }
    }
}

fn is_canceled(inner: &Inner, token: u64) -> bool {
    inner.state.lock().unwrap().canceled_jobs.contains(&token)
}

fn finish_job(inner: &Inner, token: u64, priority: Priority) {
    let mut state = inner.state.lock().unwrap();
    if priority == Priority::Bulk {
        state.bulk_jobs_active = state.bulk_jobs_active.saturating_sub(1);
    }
    state.canceled_jobs.remove(&token);
    if state.latest_bulk_job == Some(token) {
        state.latest_bulk_job = None;
    }
    for entry in state.uris.values_mut() {
        entry.waiters.remove(&token);
    }
    let now = Instant::now();
    state.uris.retain(|_, entry| {
        !entry.waiters.is_empty()
            || matches!(entry.status, UriStatus::InFlight { .. })
            || matches!(entry.status, UriStatus::Failed(retry_after) if retry_after > now)
    });
    inner.work_available.notify_all();
}

fn rewrite_message(
    inner: &Inner,
    token: u64,
    message: &mut Value,
    mode: RewriteMode,
    priority: Priority,
    deadline: Instant,
) {
    let Some(result) = message.get_mut("result") else {
        return;
    };

    let mut uris = Vec::new();
    match mode {
        RewriteMode::Locations => collect_jdt_location_uris(result, &mut uris, &mut HashSet::new()),
        RewriteMode::Strings => collect_jdt_uris(result, &mut uris, &mut HashSet::new()),
    }
    if uris.is_empty() {
        return;
    }

    let replacements = resolve_uris(inner, token, &uris, priority, deadline);
    match mode {
        RewriteMode::Locations => {
            replace_jdt_location_uris(result, &replacements);
        }
        RewriteMode::Strings => replace_in_strings(result, &replacements),
    }
}

fn resolve_uris(
    inner: &Inner,
    token: u64,
    uris: &[String],
    priority: Priority,
    deadline: Instant,
) -> HashMap<String, String> {
    let mut replacements = HashMap::new();
    let mut unresolved = Vec::new();

    for uri in uris {
        let path = cache_path(uri);
        if path.is_file() {
            replacements.insert(uri.clone(), path_to_file_uri(&path));
        } else {
            unresolved.push(uri.clone());
        }
    }
    if unresolved.is_empty() {
        return replacements;
    }

    let mut state = inner.state.lock().unwrap();
    let now = Instant::now();
    state.uris.retain(|_, entry| {
        !entry.waiters.is_empty()
            || matches!(entry.status, UriStatus::InFlight { .. })
            || matches!(entry.status, UriStatus::Failed(retry_after) if retry_after > now)
    });
    if state.canceled_jobs.contains(&token) {
        return replacements;
    }

    for uri in &unresolved {
        let mut enqueue = None;
        match state.uris.get_mut(uri) {
            Some(entry) => {
                entry.waiters.insert(token);
                match entry.status {
                    UriStatus::Ready(ref file_uri) => {
                        replacements.insert(uri.clone(), file_uri.clone());
                    }
                    UriStatus::Failed(retry_after) if retry_after <= Instant::now() => {
                        entry.status = UriStatus::Queued(priority);
                        enqueue = Some(priority);
                    }
                    UriStatus::Queued(Priority::Bulk) if priority == Priority::Interactive => {
                        entry.status = UriStatus::Queued(Priority::Interactive);
                        enqueue = Some(Priority::Interactive);
                    }
                    _ => {}
                }
            }
            None => {
                state.uris.insert(
                    uri.clone(),
                    UriEntry {
                        status: UriStatus::Queued(priority),
                        waiters: HashSet::from([token]),
                    },
                );
                enqueue = Some(priority);
            }
        }
        if let Some(priority) = enqueue {
            queue_uri(&mut state, uri.clone(), priority);
        }
    }
    inner.work_available.notify_all();

    loop {
        let mut waiting = false;
        for uri in &unresolved {
            match state.uris.get(uri).map(|entry| &entry.status) {
                Some(UriStatus::Ready(file_uri)) => {
                    replacements.insert(uri.clone(), file_uri.clone());
                }
                Some(UriStatus::Queued(_) | UriStatus::InFlight { .. }) => waiting = true,
                Some(UriStatus::Failed(_)) | None => {}
            }
        }

        if !waiting || state.canceled_jobs.contains(&token) || Instant::now() >= deadline {
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let (next_state, _) = inner.state_changed.wait_timeout(state, remaining).unwrap();
        state = next_state;
    }

    for uri in &unresolved {
        if let Some(entry) = state.uris.get_mut(uri) {
            entry.waiters.remove(&token);
        }
    }
    let orphaned = orphaned_request_ids(&state);
    drop(state);
    for request_id in orphaned {
        inner.fetcher.cancel(&request_id);
    }

    replacements
}

fn queue_uri(state: &mut State, uri: String, priority: Priority) {
    match priority {
        Priority::Interactive => state.interactive_uris.push_back(uri),
        Priority::Bulk => state.bulk_uris.push_back(uri),
    }
}

fn fetch_worker(inner: Arc<Inner>) {
    loop {
        let Some((uri, priority, request_id)) = take_uri_work(&inner) else {
            return;
        };

        let content = inner.fetcher.fetch(&uri, request_id.clone());
        let resolved = content.and_then(|content| write_cached_source(&uri, content.as_bytes()));

        let mut state = inner.state.lock().unwrap();
        if priority == Priority::Bulk {
            state.bulk_fetches = state.bulk_fetches.saturating_sub(1);
        }
        if let Some(entry) = state.uris.get_mut(&uri) {
            let is_current = matches!(
                &entry.status,
                UriStatus::InFlight {
                    request_id: current,
                    ..
                } if current == &request_id
            );
            if is_current {
                entry.status = match resolved {
                    Some(file_uri) => UriStatus::Ready(file_uri),
                    None => UriStatus::Failed(Instant::now() + NEGATIVE_CACHE_TTL),
                };
            }
        }
        let remove_ready = state.uris.get(&uri).is_some_and(|entry| {
            entry.waiters.is_empty() && matches!(entry.status, UriStatus::Ready(_))
        });
        if remove_ready {
            state.uris.remove(&uri);
        }
        inner.state_changed.notify_all();
        inner.work_available.notify_all();
    }
}

fn take_uri_work(inner: &Inner) -> Option<(String, Priority, Value)> {
    let mut state = inner.state.lock().unwrap();
    loop {
        if state.shutdown {
            return None;
        }

        let candidate = pop_valid_uri(&mut state, Priority::Interactive).or_else(|| {
            if state.bulk_fetches == 0 {
                pop_valid_uri(&mut state, Priority::Bulk)
            } else {
                None
            }
        });

        if let Some((uri, priority)) = candidate {
            let sequence = inner.request_counter.fetch_add(1, Ordering::Relaxed);
            let request_id =
                Value::String(format!("{}decompile-{sequence}", inner.owned_id_prefix));
            if priority == Priority::Bulk {
                state.bulk_fetches += 1;
            }
            if let Some(entry) = state.uris.get_mut(&uri) {
                entry.status = UriStatus::InFlight {
                    request_id: request_id.clone(),
                };
            }
            return Some((uri, priority, request_id));
        }

        state = inner.work_available.wait(state).unwrap();
    }
}

fn pop_valid_uri(state: &mut State, priority: Priority) -> Option<(String, Priority)> {
    let queue = match priority {
        Priority::Interactive => &mut state.interactive_uris,
        Priority::Bulk => &mut state.bulk_uris,
    };
    while let Some(uri) = queue.pop_front() {
        let valid = state.uris.get(&uri).is_some_and(|entry| {
            !entry.waiters.is_empty()
                && matches!(entry.status, UriStatus::Queued(current) if current == priority)
        });
        if valid {
            return Some((uri, priority));
        }
    }
    None
}

fn cache_dir() -> PathBuf {
    env::temp_dir().join(DECOMPILED_DIR)
}

fn cache_path(uri: &str) -> PathBuf {
    cache_path_in(&cache_dir(), uri)
}

fn cache_path_in(directory: &Path, uri: &str) -> PathBuf {
    let digest = hex::encode(Sha1::digest(uri.as_bytes()));
    let raw_name = uri
        .rsplit_once("%28")
        .and_then(|(_, rest)| rest.strip_suffix(".class"))
        .or_else(|| {
            uri.split('?')
                .next()
                .and_then(|path| path.rsplit('/').next())
                .and_then(|segment| {
                    segment
                        .strip_suffix(".java")
                        .or(segment.strip_suffix(".class"))
                })
        })
        .unwrap_or("Decompiled");
    let name: String = raw_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();

    directory.join(format!("{name}-{digest}.java"))
}

fn write_cached_source(uri: &str, content: &[u8]) -> Option<String> {
    if content.is_empty() {
        return None;
    }

    let directory = cache_dir();
    if let Err(error) = fs::create_dir_all(&directory) {
        lsp_error!(
            "[decompile] Failed to create {}: {error}",
            directory.display()
        );
        return None;
    }
    let target = cache_path_in(&directory, uri);
    if target.is_file() {
        return Some(path_to_file_uri(&target));
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);
    let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = target.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content)?;
        file.flush()?;
        fs::rename(&temporary, &target)
    })();

    match result {
        Ok(()) => Some(path_to_file_uri(&target)),
        Err(_error) if target.is_file() => {
            let _ = fs::remove_file(&temporary);
            Some(path_to_file_uri(&target))
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            lsp_error!(
                "[decompile] Failed to atomically write {}: {error}",
                target.display()
            );
            None
        }
    }
}

fn collect_jdt_location_uris(value: &Value, uris: &mut Vec<String>, seen: &mut HashSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_jdt_location_uris(value, uris, seen);
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "uri" | "targetUri") {
                    if let Value::String(uri) = value {
                        if uri.starts_with("jdt://") && seen.insert(uri.clone()) {
                            uris.push(uri.clone());
                        }
                    }
                    continue;
                }
                collect_jdt_location_uris(value, uris, seen);
            }
        }
        _ => {}
    }
}

fn replace_jdt_location_uris(value: &mut Value, replacements: &HashMap<String, String>) -> bool {
    match value {
        Value::Array(values) => {
            let mut rewritten = false;
            for value in values {
                rewritten |= replace_jdt_location_uris(value, replacements);
            }
            rewritten
        }
        Value::Object(object) => {
            let mut rewritten = false;
            for (key, value) in object {
                if matches!(key.as_str(), "uri" | "targetUri") {
                    if let Value::String(uri) = value {
                        if let Some(file_uri) = replacements.get(uri) {
                            *uri = file_uri.clone();
                            rewritten = true;
                        }
                    }
                    continue;
                }
                rewritten |= replace_jdt_location_uris(value, replacements);
            }
            rewritten
        }
        _ => false,
    }
}

fn jdt_uri_end(value: &str) -> usize {
    value
        .find(|character: char| {
            character.is_whitespace() || matches!(character, ')' | ']' | '"' | '>' | '`' | '\'')
        })
        .unwrap_or(value.len())
}

fn collect_jdt_uris(value: &Value, uris: &mut Vec<String>, seen: &mut HashSet<String>) {
    match value {
        Value::String(string) => {
            let mut rest = string.as_str();
            while let Some(position) = rest.find("jdt://") {
                let tail = &rest[position..];
                let end = jdt_uri_end(tail);
                let uri = tail[..end].to_string();
                if seen.insert(uri.clone()) {
                    uris.push(uri);
                }
                rest = &tail[end..];
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_jdt_uris(value, uris, seen);
            }
        }
        Value::Object(object) => {
            for value in object.values() {
                collect_jdt_uris(value, uris, seen);
            }
        }
        _ => {}
    }
}

fn replace_in_strings(value: &mut Value, replacements: &HashMap<String, String>) {
    match value {
        Value::String(string) => {
            for (from, to) in replacements {
                if string.contains(from) {
                    *string = string.replace(from, to);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                replace_in_strings(value, replacements);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                replace_in_strings(value, replacements);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeFetcher {
        calls: AtomicUsize,
        cancellations: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        delay: Duration,
        succeeds: bool,
    }

    impl FakeFetcher {
        fn new(delay: Duration) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                cancellations: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                delay,
                succeeds: true,
            }
        }

        fn failing(delay: Duration) -> Self {
            Self {
                succeeds: false,
                ..Self::new(delay)
            }
        }
    }

    impl ClassContentFetcher for FakeFetcher {
        fn fetch(&self, uri: &str, _request_id: Value) -> Option<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            thread::sleep(self.delay);
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.succeeds.then(|| format!("class {} {{}}", uri.len()))
        }

        fn cancel(&self, _request_id: &Value) {
            self.cancellations.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn unique_uri(name: &str) -> String {
        format!(
            "jdt://contents/test/{name}-{}.class",
            AtomicU64::new(1).fetch_add(1, Ordering::Relaxed) + std::process::id() as u64
        )
    }

    fn rewrite_for_test(
        coordinator: &DecompileCoordinator,
        token: u64,
        uris: &[String],
        priority: Priority,
    ) -> HashMap<String, String> {
        resolve_uris(
            &coordinator.inner,
            token,
            uris,
            priority,
            Instant::now() + Duration::from_secs(2),
        )
    }

    #[test]
    fn rewrites_nested_workspace_symbol_locations() {
        let mut result = json!([
            {
                "location": {
                    "uri": "jdt://contents/java.base/java.lang/String.class"
                }
            },
            {
                "location": {
                    "uri": "file:///workspace/ProjectString.java"
                }
            }
        ]);
        let replacements = HashMap::from([(
            "jdt://contents/java.base/java.lang/String.class".to_string(),
            "file:///tmp/String.java".to_string(),
        )]);

        assert!(replace_jdt_location_uris(&mut result, &replacements));
        assert_eq!(
            result[0]["location"]["uri"],
            json!("file:///tmp/String.java")
        );
        assert_eq!(
            result[1]["location"]["uri"],
            json!("file:///workspace/ProjectString.java")
        );
    }

    #[test]
    fn deduplicates_location_uris_before_resolution() {
        let uri = "jdt://contents/java.base/java.lang/String.class";
        let result = json!([{ "uri": uri }, { "targetUri": uri }]);
        let mut uris = Vec::new();

        collect_jdt_location_uris(&result, &mut uris, &mut HashSet::new());

        assert_eq!(uris, vec![uri]);
    }

    #[test]
    fn ignores_jdt_uris_outside_location_fields() {
        let result = json!({
            "documentation": "jdt://contents/java.base/java.lang/String.class",
            "data": {
                "sourceUri": "jdt://contents/java.base/java.lang/Object.class"
            }
        });
        let mut uris = Vec::new();

        collect_jdt_location_uris(&result, &mut uris, &mut HashSet::new());

        assert!(uris.is_empty());
    }

    #[test]
    fn concurrent_waiters_share_one_fetch() {
        let fetcher = Arc::new(FakeFetcher::new(Duration::from_millis(20)));
        let coordinator =
            DecompileCoordinator::with_fetcher(fetcher.clone(), "test-".to_string(), 2, 0);
        let uri = unique_uri("single-flight");
        let _ = fs::remove_file(cache_path(&uri));

        let first = {
            let coordinator = coordinator.clone();
            let uri = uri.clone();
            thread::spawn(move || rewrite_for_test(&coordinator, 1, &[uri], Priority::Interactive))
        };
        let second = {
            let coordinator = coordinator.clone();
            let uri = uri.clone();
            thread::spawn(move || rewrite_for_test(&coordinator, 2, &[uri], Priority::Interactive))
        };

        assert_eq!(first.join().unwrap().len(), 1);
        assert_eq!(second.join().unwrap().len(), 1);
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
        let _ = fs::remove_file(cache_path(&uri));
        coordinator.shutdown();
    }

    #[test]
    fn interactive_fetches_use_bounded_parallelism() {
        let fetcher = Arc::new(FakeFetcher::new(Duration::from_millis(30)));
        let coordinator =
            DecompileCoordinator::with_fetcher(fetcher.clone(), "test-".to_string(), 2, 0);
        let uris: Vec<_> = (0..4)
            .map(|index| unique_uri(&format!("parallel-{index}")))
            .collect();
        for uri in &uris {
            let _ = fs::remove_file(cache_path(uri));
        }

        assert_eq!(
            rewrite_for_test(&coordinator, 1, &uris, Priority::Interactive).len(),
            uris.len()
        );
        assert_eq!(fetcher.max_active.load(Ordering::SeqCst), 2);
        for uri in &uris {
            let _ = fs::remove_file(cache_path(uri));
        }
        coordinator.shutdown();
    }

    #[test]
    fn deadline_returns_completed_replacements_only() {
        let fetcher = Arc::new(FakeFetcher::new(Duration::from_millis(50)));
        let coordinator = DecompileCoordinator::with_fetcher(fetcher, "test-".to_string(), 1, 0);
        let uris = vec![unique_uri("deadline-a"), unique_uri("deadline-b")];
        for uri in &uris {
            let _ = fs::remove_file(cache_path(uri));
        }

        let replacements = resolve_uris(
            &coordinator.inner,
            1,
            &uris,
            Priority::Interactive,
            Instant::now() + Duration::from_millis(70),
        );

        assert_eq!(replacements.len(), 1);
        for uri in &uris {
            let _ = fs::remove_file(cache_path(uri));
        }
        coordinator.shutdown();
    }

    #[test]
    fn canceling_one_waiter_keeps_shared_fetch_alive() {
        let fetcher = Arc::new(FakeFetcher::new(Duration::from_millis(50)));
        let coordinator =
            DecompileCoordinator::with_fetcher(fetcher.clone(), "test-".to_string(), 1, 0);
        let uri = unique_uri("shared-cancel");
        let _ = fs::remove_file(cache_path(&uri));

        let first = {
            let coordinator = coordinator.clone();
            let uri = uri.clone();
            thread::spawn(move || rewrite_for_test(&coordinator, 1, &[uri], Priority::Interactive))
        };
        let second = {
            let coordinator = coordinator.clone();
            let uri = uri.clone();
            thread::spawn(move || rewrite_for_test(&coordinator, 2, &[uri], Priority::Interactive))
        };
        while {
            let state = coordinator.inner.state.lock().unwrap();
            state
                .uris
                .get(&uri)
                .is_none_or(|entry| entry.waiters.len() < 2)
        } {
            thread::yield_now();
        }
        coordinator.cancel(1);

        assert!(first.join().unwrap().is_empty());
        assert_eq!(second.join().unwrap().len(), 1);
        assert_eq!(fetcher.cancellations.load(Ordering::Relaxed), 0);
        let _ = fs::remove_file(cache_path(&uri));
        coordinator.shutdown();
    }

    #[test]
    fn negative_cache_prevents_immediate_retry_storms() {
        let fetcher = Arc::new(FakeFetcher::failing(Duration::ZERO));
        let coordinator =
            DecompileCoordinator::with_fetcher(fetcher.clone(), "test-".to_string(), 1, 0);
        let uri = unique_uri("negative-cache");
        let _ = fs::remove_file(cache_path(&uri));

        assert!(rewrite_for_test(
            &coordinator,
            1,
            std::slice::from_ref(&uri),
            Priority::Interactive
        )
        .is_empty());
        assert!(rewrite_for_test(
            &coordinator,
            2,
            std::slice::from_ref(&uri),
            Priority::Interactive
        )
        .is_empty());
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
        coordinator.shutdown();
    }

    #[test]
    fn competing_cache_writes_never_expose_partial_content() {
        let uri = unique_uri("atomic-cache");
        let path = cache_path(&uri);
        let _ = fs::remove_file(&path);
        let first_content = vec![b'a'; 32 * 1024];
        let second_content = vec![b'b'; 32 * 1024];

        let first = {
            let uri = uri.clone();
            let content = first_content.clone();
            thread::spawn(move || write_cached_source(&uri, &content))
        };
        let second = {
            let uri = uri.clone();
            let content = second_content.clone();
            thread::spawn(move || write_cached_source(&uri, &content))
        };
        assert!(first.join().unwrap().is_some());
        assert!(second.join().unwrap().is_some());

        let cached = fs::read(&path).unwrap();
        assert!(cached == first_content || cached == second_content);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn cache_names_use_stable_digest_and_sanitized_name() {
        let directory = Path::new("/tmp/cache");
        let first = cache_path_in(directory, "jdt://contents/a/../../Bad Name.class");
        let second = cache_path_in(directory, "jdt://contents/a/../../Bad Name.class");

        assert_eq!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Bad_Name-"));
        assert_eq!(first.extension().unwrap(), "java");
    }

    #[test]
    fn empty_sources_are_not_cached() {
        let uri = unique_uri("empty");
        let path = cache_path(&uri);
        let _ = fs::remove_file(&path);

        assert_eq!(write_cached_source(&uri, b""), None);
        assert!(!path.exists());
    }
}
