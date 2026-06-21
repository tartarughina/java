use std::{fs, path::PathBuf};

use zed_extension_api::{
    self as zed, DownloadedFileType, LanguageServerId, LanguageServerInstallationStatus, Worktree,
    download_file, serde_json::Value, set_language_server_installation_status,
};

use crate::{
    config::get_bazel_path,
    downloadable::Downloadable,
    util::{
        create_path_if_not_exists, get_curr_dir, mark_checked_once, should_use_local_or_download,
    },
};

const BAZEL_INSTALL_PATH: &str = "bazel";
const PLUGINS_DIR: &str = "plugins";
// The p2 repository is published as "latest" only, with no resolvable version
// number. We still record a checked-once marker so `check_updates: once` works.
const BAZEL_VERSION: &str = "latest";
const P2_REPOSITORY_URL: &str =
    "https://opensource.salesforce.com/bazel-eclipse/latest/p2-repository.zip";

const REQUIRED_BUNDLES: &[&str] = &[
    "com.salesforce.bazel.eclipse.jdtls",
    "com.salesforce.bazel.eclipse.core",
    "com.salesforce.bazel.sdk",
    "com.salesforce.bazel.importedsource",
    "org.fusesource.jansi",
    "com.google.protobuf",
    "com.github.ben-manes.caffeine",
    "org.jsr-305",
    "org.eclipse.equinox.event",
];

pub struct Bazel {
    /// The resolved bundle jar paths, injected into JDTLS `initializationOptions.bundles`.
    bundles: Option<Vec<PathBuf>>,
}

impl Bazel {
    pub fn new() -> Self {
        Self { bundles: None }
    }

    /// Locate the required bundle jars beneath `dir`.
    ///
    /// `dir` may be the install root (containing a `plugins/` subdirectory) or
    /// the `plugins/` directory itself, so a user-configured `bazel_path` can
    /// point at either.
    fn resolve_bundles(&self, dir: &PathBuf) -> Option<Vec<PathBuf>> {
        let plugins_dir = dir.join(PLUGINS_DIR);
        if plugins_dir.is_dir()
            && let Some(bundles) = self.find_bundles(&plugins_dir)
        {
            return Some(bundles);
        }
        self.find_bundles(dir)
    }

    fn find_bundles(&self, plugins_dir: &PathBuf) -> Option<Vec<PathBuf>> {
        let entries: Vec<_> = fs::read_dir(plugins_dir)
            .ok()?
            .filter_map(|e| e.ok())
            .collect();

        let bundles: Vec<PathBuf> = REQUIRED_BUNDLES
            .iter()
            .filter_map(|prefix| {
                entries.iter().find_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    // Match prefix, exclude .source_ JARs, must end with .jar
                    if name.starts_with(prefix)
                        && !name.contains(".source_")
                        && name.ends_with(".jar")
                    {
                        Some(entry.path())
                    } else {
                        None
                    }
                })
            })
            .collect();
        if bundles.len() == REQUIRED_BUNDLES.len() {
            Some(bundles)
        } else {
            None
        }
    }

    pub fn inject_bundles_into_options(
        &self,
        initialization_options: Option<Value>,
    ) -> zed::Result<Value> {
        use zed_extension_api::serde_json::{Value, json};

        let current_dir = get_curr_dir()?;
        let bundles = self.bundles.as_ref().ok_or("Bazel bundles not loaded")?;

        let bundle_paths: Vec<Value> = bundles
            .iter()
            .map(|p| Value::String(current_dir.join(p).to_string_lossy().to_string()))
            .collect();

        match initialization_options {
            None => Ok(json!({ "bundles": bundle_paths })),
            Some(mut options) => {
                let existing = options.get_mut("bundles").and_then(|v| v.as_array_mut());

                if let Some(arr) = existing {
                    for path in bundle_paths {
                        if !arr.contains(&path) {
                            arr.push(path);
                        }
                    }
                } else {
                    options["bundles"] = Value::Array(bundle_paths);
                }

                Ok(options)
            }
        }
    }
}

impl Downloadable for Bazel {
    const INSTALL_PATH: &'static str = BAZEL_INSTALL_PATH;

    fn find_local(&self) -> Option<PathBuf> {
        let install_root = PathBuf::from(BAZEL_INSTALL_PATH);
        self.resolve_bundles(&install_root)
            .map(|_| install_root.join(PLUGINS_DIR))
    }

    fn loaded(&self) -> bool {
        self.bundles.is_some()
    }

    fn fetch_latest_version(&self) -> zed::Result<String> {
        Ok(BAZEL_VERSION.to_string())
    }

    fn download(
        &mut self,
        version: &str,
        language_server_id: &LanguageServerId,
    ) -> zed::Result<PathBuf> {
        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Downloading,
        );

        create_path_if_not_exists(BAZEL_INSTALL_PATH)?;

        download_file(
            P2_REPOSITORY_URL,
            BAZEL_INSTALL_PATH,
            DownloadedFileType::Zip,
        )
        .map_err(|e| format!("Failed to download Bazel bundles from {P2_REPOSITORY_URL}: {e}"))?;

        let install_root = PathBuf::from(BAZEL_INSTALL_PATH);
        let bundles = self
            .resolve_bundles(&install_root)
            .ok_or("Failed to find Bazel bundles after extraction")?;
        self.bundles = Some(bundles);

        let _ = mark_checked_once(BAZEL_INSTALL_PATH, version);

        Ok(install_root.join(PLUGINS_DIR))
    }

    fn get_or_download(
        &mut self,
        language_server_id: &LanguageServerId,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> zed::Result<PathBuf> {
        if self.bundles.is_some() {
            return Ok(PathBuf::from(BAZEL_INSTALL_PATH).join(PLUGINS_DIR));
        }

        if let Some(user_path) = self.user_configured_path(configuration, worktree) {
            let dir = PathBuf::from(&user_path);
            let bundles = self.resolve_bundles(&dir).ok_or_else(|| {
                format!("No Bazel bundles found at configured bazel_path: {user_path}")
            })?;
            self.bundles = Some(bundles);
            return Ok(dir);
        }

        if let Some(dir) =
            should_use_local_or_download(configuration, self.find_local(), Self::INSTALL_PATH)?
        {
            let bundles = self
                .resolve_bundles(&PathBuf::from(BAZEL_INSTALL_PATH))
                .ok_or("Bazel bundles missing from local installation")?;
            self.bundles = Some(bundles);
            return Ok(dir);
        }

        let version = self.fetch_latest_version()?;
        self.download(&version, language_server_id)
    }

    fn user_configured_path(
        &self,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> Option<String> {
        get_bazel_path(configuration, worktree)
    }
}
