mod completions;
mod decompile;
mod http;
mod log;
mod output;
mod pending;

use completions::{process_completions, sanitize_resolved_completion};
use decompile::{DecompileCoordinator, Priority, RewriteJob, RewriteMode, SharedWriter};
use http::handle_http;
use output::Output;
use pending::PendingResponses;
use proxy_common::{
    contains_subslice, parse_lsp_content, raw_has_id, spawn_parent_monitor, LspReader,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufReader, Write},
    net::TcpListener,
    path::Path,
    process::{self, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RewriteKind {
    Locations,
    Documentation,
    Completion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedRequest {
    token: u64,
    method: String,
    rewrite: RewriteKind,
    original_params: Option<Value>,
}

impl TrackedRequest {
    fn new(token: u64, method: &str, rewrite: RewriteKind, original_params: Option<Value>) -> Self {
        Self {
            token,
            method: method.to_string(),
            rewrite,
            original_params,
        }
    }
}

fn main() {
    let output = Output::start();
    log::init(output.clone());

    let args: Vec<String> = env::args().skip(1).collect();

    if args.len() < 2 {
        eprintln!("Usage: java-lsp-proxy <workdir> <bin> [args...]");
        lsp_error!("Usage: java-lsp-proxy <workdir> <bin> [args...]");
        process::exit(1);
    }

    let workdir = &args[0];
    let bin = &args[1];
    let child_args = &args[2..];

    lsp_info!("java-lsp-proxy starting: bin={bin}, workdir={workdir}");

    let proxy_id = hex_encode(
        env::current_dir()
            .unwrap()
            .to_string_lossy()
            .trim_end_matches('/'),
    );

    // Spawn JDTLS (use shell on Windows for .bat files)
    let mut cmd = Command::new(bin);
    cmd.args(child_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    #[cfg(windows)]
    if bin.ends_with(".bat") || bin.ends_with(".cmd") {
        cmd = Command::new("cmd");
        cmd.arg("/C")
            .arg(bin)
            .args(child_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
    }

    let mut child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("Failed to spawn {bin}: {e}");
        lsp_error!("Failed to spawn {bin}: {e}");
        process::exit(1);
    });

    lsp_info!("JDTLS process spawned (pid={})", child.id());

    let child_stdin: SharedWriter = Arc::new(Mutex::new(Box::new(child.stdin.take().unwrap())));
    let child_stdout = child.stdout.take().unwrap();
    let alive = Arc::new(AtomicBool::new(true));

    let owned_id_prefix = format!("{proxy_id}-proxy-");
    let pending = Arc::new(PendingResponses::new(owned_id_prefix.clone()));
    let decompile = DecompileCoordinator::new(
        Arc::clone(&child_stdin),
        Arc::clone(&pending),
        owned_id_prefix.clone(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let port_file = Path::new(workdir).join("proxy").join(&proxy_id);
    fs::create_dir_all(port_file.parent().unwrap()).unwrap();
    fs::write(&port_file, port.to_string()).unwrap();

    lsp_info!("HTTP server listening on 127.0.0.1:{port}");

    let id_counter = Arc::new(AtomicU64::new(1));
    let job_counter = Arc::new(AtomicU64::new(1));

    // Track requests whose responses may contain jdt:// URIs so they can be
    // intercepted and rewritten.
    let tracked_ids: Arc<Mutex<HashMap<Value, TrackedRequest>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let active_rewrites: Arc<Mutex<HashMap<Value, u64>>> = Arc::new(Mutex::new(HashMap::new()));

    // --- Thread 1: Zed stdin -> JDTLS stdin (track definition requests) ---
    let stdin_writer = Arc::clone(&child_stdin);
    let alive_stdin = Arc::clone(&alive);
    let tracked_in = Arc::clone(&tracked_ids);
    let active_in = Arc::clone(&active_rewrites);
    let jobs_in = Arc::clone(&job_counter);
    let decompile_in = decompile.clone();
    let latest_workspace_job = Arc::new(Mutex::new(None::<u64>));
    let latest_workspace_in = Arc::clone(&latest_workspace_job);
    thread::spawn(move || {
        let stdin = io::stdin().lock();
        let mut reader = LspReader::new(BufReader::new(stdin));
        while alive_stdin.load(Ordering::Relaxed) {
            match reader.read_message() {
                Ok(Some(raw)) => {
                    let should_parse =
                        raw_has_id(&raw) || contains_subslice(&raw, b"$/cancelRequest");
                    if should_parse {
                        let Some(msg) = parse_lsp_content(&raw) else {
                            let mut writer = stdin_writer.lock().unwrap();
                            if writer.write_all(&raw).is_err() || writer.flush().is_err() {
                                break;
                            }
                            continue;
                        };
                        if msg.get("method").and_then(Value::as_str) == Some("$/cancelRequest") {
                            if let Some(id) = msg.pointer("/params/id") {
                                let request = tracked_in.lock().unwrap().get(id).cloned();
                                let active_token = active_in.lock().unwrap().remove(id);
                                let token = request
                                    .as_ref()
                                    .map(|request| request.token)
                                    .or(active_token);
                                if let Some(token) = token {
                                    clear_latest_workspace(&latest_workspace_in, token);
                                    decompile_in.cancel(token);
                                }
                            }
                        } else if raw_has_id(&raw) {
                            let token = jobs_in.fetch_add(1, Ordering::Relaxed);
                            if let Some((id, request)) = tracked_request_for(&msg, token) {
                                if request.method == "workspace/symbol" {
                                    let previous =
                                        latest_workspace_in.lock().unwrap().replace(token);
                                    if let Some(previous) = previous {
                                        remove_active_token(&active_in, previous);
                                        decompile_in.cancel(previous);
                                    }
                                }
                                let previous = tracked_in.lock().unwrap().insert(id, request);
                                if let Some(previous) = previous {
                                    remove_active_token(&active_in, previous.token);
                                    decompile_in.cancel(previous.token);
                                }
                            }
                        }
                    }
                    let mut w = stdin_writer.lock().unwrap();
                    if w.write_all(&raw).is_err() || w.flush().is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        alive_stdin.store(false, Ordering::Relaxed);
    });

    // --- Thread 2: JDTLS stdout -> rewrite jdt:// URIs, modify completions -> Zed stdout / resolve pending ---
    let pending_out = Arc::clone(&pending);
    let alive_out = Arc::clone(&alive);
    let tracked_out = Arc::clone(&tracked_ids);
    let active_out = Arc::clone(&active_rewrites);
    let decompile_out = decompile.clone();
    let output_router = output.clone();
    let latest_workspace_out = Arc::clone(&latest_workspace_job);
    thread::spawn(move || {
        let mut reader = LspReader::new(BufReader::new(child_stdout));
        while alive_out.load(Ordering::Relaxed) {
            match reader.read_message() {
                Ok(Some(raw)) => {
                    // Fast path: notifications (no `id`) can't be responses we
                    // need to intercept. Forward the raw bytes without parsing.
                    if !raw_has_id(&raw) {
                        output_router.send_raw(raw);
                        continue;
                    }

                    let Some(mut msg) = parse_lsp_content(&raw) else {
                        output_router.send_raw(raw);
                        continue;
                    };

                    // Route responses to pending HTTP requests
                    if pending_out.route(&msg) {
                        continue;
                    }

                    // Rewrite jdt:// URIs in location or documentation responses.
                    // The bounded coordinator keeps this router free to deliver
                    // java/classFileContents responses through `pending`.
                    if msg.get("method").is_none() {
                        let Some(id) = msg.get("id").cloned() else {
                            output_router.send_raw(raw);
                            continue;
                        };
                        let request = tracked_out.lock().unwrap().get(&id).cloned();
                        if let Some(request) = request {
                            let canceled = decompile_out.is_canceled(request.token);
                            if canceled {
                                tracked_out.lock().unwrap().remove(&id);
                                decompile_out.consume_cancellation(request.token);
                                clear_latest_workspace(&latest_workspace_out, request.token);
                                continue;
                            }
                            if let Some(fallback) = completion_resolve_fallback(&msg, &request) {
                                tracked_out.lock().unwrap().remove(&id);
                                if should_log_completion_fallback() {
                                    lsp_warn!(
                                        "JDTLS completion resolution failed with -32603; \
                                         using the unresolved item, so documentation, imports, \
                                         commands, or additional edits may be missing"
                                    );
                                }
                                output_router.send_value(&fallback);
                                continue;
                            }
                            if msg.get("error").is_some() {
                                tracked_out.lock().unwrap().remove(&id);
                                clear_latest_workspace(&latest_workspace_out, request.token);
                                output_router.send_raw(raw);
                                continue;
                            }
                            if request.rewrite == RewriteKind::Completion {
                                tracked_out.lock().unwrap().remove(&id);
                                process_completions(&mut msg);
                                output_router.send_value(&msg);
                                continue;
                            }

                            let output = output_router.clone();
                            let sanitize_completion = request.method == "completionItem/resolve";
                            let mode = match request.rewrite {
                                RewriteKind::Locations => RewriteMode::Locations,
                                RewriteKind::Documentation => RewriteMode::Strings,
                                RewriteKind::Completion => unreachable!(),
                            };
                            let priority = if request.method == "workspace/symbol" {
                                Priority::Bulk
                            } else {
                                Priority::Interactive
                            };
                            let deadline =
                                std::time::Instant::now() + rewrite_timeout(&request.method);
                            let workspace_job = (request.method == "workspace/symbol")
                                .then(|| (Arc::clone(&latest_workspace_out), request.token));
                            active_out.lock().unwrap().insert(id.clone(), request.token);
                            tracked_out.lock().unwrap().remove(&id);
                            if decompile_out.is_canceled(request.token) {
                                active_out.lock().unwrap().remove(&id);
                                decompile_out.consume_cancellation(request.token);
                                clear_latest_workspace(&latest_workspace_out, request.token);
                                continue;
                            }
                            let active = Arc::clone(&active_out);
                            let active_id = id;
                            let completion_coordinator = decompile_out.clone();
                            let job = RewriteJob {
                                token: request.token,
                                message: msg,
                                mode,
                                priority,
                                deadline,
                                complete: Box::new(move |mut message| {
                                    if sanitize_completion {
                                        sanitize_resolved_completion(&mut message);
                                    }
                                    let owns_response = active.lock().unwrap().remove(&active_id)
                                        == Some(request.token);
                                    completion_coordinator.consume_cancellation(request.token);
                                    if owns_response {
                                        output.send_value(&message);
                                    }
                                    if let Some((latest, token)) = workspace_job {
                                        clear_latest_workspace(&latest, token);
                                    }
                                }),
                            };
                            if let Err(job) = decompile_out.submit(job) {
                                let RewriteJob {
                                    message, complete, ..
                                } = job;
                                complete(message);
                            }
                            continue;
                        }
                    }

                    // Passthrough
                    output_router.send_raw(raw);
                }
                Ok(None) | Err(_) => break,
            }
        }
        alive_out.store(false, Ordering::Relaxed);
    });

    // --- Thread 3: HTTP server for extension requests ---
    let http_writer = Arc::clone(&child_stdin);
    let http_pending = Arc::clone(&pending);
    let http_alive = Arc::clone(&alive);
    let http_id_counter = Arc::clone(&id_counter);
    let http_owned_id_prefix = owned_id_prefix.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            if !http_alive.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let writer = Arc::clone(&http_writer);
            let pend = Arc::clone(&http_pending);
            let counter = Arc::clone(&http_id_counter);
            let owned_id_prefix = http_owned_id_prefix.clone();

            thread::spawn(move || {
                handle_http(stream, writer, pend, counter, &owned_id_prefix);
            });
        }
    });

    // --- Thread 4: Parent process monitor ---
    spawn_parent_monitor(Arc::clone(&alive), child.id());

    // Poll so broken editor/JDTLS transport can terminate the child and release
    // all pending proxy jobs instead of waiting indefinitely in `Child::wait`.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if alive.load(Ordering::Relaxed) && !output.failed() => {
                thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                break child.wait();
            }
            Err(error) => break Err(error),
        }
    };
    lsp_info!("JDTLS process exited: {status:?}");
    alive.store(false, Ordering::Relaxed);
    decompile.shutdown();
    pending.clear();
    let _ = fs::remove_file(&port_file);
}

// --- Utilities ---

fn hex_encode(s: &str) -> String {
    s.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn clear_latest_workspace(latest: &Mutex<Option<u64>>, token: u64) {
    let mut latest = latest.lock().unwrap();
    if *latest == Some(token) {
        *latest = None;
    }
}

fn remove_active_token(active: &Mutex<HashMap<Value, u64>>, token: u64) {
    active
        .lock()
        .unwrap()
        .retain(|_, active_token| *active_token != token);
}

fn should_log_completion_fallback() -> bool {
    static LAST_WARNING: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    let now = std::time::Instant::now();
    let mut last = LAST_WARNING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap();
    if last
        .is_some_and(|previous| now.duration_since(previous) < std::time::Duration::from_secs(60))
    {
        return false;
    }
    *last = Some(now);
    true
}

fn tracked_request_for(msg: &Value, token: u64) -> Option<(Value, TrackedRequest)> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id")?.clone();
    let rewrite = match method {
        "textDocument/completion" => RewriteKind::Completion,
        "textDocument/definition"
        | "textDocument/declaration"
        | "textDocument/typeDefinition"
        | "textDocument/implementation"
        | "textDocument/references"
        | "textDocument/prepareCallHierarchy"
        | "callHierarchy/incomingCalls"
        | "callHierarchy/outgoingCalls"
        | "textDocument/prepareTypeHierarchy"
        | "typeHierarchy/supertypes"
        | "typeHierarchy/subtypes"
        | "workspace/symbol" => RewriteKind::Locations,
        "textDocument/hover" | "textDocument/signatureHelp" | "completionItem/resolve" => {
            RewriteKind::Documentation
        }
        _ => return None,
    };

    let original_params = if method == "completionItem/resolve" {
        msg.get("params")
            .filter(|params| params.is_object())
            .cloned()
    } else {
        None
    };

    Some((
        id,
        TrackedRequest::new(token, method, rewrite, original_params),
    ))
}

fn completion_resolve_fallback(message: &Value, request: &TrackedRequest) -> Option<Value> {
    if request.method != "completionItem/resolve"
        || message.pointer("/error/code").and_then(Value::as_i64) != Some(-32603)
    {
        return None;
    }
    let item = request.original_params.as_ref()?;
    Some(json!({
        "jsonrpc": "2.0",
        "id": message.get("id")?.clone(),
        "result": item,
    }))
}

fn rewrite_timeout(method: &str) -> std::time::Duration {
    match method {
        "textDocument/hover" | "textDocument/signatureHelp" | "completionItem/resolve" => {
            std::time::Duration::from_millis(500)
        }
        "workspace/symbol" => std::time::Duration::from_secs(1),
        _ => std::time::Duration::from_secs(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracks_location_response_methods() {
        let methods = [
            "textDocument/definition",
            "textDocument/declaration",
            "textDocument/typeDefinition",
            "textDocument/implementation",
            "textDocument/references",
            "textDocument/prepareCallHierarchy",
            "callHierarchy/incomingCalls",
            "callHierarchy/outgoingCalls",
            "textDocument/prepareTypeHierarchy",
            "typeHierarchy/supertypes",
            "typeHierarchy/subtypes",
            "workspace/symbol",
        ];

        for method in methods {
            let request = json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": method,
                "params": {}
            });

            assert_eq!(
                tracked_request_for(&request, 42),
                Some((
                    json!(7),
                    TrackedRequest::new(42, method, RewriteKind::Locations, None)
                )),
                "{method} should rewrite location URIs"
            );
        }
    }

    #[test]
    fn tracks_completion_resolve_documentation() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "resolve-1",
            "method": "completionItem/resolve",
            "params": { "label": "String" }
        });

        assert_eq!(
            tracked_request_for(&request, 43),
            Some((
                json!("resolve-1"),
                TrackedRequest::new(
                    43,
                    "completionItem/resolve",
                    RewriteKind::Documentation,
                    Some(json!({ "label": "String" }))
                )
            ))
        );
    }

    #[test]
    fn ignores_untracked_requests_and_notifications() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "textDocument/rename",
            "params": {}
        });
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {}
        });

        assert_eq!(tracked_request_for(&request, 1), None);
        assert_eq!(tracked_request_for(&notification, 2), None);
    }

    #[test]
    fn tracks_completion_responses_by_method() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "textDocument/completion",
            "params": {}
        });

        assert_eq!(
            tracked_request_for(&request, 44),
            Some((
                json!(9),
                TrackedRequest::new(44, "textDocument/completion", RewriteKind::Completion, None)
            ))
        );
    }

    #[test]
    fn falls_back_only_for_internal_completion_resolve_errors() {
        let request = TrackedRequest::new(
            1,
            "completionItem/resolve",
            RewriteKind::Documentation,
            Some(json!({ "label": "value", "kind": 6 })),
        );
        let internal = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "error": { "code": -32603, "message": "Invalid completion proposal" }
        });
        let canceled = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "error": { "code": -32800, "message": "Request cancelled" }
        });

        assert_eq!(
            completion_resolve_fallback(&internal, &request),
            Some(json!({
                "jsonrpc": "2.0",
                "id": 5,
                "result": { "label": "value", "kind": 6 }
            }))
        );
        assert_eq!(completion_resolve_fallback(&canceled, &request), None);
    }

    #[test]
    fn malformed_completion_resolve_params_do_not_fallback() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "completionItem/resolve",
            "params": null
        });
        let (_, tracked) = tracked_request_for(&request, 1).unwrap();
        let error = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "error": { "code": -32603, "message": "Internal error" }
        });

        assert_eq!(completion_resolve_fallback(&error, &tracked), None);
    }
}
