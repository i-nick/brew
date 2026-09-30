use std::fs;
use std::path::{Path, PathBuf};

use brew_core::{Error, formula_token};

use super::Installer;
use super::bottle::read_cask_metadata;

impl Installer {
    pub fn uninstall(&mut self, name: &str, keep_data: bool) -> Result<(), Error> {
        let installed = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;
        let keg_name = formula_token(&installed.name);
        let recorded_paths = self.db.list_keg_files_for_name(name)?;

        let keg_path = self.cellar.keg_path(keg_name, &installed.version);
        let zap_paths = if installed.name.starts_with("cask:") && !keep_data {
            read_cask_metadata(&keg_path)?.zap_paths
        } else {
            Vec::new()
        };
        self.linker.unlink_keg(&keg_path)?;
        self.linker.unlink_isolated(name)?;
        for record in &recorded_paths {
            let path = Path::new(&record.linked_path);
            if !path.starts_with(&self.prefix) || path.starts_with(self.prefix.join("isolated")) {
                remove_recorded_path(path)?;
            }
        }

        {
            let tx = self.db.transaction()?;
            tx.record_uninstall(name)?;
            tx.commit()?;
        }

        self.cellar.remove_keg(keg_name, &installed.version)?;
        remove_zap_paths(&zap_paths)?;

        Ok(())
    }

    pub fn gc(&mut self) -> Result<Vec<String>, Error> {
        let unreferenced = self.db.get_unreferenced_store_keys()?;
        let mut removed = Vec::new();

        for store_key in unreferenced {
            self.store.remove_entry(&store_key)?;
            self.db.delete_store_ref(&store_key)?;
            removed.push(store_key);
        }

        Ok(removed)
    }
}

fn remove_recorded_path(path: &std::path::Path) -> Result<(), Error> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(Error::store("failed to read installed artifact metadata")(
                err,
            ));
        }
    };

    let result = if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|err| removal_error(path, err))
}

fn removal_error(path: &Path, err: std::io::Error) -> Error {
    const EPERM: i32 = 1;
    let is_app_bundle = path.extension().is_some_and(|ext| ext == "app");

    // macOS App Management (TCC) blocks modifying signed app bundles unless the
    // calling terminal has been granted that permission; EPERM is the only signal.
    if cfg!(target_os = "macos") && is_app_bundle && err.raw_os_error() == Some(EPERM) {
        return Error::FileError {
            message: format!(
                "cannot remove '{}': {err}.\n\n\
                macOS App Management is blocking changes to this app. Allow your terminal in\n  \
                System Settings > Privacy & Security > App Management\n\
                then restart the terminal and re-run the command.",
                path.display()
            ),
        };
    }

    Error::FileError {
        message: format!("failed to remove '{}': {err}", path.display()),
    }
}

fn remove_zap_paths(paths: &[String]) -> Result<(), Error> {
    for raw_path in paths {
        let path = expand_zap_path(raw_path)?;
        remove_recorded_path(&path)?;
    }

    Ok(())
}

fn expand_zap_path(raw_path: &str) -> Result<PathBuf, Error> {
    let path = if raw_path == "~" {
        PathBuf::from(std::env::var("HOME").map_err(|_| Error::InvalidArgument {
            message: "HOME must be set to expand cask zap paths".to_string(),
        })?)
    } else if let Some(stripped) = raw_path.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").map_err(|_| Error::InvalidArgument {
            message: "HOME must be set to expand cask zap paths".to_string(),
        })?)
        .join(stripped)
    } else {
        PathBuf::from(raw_path)
    };

    if !path.is_absolute() {
        return Err(Error::InvalidArgument {
            message: format!(
                "invalid cask zap path '{}': path must be absolute or start with '~/'",
                raw_path
            ),
        });
    }

    crate::validate_privileged_path(&path).map_err(|err| Error::InvalidArgument {
        message: format!("invalid cask zap path '{}': {}", raw_path, err),
    })?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{expand_zap_path, removal_error, remove_zap_paths};
    use crate::cellar::Cellar;
    use crate::installer::install::test_support::*;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    struct HomeOverride {
        previous: Option<OsString>,
    }

    impl HomeOverride {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", path);
            }
            Self { previous }
        }
    }

    impl Drop for HomeOverride {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = self.previous.take() {
                    std::env::set_var("HOME", previous);
                } else {
                    std::env::remove_var("HOME");
                }
            }
        }
    }

    #[test]
    fn expand_zap_path_expands_tilde() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeOverride::set(tmp.path());

        let expanded = expand_zap_path("~/Library/Caches/Test").unwrap();
        assert_eq!(expanded, tmp.path().join("Library/Caches/Test"));
    }

    #[test]
    fn remove_zap_paths_ignores_missing_paths() {
        let tmp = TempDir::new().unwrap();
        let _home = HomeOverride::set(tmp.path());

        let result = remove_zap_paths(&["~/does-not-exist".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn removal_error_names_path_and_hints_app_management_for_app_bundles() {
        let eperm = || std::io::Error::from_raw_os_error(1);

        let err = removal_error(Path::new("/Applications/Foo.app"), eperm()).to_string();
        assert!(err.contains("/Applications/Foo.app"));
        assert!(!err.contains("store corruption"));
        assert_eq!(err.contains("App Management"), cfg!(target_os = "macos"));

        let err = removal_error(Path::new("/tmp/foo"), eperm()).to_string();
        assert!(err.contains("/tmp/foo"));
        assert!(!err.contains("App Management"));
    }

    #[test]
    fn expand_zap_path_rejects_relative_paths() {
        let err = expand_zap_path("relative/path").unwrap_err();
        assert!(err.to_string().contains("absolute or start with '~/'"));
    }

    #[test]
    fn expand_zap_path_rejects_parent_traversal() {
        let err = expand_zap_path("/tmp/../evil").unwrap_err();
        assert!(err.to_string().contains("invalid cask zap path"));
        assert!(err.to_string().contains("'..'"));
    }

    #[tokio::test]
    async fn uninstall_cleans_everything() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("uninstallme");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "uninstallme",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/uninstallme-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/uninstallme.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/uninstallme-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["uninstallme".to_string()], true)
            .await
            .unwrap();

        assert!(installer.is_installed("uninstallme"));
        assert!(root.join("cellar/uninstallme/1.0.0").exists());
        assert!(prefix.join("bin/uninstallme").exists());

        installer.uninstall("uninstallme", false).unwrap();

        assert!(!installer.is_installed("uninstallme"));
        assert!(!root.join("cellar/uninstallme/1.0.0").exists());
        assert!(!prefix.join("bin/uninstallme").exists());
    }

    #[tokio::test]
    async fn gc_removes_unreferenced_store_entries() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("gctest");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "gctest",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/gctest-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/gctest.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/bottles/gctest-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["gctest".to_string()], true)
            .await
            .unwrap();

        assert!(root.join("store").join(&bottle_sha).exists());

        installer.uninstall("gctest", false).unwrap();

        assert!(root.join("store").join(&bottle_sha).exists());

        let removed = installer.gc().unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], bottle_sha);

        assert!(!root.join("store").join(&bottle_sha).exists());
        assert!(
            installer
                .db
                .get_unreferenced_store_keys()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn gc_does_not_remove_referenced_store_entries() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("keepme");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "keepme",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/keepme-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/keepme.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/bottles/keepme-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        installer
            .install(&["keepme".to_string()], true)
            .await
            .unwrap();

        assert!(root.join("store").join(&bottle_sha).exists());

        let removed = installer.gc().unwrap();
        assert!(removed.is_empty());

        assert!(root.join("store").join(&bottle_sha).exists());
    }

    #[tokio::test]
    async fn uninstall_accepts_full_tap_reference_after_install() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("terraform");
        let sha = sha256_hex(&bottle);
        let tag = get_test_bottle_tag();

        let tap_formula_rb = format!(
            r#"
class Terraform < Formula
  version "1.10.0"
  bottle do
    root_url "{}/v2/hashicorp/tap"
    sha256 {}: "{}"
  end
end
"#,
            mock_server.uri(),
            tag,
            sha
        );

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/hashicorp/tap/terraform/blobs/sha256:{sha}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        );

        installer
            .install(&["hashicorp/tap/terraform".to_string()], true)
            .await
            .unwrap();

        assert!(installer.is_installed("hashicorp/tap/terraform"));
        assert!(!installer.is_installed("terraform"));
        assert!(root.join("cellar/terraform/1.10.0").exists());
        installer
            .uninstall("hashicorp/tap/terraform", false)
            .unwrap();
        assert!(!installer.is_installed("hashicorp/tap/terraform"));
        assert!(!root.join("cellar/terraform/1.10.0").exists());
    }

    #[tokio::test]
    async fn uninstalling_non_installed_tap_ref_does_not_remove_core_formula() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("terraform");
        let sha = sha256_hex(&bottle);
        let tag = get_test_bottle_tag();
        let core_json = format!(
            r#"{{
                "name": "terraform",
                "versions": {{ "stable": "1.10.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/terraform-1.10.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/terraform.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(core_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/terraform-1.10.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let mut installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        );
        installer
            .install(&["terraform".to_string()], true)
            .await
            .unwrap();
        assert!(installer.is_installed("terraform"));

        let err = installer
            .uninstall("hashicorp/tap/terraform", false)
            .unwrap_err();
        assert!(matches!(err, brew_core::Error::NotInstalled { .. }));
        assert!(installer.is_installed("terraform"));
    }
}
