use std::collections::{BTreeMap, BTreeSet};

use brew_core::Error;

use super::{CleanupCandidate, CleanupResult, Installer};

impl Installer {
    pub async fn cleanup_candidates(&self) -> Result<Vec<CleanupCandidate>, Error> {
        let installed = self.db.list_installed()?;
        let installed_by_name: BTreeMap<String, _> = installed
            .iter()
            .map(|keg| (keg.name.clone(), keg.clone()))
            .collect();
        let requested_roots: Vec<String> = installed_by_name
            .values()
            .filter(|keg| keg.requested && !keg.name.starts_with("cask:"))
            .map(|keg| keg.name.clone())
            .collect();

        let mut protected = BTreeSet::new();
        let mut stack = requested_roots;
        while let Some(name) = stack.pop() {
            if !protected.insert(name.clone()) {
                continue;
            }

            let Some(keg) = installed_by_name.get(&name) else {
                continue;
            };
            if !keg.deps_recorded {
                return Err(Error::StoreCorruption {
                    message: format!(
                        "dependency metadata is missing for installed package '{}'; reinstall it or reset brew before running cleanup",
                        keg.name
                    ),
                });
            }

            for dependency in self.db.list_dependencies_for_name(&name)? {
                if installed_by_name.contains_key(&dependency) && !protected.contains(&dependency) {
                    stack.push(dependency);
                }
            }
        }

        Ok(installed
            .into_iter()
            .filter(|keg| {
                !keg.requested && !keg.name.starts_with("cask:") && !protected.contains(&keg.name)
            })
            .map(|keg| CleanupCandidate {
                name: keg.name,
                version: keg.version,
            })
            .collect())
    }

    pub async fn cleanup_unused_dependencies(&mut self) -> Result<CleanupResult, Error> {
        let candidates = self.cleanup_candidates().await?;

        for candidate in &candidates {
            self.uninstall(&candidate.name, false)?;
        }

        let removed_store_keys = self.gc()?;

        Ok(CleanupResult {
            removed: candidates,
            removed_store_keys,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    use super::super::test_support::get_test_bottle_tag;

    fn make_installer(root: &Path, prefix: &Path, server: &MockServer) -> Installer {
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(root).unwrap();
        let cellar = Cellar::new(root).unwrap();
        let linker = Linker::new(prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        )
    }

    fn formula_json(server: &MockServer, name: &str, dependencies: &[&str]) -> String {
        let tag = get_test_bottle_tag();
        let deps = dependencies
            .iter()
            .map(|dep| format!(r#""{dep}""#))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [{deps}],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{tag}": {{
                                "url": "{}/bottles/{name}-1.0.0.{tag}.bottle.tar.gz",
                                "sha256": "{name}_sha"
                            }}
                        }}
                    }}
                }}
            }}"#,
            server.uri()
        )
    }

    async fn mock_formula(server: &MockServer, name: &str, dependencies: &[&str]) {
        Mock::given(method("GET"))
            .and(path(format!("/formula/{name}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                server,
                name,
                dependencies,
            )))
            .mount(server)
            .await;
    }

    fn record_install(installer: &mut Installer, name: &str, requested: bool, store_key: &str) {
        record_install_with_dependencies(installer, name, requested, store_key, &[]);
    }

    fn record_install_with_dependencies(
        installer: &mut Installer,
        name: &str,
        requested: bool,
        store_key: &str,
        dependencies: &[&str],
    ) {
        let tx = installer.db.transaction().unwrap();
        let dependencies = dependencies
            .iter()
            .map(|dependency| dependency.to_string())
            .collect::<Vec<_>>();
        tx.record_formula_install_with_dependencies(
            name,
            "1.0.0",
            store_key,
            requested,
            &dependencies,
        )
        .unwrap();
        tx.commit().unwrap();

        fs::create_dir_all(installer.cellar.keg_path(name, "1.0.0")).unwrap();
    }

    #[tokio::test]
    async fn cleanup_keeps_dependencies_required_by_requested_roots() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        mock_formula(&server, "root", &["dep"]).await;
        mock_formula(&server, "dep", &[]).await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        record_install_with_dependencies(&mut installer, "root", true, "root_sha", &["dep"]);
        record_install(&mut installer, "dep", false, "dep_sha");

        let candidates = installer.cleanup_candidates().await.unwrap();
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn cleanup_removes_dependency_after_root_is_uninstalled() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        record_install(&mut installer, "dep", false, "dep_sha");

        let result = installer.cleanup_unused_dependencies().await.unwrap();

        assert_eq!(result.removed.len(), 1);
        assert_eq!(result.removed[0].name, "dep");
        assert_eq!(result.removed_store_keys, vec!["dep_sha".to_string()]);
        assert!(installer.db.get_installed("dep").is_none());
        assert!(!installer.cellar.keg_path("dep", "1.0.0").exists());
    }

    #[tokio::test]
    async fn cleanup_keeps_shared_dependency_and_finds_orphan() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        mock_formula(&server, "root-a", &["shared"]).await;
        mock_formula(&server, "root-b", &["shared"]).await;
        mock_formula(&server, "shared", &[]).await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        record_install_with_dependencies(&mut installer, "root-a", true, "root_a_sha", &["shared"]);
        record_install_with_dependencies(&mut installer, "root-b", true, "root_b_sha", &["shared"]);
        record_install(&mut installer, "shared", false, "shared_sha");
        record_install(&mut installer, "orphan", false, "orphan_sha");

        let candidates = installer.cleanup_candidates().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "orphan");
    }

    #[tokio::test]
    async fn cleanup_keeps_explicitly_requested_dependency() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        mock_formula(&server, "dep", &[]).await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        record_install(&mut installer, "dep", true, "dep_sha");

        let candidates = installer.cleanup_candidates().await.unwrap();
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn cleanup_aborts_on_missing_recorded_deps_without_removing_packages() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install_with_requested("root", "1.0.0", "root_sha", true)
                .unwrap();
            tx.commit().unwrap();
        }
        fs::create_dir_all(installer.cellar.keg_path("root", "1.0.0")).unwrap();
        record_install(&mut installer, "dep", false, "dep_sha");

        let err = installer.cleanup_unused_dependencies().await.unwrap_err();

        assert!(matches!(err, brew_core::Error::StoreCorruption { .. }));
        assert!(installer.db.get_installed("dep").is_some());
        assert!(installer.cellar.keg_path("dep", "1.0.0").exists());
    }

    #[tokio::test]
    async fn cleanup_does_not_remove_casks() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        record_install(&mut installer, "cask:zed", false, "zed_sha");

        let candidates = installer.cleanup_candidates().await.unwrap();
        assert!(candidates.is_empty());
    }
}
