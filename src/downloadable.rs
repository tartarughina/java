use std::path::PathBuf;

use zed_extension_api::{
    self as zed, LanguageServerId, LanguageServerInstallationStatus, Worktree, serde_json::Value,
    set_language_server_installation_status,
};

use crate::{
    config::{CheckUpdates, get_check_updates},
    util::{
        fresh_cached_version, record_successful_update_check, should_use_local_or_download,
        update_check_path,
    },
};

pub trait Downloadable {
    const INSTALL_PATH: &'static str;

    fn find_local(&self) -> Option<PathBuf>;

    fn loaded(&self) -> bool;

    fn fetch_latest_version(&self, worktree: &Worktree) -> zed::Result<String>;

    fn download(
        &mut self,
        version: &str,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<PathBuf>;

    fn update_check_path(&self) -> PathBuf {
        update_check_path(Self::INSTALL_PATH)
    }

    fn version_for_download(
        &self,
        language_server_id: &LanguageServerId,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> zed::Result<(String, bool)> {
        if get_check_updates(configuration) == CheckUpdates::Always
            && let Some(version) = fresh_cached_version(&self.update_check_path())
        {
            return Ok((version, false));
        }

        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let version = self.fetch_latest_version(worktree);
        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );
        version.map(|version| (version, true))
    }

    fn record_update_check(&self, version: &str) {
        if let Err(err) = record_successful_update_check(&self.update_check_path(), version) {
            println!(
                "Failed to record update check for {}: {err}",
                Self::INSTALL_PATH
            );
        }
    }

    fn get_or_download(
        &mut self,
        language_server_id: &LanguageServerId,
        configuration: &Option<Value>,
        worktree: &Worktree,
    ) -> zed::Result<PathBuf> {
        if let Some(path) = self.user_configured_path(configuration, worktree) {
            return Ok(PathBuf::from(path));
        }

        if let Some(path) = should_use_local_or_download(
            configuration,
            self.find_local(),
            Self::INSTALL_PATH,
            &self.update_check_path(),
        )? {
            return Ok(path);
        }

        let downloaded = self
            .version_for_download(language_server_id, configuration, worktree)
            .and_then(|(version, was_fetched)| {
                self.download(&version, language_server_id, worktree)
                    .map(|path| (path, version, was_fetched))
            });

        match downloaded {
            Ok((path, version, was_fetched)) => {
                if was_fetched {
                    self.record_update_check(&version);
                }
                Ok(path)
            }
            // The version check or download failed (e.g. GitHub API rate
            // limiting) — an existing local installation is better than none.
            Err(err) => match self.find_local() {
                Some(path) => {
                    println!(
                        "Failed to update {}, falling back to local installation: {err}",
                        Self::INSTALL_PATH
                    );
                    Ok(path)
                }
                None => Err(err),
            },
        }
    }

    fn user_configured_path(
        &self,
        _configuration: &Option<Value>,
        _worktree: &Worktree,
    ) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod fallback_tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    #[test]
    fn test_check_updates_always_allows_download() {
        let result =
            should_use_local_or_download(&None, None, "jdtls", &update_check_path("jdtls"))
                .unwrap();
        assert!(result.is_none(), "Always mode should allow download");
    }

    #[test]
    fn test_check_updates_always_with_local_still_downloads() {
        let local = PathBuf::from("/mock/jdtls/1.44.0");
        let result =
            should_use_local_or_download(&None, Some(local), "jdtls", &update_check_path("jdtls"))
                .unwrap();
        assert!(result.is_none(), "Always mode downloads even with local");
    }

    #[test]
    fn test_check_updates_never_with_local_uses_it() {
        let config = Some(json!({"check_updates": "never"}));
        let local = PathBuf::from("/mock/jdtls/1.44.0");
        let result = should_use_local_or_download(
            &config,
            Some(local.clone()),
            "jdtls",
            &update_check_path("jdtls"),
        )
        .unwrap();
        assert_eq!(result, Some(local));
    }

    #[test]
    fn test_check_updates_never_without_local_is_error() {
        let config = Some(json!({"check_updates": "never"}));
        let result =
            should_use_local_or_download(&config, None, "jdtls", &update_check_path("jdtls"));
        assert!(result.is_err());
    }

    #[test]
    fn test_check_updates_once_with_local_uses_it() {
        let config = Some(json!({"check_updates": "once"}));
        let local = PathBuf::from("/mock/jdtls/1.44.0");
        let result = should_use_local_or_download(
            &config,
            Some(local.clone()),
            "jdtls",
            &update_check_path("jdtls"),
        )
        .unwrap();
        assert_eq!(result, Some(local));
    }

    #[test]
    fn test_default_is_always() {
        let result =
            should_use_local_or_download(&None, None, "test", &update_check_path("test")).unwrap();
        assert!(result.is_none(), "Default should be Always (None)");
    }
}
