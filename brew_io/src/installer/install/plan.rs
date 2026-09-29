use std::collections::{BTreeMap, HashMap, HashSet};

use brew_core::{BuildPlan, Error, Formula, InstallMethod, select_bottle};

use crate::network::tap_formula::parse_tap_formula_ref;

use super::{InstallPlan, Installer, PlannedInstall};

impl Installer {
    pub async fn plan(&self, names: &[String]) -> Result<InstallPlan, Error> {
        self.plan_with_options(names, false).await
    }

    pub async fn plan_unrequested(&self, names: &[String]) -> Result<InstallPlan, Error> {
        self.plan_with_request_status(names, false, false).await
    }

    pub async fn plan_with_options(
        &self,
        names: &[String],
        build_from_source: bool,
    ) -> Result<InstallPlan, Error> {
        self.plan_with_request_status(names, build_from_source, true)
            .await
    }

    pub(crate) async fn plan_with_request_status(
        &self,
        names: &[String],
        build_from_source: bool,
        requested: bool,
    ) -> Result<InstallPlan, Error> {
        let formulas = self.fetch_all_formulas(names).await?;
        let ordered = brew_core::resolve_closure(names, &formulas)?;
        let requested_names: HashSet<&str> = if requested {
            names.iter().map(String::as_str).collect()
        } else {
            HashSet::new()
        };

        let mut items = Vec::with_capacity(ordered.len());
        for install_name in ordered {
            let formula = formulas.get(&install_name).cloned().unwrap();
            let method = if build_from_source {
                match BuildPlan::from_formula(&formula, &self.prefix) {
                    Some(plan) => InstallMethod::Source(plan),
                    None => InstallMethod::Bottle(select_bottle(&formula)?),
                }
            } else {
                match select_bottle(&formula) {
                    Ok(bottle) => InstallMethod::Bottle(bottle),
                    Err(bottle_err) => match BuildPlan::from_formula(&formula, &self.prefix) {
                        Some(plan) => InstallMethod::Source(plan),
                        None => return Err(bottle_err),
                    },
                }
            };
            items.push(PlannedInstall {
                requested: requested_names.contains(install_name.as_str()),
                install_name,
                formula,
                method,
            });
        }

        Ok(InstallPlan { items })
    }

    pub(crate) async fn fetch_all_formulas(
        &self,
        names: &[String],
    ) -> Result<BTreeMap<String, Formula>, Error> {
        use std::collections::HashSet;

        let mut formulas = BTreeMap::new();
        let mut fetched: HashSet<String> = HashSet::new();
        let mut raw_cache: HashMap<String, Formula> = HashMap::new();
        let mut to_fetch: Vec<String> = names.to_vec();

        while !to_fetch.is_empty() {
            let batch: Vec<String> = to_fetch
                .drain(..)
                .filter(|n| !fetched.contains(n))
                .collect();

            if batch.is_empty() {
                break;
            }

            for n in &batch {
                fetched.insert(n.clone());
            }

            for name in batch {
                let mut formula = match raw_cache.remove(&name) {
                    Some(formula) => formula,
                    None => self.api_client.get_formula(&name).await?,
                };

                formula.dependencies = self
                    .resolve_formula_dependencies(&name, &formula.dependencies, &mut raw_cache)
                    .await?;

                for dep in &formula.dependencies {
                    if !fetched.contains(dep) && !to_fetch.contains(dep) {
                        to_fetch.push(dep.clone());
                    }
                }

                formulas.insert(name, formula);
            }
        }

        Ok(formulas)
    }

    async fn resolve_formula_dependencies(
        &self,
        formula_name: &str,
        dependencies: &[String],
        raw_cache: &mut HashMap<String, Formula>,
    ) -> Result<Vec<String>, Error> {
        let Some(spec) = parse_tap_formula_ref(formula_name) else {
            return Ok(dependencies.to_vec());
        };

        let mut resolved = Vec::with_capacity(dependencies.len());
        for dependency in dependencies {
            if parse_tap_formula_ref(dependency).is_some() || !is_unqualified_formula(dependency) {
                resolved.push(dependency.clone());
                continue;
            }

            let same_tap = format!("{}/{}/{}", spec.owner, spec.repo, dependency);
            match self.api_client.get_formula(&same_tap).await {
                Ok(formula) => {
                    raw_cache.insert(same_tap.clone(), formula);
                    resolved.push(same_tap);
                }
                Err(Error::MissingFormula { .. }) => resolved.push(dependency.clone()),
                Err(err) => return Err(err),
            }
        }

        Ok(resolved)
    }
}

fn is_unqualified_formula(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.starts_with("cask:")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::installer::install::test_support::*;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    #[tokio::test]
    async fn plans_tapped_formula_with_core_dependency() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let dep_bottle = create_bottle_tarball("go");
        let dep_sha = sha256_hex(&dep_bottle);
        let tag = get_test_bottle_tag();
        let dep_json = format!(
            r#"{{
                "name": "go",
                "versions": {{ "stable": "1.24.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/go-1.24.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            dep_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/go.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&dep_json))
            .mount(&mock_server)
            .await;

        let tap_formula_rb = format!(
            r#"
class Terraform < Formula
  version "1.10.0"
  depends_on "go"
  bottle do
    root_url "{}/ghcr/hashicorp/tap"
    sha256 {}: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#,
            mock_server.uri(),
            tag
        );

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        );
        let plan = installer
            .plan(&["hashicorp/tap/terraform".to_string()])
            .await
            .unwrap();

        let planned_names: Vec<String> = plan
            .items
            .iter()
            .map(|item| item.formula.name.clone())
            .collect();
        assert!(planned_names.contains(&"terraform".to_string()));
        assert!(planned_names.contains(&"go".to_string()));
    }

    #[tokio::test]
    async fn tapped_formula_dependency_resolves_same_tap_first() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let root_formula_rb = format!(
            r#"
class Roottool < Formula
  version "1.0.0"
  depends_on "helper"
  bottle do
    root_url "{}/ghcr/example/tap"
    sha256 {}: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  end
end
"#,
            mock_server.uri(),
            tag
        );
        let helper_formula_rb = format!(
            r#"
class Helper < Formula
  version "1.0.0"
  bottle do
    root_url "{}/ghcr/example/tap"
    sha256 {}: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  end
end
"#,
            mock_server.uri(),
            tag
        );

        Mock::given(method("GET"))
            .and(path("/example/homebrew-tap/main/Formula/roottool.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(root_formula_rb))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/example/homebrew-tap/main/Formula/helper.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(helper_formula_rb))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        );
        let plan = installer
            .plan(&["example/tap/roottool".to_string()])
            .await
            .unwrap();

        let install_names: Vec<String> = plan
            .items
            .iter()
            .map(|item| item.install_name.clone())
            .collect();
        assert!(install_names.contains(&"example/tap/roottool".to_string()));
        assert!(install_names.contains(&"example/tap/helper".to_string()));
        assert!(!install_names.contains(&"helper".to_string()));
    }

    #[tokio::test]
    async fn falls_back_to_source_when_no_bottle() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let formula_json = r#"{
            "name": "nobottle",
            "versions": { "stable": "1.0.0" },
            "dependencies": [],
            "build_dependencies": ["pkgconf"],
            "urls": {
                "stable": {
                    "url": "https://example.com/nobottle-1.0.0.tar.gz",
                    "checksum": "abc123"
                }
            },
            "ruby_source_path": "Formula/n/nobottle.rb",
            "bottle": { "stable": { "files": {} } }
        }"#;

        Mock::given(method("GET"))
            .and(path("/formula/nobottle.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let plan = installer.plan(&["nobottle".to_string()]).await.unwrap();

        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].formula.name, "nobottle");
        assert!(matches!(
            plan.items[0].method,
            brew_core::InstallMethod::Source(_)
        ));

        if let brew_core::InstallMethod::Source(ref bp) = plan.items[0].method {
            assert_eq!(bp.source_url, "https://example.com/nobottle-1.0.0.tar.gz");
            assert_eq!(bp.formula_name, "nobottle");
            assert_eq!(bp.build_dependencies, vec!["pkgconf"]);
        }
    }

    #[tokio::test]
    async fn prefers_bottle_over_source() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "hasboth",
                "versions": {{ "stable": "2.0.0" }},
                "dependencies": [],
                "urls": {{
                    "stable": {{
                        "url": "https://example.com/hasboth-2.0.0.tar.gz",
                        "checksum": "def456"
                    }}
                }},
                "ruby_source_path": "Formula/h/hasboth.rb",
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "https://example.com/hasboth.bottle.tar.gz",
                                "sha256": "aabbccdd"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag
        );

        Mock::given(method("GET"))
            .and(path("/formula/hasboth.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let plan = installer.plan(&["hasboth".to_string()]).await.unwrap();

        assert_eq!(plan.items.len(), 1);
        assert!(matches!(
            plan.items[0].method,
            brew_core::InstallMethod::Bottle(_)
        ));
    }

    #[tokio::test]
    async fn errors_when_no_bottle_and_no_source() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let formula_json = r#"{
            "name": "nothing",
            "versions": { "stable": "1.0.0" },
            "dependencies": [],
            "bottle": { "stable": { "files": {} } }
        }"#;

        Mock::given(method("GET"))
            .and(path("/formula/nothing.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json))
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

        let installer = Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.clone(),
            root.join("locks"),
        );

        let result = installer.plan(&["nothing".to_string()]).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            brew_core::Error::UnsupportedBottle { .. }
        ));
    }
}
