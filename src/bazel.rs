use std::{fs, path::PathBuf};

use zed_extension_api::{
    self as zed, DownloadedFileType, LanguageServerId, LanguageServerInstallationStatus,
    download_file, set_language_server_installation_status,
};

use crate::util::{create_path_if_not_exists, get_curr_dir};

const BAZEL_INSTALL_PATH: &str = "bazel";
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
    bundles: Option<Vec<PathBuf>>,
}

impl Bazel {
    pub fn new() -> Self {
        Self { bundles: None }
    }

    pub fn loaded(&self) -> bool {
        self.bundles.is_some()
    }

    pub fn get_or_download(
        &mut self,
        language_server_id: &LanguageServerId,
    ) -> zed::Result<Vec<PathBuf>> {
        if let Some(bundles) = &self.bundles {
            return Ok(bundles.clone());
        }

        let plugins_dir = PathBuf::from(BAZEL_INSTALL_PATH).join("plugins");

        // Check if already downloaded
        if plugins_dir.exists() {
            if let Some(bundles) = self.find_bundles(&plugins_dir) {
                self.bundles = Some(bundles.clone());
                return Ok(bundles);
            }
        }

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
        .map_err(|e| format!("Failed to download Bazel bundles: {e}"))?;

        let bundles = self
            .find_bundles(&plugins_dir)
            .ok_or("Failed to find Bazel bundles after extraction")?;
        self.bundles = Some(bundles.clone());
        Ok(bundles)
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
        initialization_options: Option<zed_extension_api::serde_json::Value>,
    ) -> zed::Result<zed_extension_api::serde_json::Value> {
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
