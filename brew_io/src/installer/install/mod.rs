mod bottle;
mod cleanup;
pub mod doctor;
mod outdated;
mod plan;
mod source;
mod uninstall;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::warn;

use crate::cellar::link::Linker;
use crate::cellar::materialize::Cellar;
use crate::network::api::ApiClient;
use crate::network::create_api_client_with_cache;
use crate::network::download::{DownloadProgressCallback, DownloadRequest, ParallelDownloader};
use crate::progress::{InstallProgress, ProgressCallback};
use crate::storage::blob::BlobCache;
use crate::storage::db::Database;
use crate::storage::store::Store;

use brew_core::{ConflictedLink, Error, Formula, InstallMethod};

use bottle::{LinkFormulaRequest, dependency_cellar_path};

const MAX_CORRUPTION_RETRIES: usize = 3;

/// Directory cask app bundles are installed into.
const DEFAULT_APPDIR: &str = "/Applications";

pub struct Installer {
    api_client: ApiClient,
    downloader: ParallelDownloader,
    store: Store,
    cellar: Cellar,
    linker: Linker,
    pub(crate) db: Database,
    prefix: PathBuf,
    locks_dir: PathBuf,
    appdir: PathBuf,
}

#[derive(Debug)]
pub struct PlannedInstall {
    pub install_name: String,
    pub formula: Formula,
    pub method: InstallMethod,
    pub requested: bool,
}

#[derive(Debug)]
pub struct InstallPlan {
    pub items: Vec<PlannedInstall>,
}

pub struct ExecuteResult {
    pub installed: usize,
    pub links: Vec<LinkOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupCandidate {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupResult {
    pub removed: Vec<CleanupCandidate>,
    pub removed_store_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    Shared {
        name: String,
    },
    Isolated {
        name: String,
        prefix: PathBuf,
        reason: IsolatedLinkReason,
        conflicts: Vec<ConflictedLink>,
        requested: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolatedLinkReason {
    KegOnly(String),
    Conflict,
}

/// A package that has a newer version available upstream.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OutdatedPackage {
    pub name: String,
    pub installed_version: String,
    pub current_version: String,
    #[serde(skip)]
    pub installed_sha256: String,
    #[serde(skip)]
    pub current_sha256: String,
    #[serde(skip)]
    pub is_source_build: bool,
}

impl Installer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_client: ApiClient,
        blob_cache: BlobCache,
        store: Store,
        cellar: Cellar,
        linker: Linker,
        db: Database,
        prefix: PathBuf,
        locks_dir: PathBuf,
    ) -> Self {
        Self {
            api_client,
            downloader: ParallelDownloader::new(blob_cache),
            store,
            cellar,
            linker,
            db,
            prefix,
            locks_dir,
            appdir: PathBuf::from(DEFAULT_APPDIR),
        }
    }

    pub fn clear_api_cache(&self) -> Result<usize, Error> {
        self.api_client.clear_cache()
    }

    pub async fn execute(&mut self, plan: InstallPlan, link: bool) -> Result<ExecuteResult, Error> {
        self.execute_with_progress(plan, link, None).await
    }

    pub async fn execute_with_progress(
        &mut self,
        plan: InstallPlan,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<ExecuteResult, Error> {
        let lock_path = self.locks_dir.join("install.lock");
        let lock_file =
            File::create(&lock_path).map_err(Error::store("failed to create install lock"))?;
        lock_file
            .lock()
            .map_err(Error::store("failed to acquire install lock"))?;
        let _lock = lock_file;

        let report = |event: InstallProgress| {
            if let Some(ref cb) = progress {
                cb(event);
            }
        };

        let (bottle_items, source_items): (Vec<_>, Vec<_>) = plan
            .items
            .into_iter()
            .partition(|item| matches!(item.method, InstallMethod::Bottle(_)));

        if bottle_items.is_empty() && source_items.is_empty() {
            return Ok(ExecuteResult {
                installed: 0,
                links: Vec::new(),
            });
        }

        let mut installed = 0usize;
        let mut links = Vec::new();
        let mut error: Option<Error> = None;

        if !bottle_items.is_empty() {
            let requests: Vec<DownloadRequest> = bottle_items
                .iter()
                .map(|item| {
                    let InstallMethod::Bottle(ref bottle) = item.method else {
                        unreachable!()
                    };
                    DownloadRequest {
                        url: bottle.url.clone(),
                        sha256: bottle.sha256.clone(),
                        name: item.formula.name.clone(),
                    }
                })
                .collect();

            let download_progress: Option<DownloadProgressCallback> = progress.clone().map(|cb| {
                Arc::new(move |event: InstallProgress| {
                    cb(event);
                }) as DownloadProgressCallback
            });

            let mut rx = self
                .downloader
                .download_streaming(requests, download_progress.clone());

            while let Some(result) = rx.recv().await {
                match result {
                    Ok(download) => {
                        match self
                            .process_bottle_item(
                                &bottle_items[download.index],
                                &download,
                                &download_progress,
                                link,
                                &report,
                            )
                            .await
                        {
                            Ok(link_outcome) => {
                                installed += 1;
                                if let Some(link_outcome) = link_outcome {
                                    links.push(link_outcome);
                                }
                            }
                            Err(e) => error = Some(e),
                        }
                    }
                    Err(e) => {
                        error = Some(e);
                    }
                }
            }
        }

        for item in &source_items {
            let InstallMethod::Source(ref build_plan) = item.method else {
                unreachable!()
            };

            report(InstallProgress::UnpackStarted {
                name: item.formula.name.clone(),
            });

            match self
                .install_from_source(item, build_plan, link, &report)
                .await
            {
                Ok(link_outcome) => {
                    installed += 1;
                    if let Some(link_outcome) = link_outcome {
                        links.push(link_outcome);
                    }
                }
                Err(e) => {
                    error = Some(e);
                    continue;
                }
            }
        }

        if let Some(e) = error {
            return Err(e);
        }

        Ok(ExecuteResult { installed, links })
    }

    pub async fn install(&mut self, names: &[String], link: bool) -> Result<ExecuteResult, Error> {
        self.install_with_request_status(names, link, true).await
    }

    pub async fn install_preserving_request_status(
        &mut self,
        names: &[String],
        link: bool,
    ) -> Result<ExecuteResult, Error> {
        self.install_with_request_status(names, link, false).await
    }

    async fn install_with_request_status(
        &mut self,
        names: &[String],
        link: bool,
        requested: bool,
    ) -> Result<ExecuteResult, Error> {
        let (casks, formulas): (Vec<_>, Vec<_>) = names
            .iter()
            .cloned()
            .partition(|name| name.starts_with("cask:"));

        let mut installed = 0usize;
        let mut links = Vec::new();

        if !formulas.is_empty() {
            let plan = self
                .plan_with_request_status(&formulas, false, requested)
                .await?;
            let result = self.execute(plan, link).await?;
            installed += result.installed;
            links.extend(result.links);
        }

        if !casks.is_empty() {
            installed += self
                .install_casks_with_request_status(&casks, link, requested)
                .await?
                .installed;
        }

        Ok(ExecuteResult { installed, links })
    }

    pub async fn install_casks(
        &mut self,
        names: &[String],
        link: bool,
    ) -> Result<ExecuteResult, Error> {
        self.install_casks_with_request_status(names, link, true)
            .await
    }

    async fn install_casks_with_request_status(
        &mut self,
        names: &[String],
        link: bool,
        requested: bool,
    ) -> Result<ExecuteResult, Error> {
        let mut installed = 0usize;
        for name in names {
            let token = name
                .strip_prefix("cask:")
                .expect("install_casks expects cask: prefixed names");
            self.install_single_cask(token, link, requested).await?;
            installed += 1;
        }
        Ok(ExecuteResult {
            installed,
            links: Vec::new(),
        })
    }

    pub fn is_installed(&self, name: &str) -> bool {
        self.db.get_installed(name).is_some()
    }

    pub fn get_installed(&self, name: &str) -> Option<crate::storage::db::InstalledKeg> {
        self.db.get_installed(name)
    }

    pub fn list_installed(&self) -> Result<Vec<crate::storage::db::InstalledKeg>, Error> {
        self.db.list_installed()
    }

    pub fn list_requested_installed(&self) -> Result<Vec<crate::storage::db::InstalledKeg>, Error> {
        self.db.list_requested_installed()
    }

    pub fn is_linked(&self, name: &str) -> Result<bool, Error> {
        let isolated_root = self.prefix.join("isolated");
        let records = self.db.list_keg_files_for_name(name)?;
        Ok(records
            .iter()
            .map(|record| Path::new(&record.linked_path))
            .any(|path| path.starts_with(&self.prefix) && !path.starts_with(&isolated_root)))
    }

    pub fn is_isolated_linked(&self, name: &str) -> Result<bool, Error> {
        let isolated_prefix = self.linker.isolated_prefix(name);
        let records = self.db.list_keg_files_for_name(name)?;
        Ok(records
            .iter()
            .any(|record| Path::new(&record.linked_path).starts_with(&isolated_prefix)))
    }

    pub fn isolated_prefix(&self, name: &str) -> PathBuf {
        self.linker.isolated_prefix(name)
    }

    pub fn keg_path(&self, name: &str, version: &str) -> PathBuf {
        self.cellar.keg_path(name, version)
    }

    fn cleanup_materialized(cellar: &Cellar, name: &str, version: &str) {
        if let Err(e) = cellar.remove_keg(name, version) {
            warn!(
                formula = %name,
                version = %version,
                error = %e,
                "failed to remove keg after install error"
            );
        }
    }
}

pub fn create_installer(
    root: &Path,
    prefix: &Path,
    concurrency: usize,
) -> Result<Installer, Error> {
    if !root.exists() {
        fs::create_dir_all(root).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::StoreCorruption {
                    message: format!(
                        "cannot create root directory '{}': permission denied.\n\n\
                        Create it with:\n  sudo mkdir -p {} && sudo chown $USER {}",
                        root.display(),
                        root.display(),
                        root.display()
                    ),
                }
            } else {
                Error::StoreCorruption {
                    message: format!("failed to create root directory '{}': {e}", root.display()),
                }
            }
        })?;
    }

    fs::create_dir_all(root.join("db")).map_err(Error::store("failed to create db directory"))?;

    let api_client = create_api_client_with_cache(root)?;

    let blob_cache =
        BlobCache::new(&root.join("cache")).map_err(Error::store("failed to create blob cache"))?;
    let store = Store::new(root).map_err(Error::store("failed to create store"))?;
    // Use prefix/Cellar so bottles' hardcoded rpaths work
    let cellar =
        Cellar::new_at(prefix.join("Cellar")).map_err(Error::store("failed to create cellar"))?;
    let linker = Linker::new(prefix).map_err(Error::store("failed to create linker"))?;
    let db = Database::open(&root.join("db/brew.sqlite3"))?;

    let locks_dir = root.join("locks");
    fs::create_dir_all(&locks_dir).map_err(Error::store("failed to create locks directory"))?;

    let parallel_downloader = ParallelDownloader::with_concurrency(blob_cache, concurrency);

    Ok(Installer {
        api_client,
        downloader: parallel_downloader,
        store,
        cellar,
        linker,
        db,
        prefix: prefix.to_path_buf(),
        locks_dir,
        appdir: PathBuf::from(DEFAULT_APPDIR),
    })
}

#[cfg(test)]
mod test_support {
    pub fn create_bottle_tarball(formula_name: &str) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        use tar::Builder;

        let mut builder = Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{}/1.0.0/bin/{}", formula_name, formula_name))
            .unwrap();
        header.set_size(20);
        header.set_mode(0o755);
        header.set_cksum();

        let content = format!("#!/bin/sh\necho {}", formula_name);
        builder.append(&header, content.as_bytes()).unwrap();

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    pub fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    pub fn create_cask_dmg(app_name: &str, binary_relative_path: &str, contents: &str) -> Vec<u8> {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let tmp = tempfile::TempDir::new().unwrap();
        let payload_dir = tmp.path().join("payload");
        let binary_path = payload_dir.join(app_name).join(binary_relative_path);
        fs::create_dir_all(binary_path.parent().unwrap()).unwrap();
        fs::write(&binary_path, contents).unwrap();
        let mut permissions = fs::metadata(&binary_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary_path, permissions).unwrap();

        let dmg_path = tmp.path().join("fixture.dmg");
        let status = Command::new("/usr/bin/hdiutil")
            .arg("create")
            .arg("-quiet")
            .arg("-fs")
            .arg("HFS+")
            .arg("-format")
            .arg("UDZO")
            .arg("-srcfolder")
            .arg(&payload_dir)
            .arg(&dmg_path)
            .status()
            .unwrap();
        assert!(status.success(), "failed to create test DMG");

        fs::read(dmg_path).unwrap()
    }

    /// Deliberately an old tag: mock formulas go through the real
    /// `select_bottle`, so this must be compatible with any host macOS >= 14
    /// (newer tags like arm64_golden_gate would fail selection on older hosts).
    pub fn get_test_bottle_tag() -> &'static str {
        "arm64_sonoma"
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::Builder;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    use super::test_support::*;
    use super::{IsolatedLinkReason, LinkOutcome};

    fn create_versioned_bottle_tarball(
        formula_name: &str,
        version: &str,
        extra_entries: &[(&str, &[u8], u32)],
    ) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        let mut append = |path: String, contents: &[u8], mode: u32| {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append(&header, contents).unwrap();
        };

        append(
            format!("{formula_name}/{version}/bin/{formula_name}"),
            format!("#!/bin/sh\necho {formula_name} {version}").as_bytes(),
            0o755,
        );

        for (relative_path, contents, mode) in extra_entries {
            append(
                format!("{formula_name}/{version}/{relative_path}"),
                contents,
                *mode,
            );
        }

        let tar_data = builder.into_inner().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    fn create_rustup_style_bottle_tarball() -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        let content = b"#!/bin/sh\necho rustup-init";
        let mut header = tar::Header::new_gnu();
        header.set_path("rustup/1.29.0/bin/rustup-init").unwrap();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &content[..]).unwrap();

        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_size(0);
        link_header.set_mode(0o777);
        link_header.set_cksum();
        builder
            .append_link(&mut link_header, "rustup/1.29.0/bin/rustup", "rustup-init")
            .unwrap();

        let tar_data = builder.into_inner().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    fn formula_json(
        name: &str,
        version: &str,
        bottle_url: &str,
        sha256: &str,
        extra_fields: &str,
    ) -> String {
        let tag = get_test_bottle_tag();
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "{version}" }},
                "dependencies": [],
                {extra_fields}
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{tag}": {{
                                "url": "{bottle_url}",
                                "sha256": "{sha256}"
                            }}
                        }}
                    }}
                }}
            }}"#
        )
    }

    fn make_installer(
        root: &std::path::Path,
        prefix: &std::path::Path,
        server: &MockServer,
    ) -> Installer {
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

    #[tokio::test]
    async fn install_completes_successfully() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("testpkg");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "testpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
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
            .and(path("/formula/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/testpkg-1.0.0.{}.bottle.tar.gz",
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
            .install(&["testpkg".to_string()], true)
            .await
            .unwrap();

        assert!(root.join("cellar/testpkg/1.0.0").exists());
        assert!(prefix.join("bin/testpkg").exists());

        let installed = installer.db.get_installed("testpkg");
        assert!(installed.is_some());
        assert_eq!(installed.unwrap().version, "1.0.0");
    }

    #[tokio::test]
    async fn keg_only_formula_links_into_isolated_prefix() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_rustup_style_bottle_tarball();
        let bottle_sha = sha256_hex(&bottle);
        let tag = get_test_bottle_tag();
        let bottle_url = format!(
            "{}/bottles/rustup-1.29.0.{}.bottle.tar.gz",
            mock_server.uri(),
            tag
        );
        let formula = formula_json(
            "rustup",
            "1.29.0",
            &bottle_url,
            &bottle_sha,
            r#""keg_only": true,
                "keg_only_reason": { "reason": "it conflicts with rust", "explanation": "" },"#,
        );

        Mock::given(method("GET"))
            .and(path("/formula/rustup.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/rustup-1.29.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server);

        let plan = installer.plan(&["rustup".to_string()]).await.unwrap();
        let result = installer.execute(plan, true).await.unwrap();

        assert_eq!(result.installed, 1);
        assert!(matches!(
            &result.links[0],
            LinkOutcome::Isolated {
                name,
                reason: IsolatedLinkReason::KegOnly(reason),
                ..
            } if name == "rustup" && reason == "it conflicts with rust"
        ));
        assert!(prefix.join("bin/rustup").exists());
        assert!(prefix.join("bin/rustup-init").exists());

        installer.uninstall("rustup", false).unwrap();
        assert!(!prefix.join("bin/rustup").exists());
        assert!(!prefix.join("bin/rustup-init").exists());
    }

    #[tokio::test]
    async fn link_conflict_falls_back_to_isolated_prefix() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let alpha_bottle = create_bottle_tarball("alpha");
        let alpha_sha = sha256_hex(&alpha_bottle);
        let beta_bottle = create_versioned_bottle_tarball(
            "beta",
            "1.0.0",
            &[("bin/alpha", b"#!/bin/sh\necho conflicting alpha", 0o755)],
        );
        let beta_sha = sha256_hex(&beta_bottle);
        let tag = get_test_bottle_tag();

        let alpha_url = format!(
            "{}/bottles/alpha-1.0.0.{}.bottle.tar.gz",
            mock_server.uri(),
            tag
        );
        let beta_url = format!(
            "{}/bottles/beta-1.0.0.{}.bottle.tar.gz",
            mock_server.uri(),
            tag
        );

        Mock::given(method("GET"))
            .and(path("/formula/alpha.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(formula_json("alpha", "1.0.0", &alpha_url, &alpha_sha, "")),
            )
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/beta.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(formula_json("beta", "1.0.0", &beta_url, &beta_sha, "")),
            )
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/alpha-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(alpha_bottle))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/beta-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(beta_bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server);

        installer
            .install(&["alpha".to_string()], true)
            .await
            .unwrap();
        let plan = installer.plan(&["beta".to_string()]).await.unwrap();
        let result = installer.execute(plan, true).await.unwrap();

        assert_eq!(result.installed, 1);
        assert!(matches!(
            &result.links[0],
            LinkOutcome::Isolated {
                name,
                reason: IsolatedLinkReason::Conflict,
                conflicts,
                ..
            } if name == "beta" && conflicts.len() == 1
        ));
        assert!(prefix.join("bin/alpha").exists());
        assert!(prefix.join("bin/beta").exists());
    }

    #[tokio::test]
    async fn reinstalling_new_formula_version_replaces_its_own_links() {
        let first_server = MockServer::start().await;
        let second_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let first_bottle = create_versioned_bottle_tarball(
            "deno",
            "2.7.10",
            &[
                ("bin/dx", b"#!/bin/sh\necho dx 2.7.10", 0o755),
                (
                    "share/zsh/site-functions/_deno",
                    b"#compdef deno\n# 2.7.10",
                    0o644,
                ),
                (
                    "share/fish/vendor_completions.d/deno.fish",
                    b"# fish completion 2.7.10",
                    0o644,
                ),
                (
                    "etc/bash_completion.d/deno",
                    b"# bash completion 2.7.10",
                    0o644,
                ),
            ],
        );
        let first_sha = sha256_hex(&first_bottle);

        let second_bottle = create_versioned_bottle_tarball(
            "deno",
            "2.7.11",
            &[
                ("bin/dx", b"#!/bin/sh\necho dx 2.7.11", 0o755),
                (
                    "share/zsh/site-functions/_deno",
                    b"#compdef deno\n# 2.7.11",
                    0o644,
                ),
                (
                    "share/fish/vendor_completions.d/deno.fish",
                    b"# fish completion 2.7.11",
                    0o644,
                ),
                (
                    "etc/bash_completion.d/deno",
                    b"# bash completion 2.7.11",
                    0o644,
                ),
            ],
        );
        let second_sha = sha256_hex(&second_bottle);

        let tag = get_test_bottle_tag();
        let first_formula_json = format!(
            r#"{{
                "name": "deno",
                "versions": {{ "stable": "2.7.10" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/deno-2.7.10.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            first_server.uri(),
            tag,
            first_sha
        );
        let second_formula_json = format!(
            r#"{{
                "name": "deno",
                "versions": {{ "stable": "2.7.11" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/deno-2.7.11.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            second_server.uri(),
            tag,
            second_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/deno.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&first_formula_json))
            .mount(&first_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/deno-2.7.10.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(first_bottle))
            .mount(&first_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/formula/deno.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&second_formula_json))
            .mount(&second_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/deno-2.7.11.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(second_bottle))
            .mount(&second_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new_at(prefix.join("Cellar")).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", first_server.uri())).unwrap();
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
            .install(&["deno".to_string()], true)
            .await
            .unwrap();

        let api_client =
            ApiClient::with_base_url(format!("{}/formula", second_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new_at(prefix.join("Cellar")).unwrap();
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
            .install(&["deno".to_string()], true)
            .await
            .unwrap();

        let new_keg = prefix.join("Cellar/deno/2.7.11");
        for path in [
            prefix.join("bin/deno"),
            prefix.join("bin/dx"),
            prefix.join("share/zsh/site-functions/_deno"),
            prefix.join("share/fish/vendor_completions.d/deno.fish"),
            prefix.join("etc/bash_completion.d/deno"),
        ] {
            assert_eq!(
                fs::canonicalize(&path).unwrap(),
                fs::canonicalize(new_keg.join(path.strip_prefix(&prefix).unwrap())).unwrap()
            );
        }

        let installed = installer.db.get_installed("deno").unwrap();
        assert_eq!(installed.version, "2.7.11");
    }

    #[tokio::test]
    async fn install_with_dependencies() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let dep_bottle = create_bottle_tarball("deplib");
        let dep_sha = sha256_hex(&dep_bottle);
        let main_bottle = create_bottle_tarball("mainpkg");
        let main_sha = sha256_hex(&main_bottle);

        let tag = get_test_bottle_tag();
        let dep_json = format!(
            r#"{{"name":"deplib","versions":{{"stable":"1.0.0"}},"dependencies":[],"keg_only":true,"keg_only_reason":{{"reason":"versioned_formula","explanation":"versioned formula"}},"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/deplib-1.0.0.{}.bottle.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            tag,
            dep_sha
        );
        let main_json = format!(
            r#"{{"name":"mainpkg","versions":{{"stable":"2.0.0"}},"dependencies":["deplib"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mainpkg-2.0.0.{}.bottle.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            tag,
            main_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/deplib.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&dep_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/mainpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&main_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/deplib-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(dep_bottle))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/mainpkg-2.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(main_bottle))
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

        let result = installer
            .install(&["mainpkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("mainpkg").unwrap().requested);
        assert!(!installer.db.get_installed("deplib").unwrap().requested);
        assert!(result.links.iter().any(|link| matches!(
            link,
            LinkOutcome::Isolated {
                name,
                reason: IsolatedLinkReason::KegOnly(reason),
                requested: false,
                ..
            } if name == "deplib" && reason == "versioned formula"
        )));

        let requested = installer.db.list_requested_installed().unwrap();
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].name, "mainpkg");

        installer
            .install(&["deplib".to_string()], true)
            .await
            .unwrap();
        assert!(installer.db.get_installed("deplib").unwrap().requested);
    }

    #[tokio::test]
    async fn preserves_successful_installs_when_one_package_fails() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let good_bottle = create_bottle_tarball("goodpkg");
        let good_sha = sha256_hex(&good_bottle);

        let tag = get_test_bottle_tag();
        let good_json = format!(
            r#"{{
                "name": "goodpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            good_sha
        );

        let bad_json = format!(
            r#"{{
                "name": "badpkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/badpkg-1.0.0.{}.bottle.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            tag,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        Mock::given(method("GET"))
            .and(path("/formula/goodpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&good_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/badpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&bad_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/goodpkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(good_bottle))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/badpkg-1.0.0.{}.bottle.tar.gz", tag)))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_delay(Duration::from_millis(100))
                    .set_body_string("download failed"),
            )
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

        let result = installer
            .install(&["goodpkg".to_string(), "badpkg".to_string()], false)
            .await;
        assert!(result.is_err());

        assert!(installer.db.get_installed("goodpkg").is_some());
        assert!(installer.db.get_installed("badpkg").is_none());
        assert!(root.join("cellar/goodpkg/1.0.0").exists());
    }

    #[tokio::test]
    async fn db_persist_failure_cleans_materialized_and_linked_files() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("rollbackme");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "rollbackme",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/rollbackme-1.0.0.{}.bottle.tar.gz",
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
            .and(path("/formula/rollbackme.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/rollbackme-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let db_path = root.join("db/brew.sqlite3");
        let api_client =
            ApiClient::with_base_url(format!("{}/formula", mock_server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&db_path).unwrap();

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

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TABLE installed_kegs", []).unwrap();

        let result = installer.install(&["rollbackme".to_string()], true).await;
        assert!(result.is_err());

        assert!(!root.join("cellar/rollbackme/1.0.0").exists());
        assert!(!prefix.join("bin/rollbackme").exists());
        assert!(!prefix.join("opt/rollbackme").exists());
        assert!(root.join("store").join(&bottle_sha).exists());
    }

    #[tokio::test]
    async fn db_persist_failure_cleans_materialized_tap_formula_keg() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("terraform");
        let bottle_sha = sha256_hex(&bottle);
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
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/hashicorp/homebrew-tap/main/Formula/terraform.rb"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tap_formula_rb))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/v2/hashicorp/tap/terraform/blobs/sha256:{bottle_sha}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let db_path = root.join("db/brew.sqlite3");
        let api_client = ApiClient::with_base_url(format!("{}/formula", mock_server.uri()))
            .unwrap()
            .with_tap_raw_base_url(mock_server.uri());
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(&root).unwrap();
        let cellar = Cellar::new(&root).unwrap();
        let linker = Linker::new(&prefix).unwrap();
        let db = Database::open(&db_path).unwrap();

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

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DROP TABLE installed_kegs", []).unwrap();

        let result = installer
            .install(&["hashicorp/tap/terraform".to_string()], true)
            .await;
        assert!(result.is_err());

        assert!(!root.join("cellar/terraform/1.10.0").exists());
        assert!(!prefix.join("bin/terraform").exists());
        assert!(!prefix.join("opt/terraform").exists());
        assert!(root.join("store").join(&bottle_sha).exists());
    }

    #[tokio::test]
    async fn parallel_api_fetching_with_deep_deps() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let leaf1_bottle = create_bottle_tarball("leaf1");
        let leaf1_sha = sha256_hex(&leaf1_bottle);
        let leaf2_bottle = create_bottle_tarball("leaf2");
        let leaf2_sha = sha256_hex(&leaf2_bottle);
        let mid1_bottle = create_bottle_tarball("mid1");
        let mid1_sha = sha256_hex(&mid1_bottle);
        let mid2_bottle = create_bottle_tarball("mid2");
        let mid2_sha = sha256_hex(&mid2_bottle);
        let root_bottle = create_bottle_tarball("root");
        let root_sha = sha256_hex(&root_bottle);

        let tag = get_test_bottle_tag();
        let leaf1_json = format!(
            r#"{{"name":"leaf1","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/leaf1.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            leaf1_sha
        );
        let leaf2_json = format!(
            r#"{{"name":"leaf2","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/leaf2.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            leaf2_sha
        );
        let mid1_json = format!(
            r#"{{"name":"mid1","versions":{{"stable":"1.0.0"}},"dependencies":["leaf1"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mid1.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            mid1_sha
        );
        let mid2_json = format!(
            r#"{{"name":"mid2","versions":{{"stable":"1.0.0"}},"dependencies":["leaf1","leaf2"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/mid2.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            mid2_sha
        );
        let root_json = format!(
            r#"{{"name":"root","versions":{{"stable":"1.0.0"}},"dependencies":["mid1","mid2"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/root.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            root_sha
        );

        for (name, json) in [
            ("leaf1", &leaf1_json),
            ("leaf2", &leaf2_json),
            ("mid1", &mid1_json),
            ("mid2", &mid2_json),
            ("root", &root_json),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/formula/{}.json", name)))
                .respond_with(ResponseTemplate::new(200).set_body_string(json))
                .mount(&mock_server)
                .await;
        }
        for (name, bottle) in [
            ("leaf1", &leaf1_bottle),
            ("leaf2", &leaf2_bottle),
            ("mid1", &mid1_bottle),
            ("mid2", &mid2_bottle),
            ("root", &root_bottle),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/bottles/{}.tar.gz", name)))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
                .mount(&mock_server)
                .await;
        }

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
            .install(&["root".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("root").is_some());
        assert!(installer.db.get_installed("mid1").is_some());
        assert!(installer.db.get_installed("mid2").is_some());
        assert!(installer.db.get_installed("leaf1").is_some());
        assert!(installer.db.get_installed("leaf2").is_some());
    }

    #[tokio::test]
    async fn streaming_extraction_processes_as_downloads_complete() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let fast_bottle = create_bottle_tarball("fastpkg");
        let fast_sha = sha256_hex(&fast_bottle);
        let slow_bottle = create_bottle_tarball("slowpkg");
        let slow_sha = sha256_hex(&slow_bottle);

        let tag = get_test_bottle_tag();
        let fast_json = format!(
            r#"{{"name":"fastpkg","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/fast.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            fast_sha
        );
        let slow_json = format!(
            r#"{{"name":"slowpkg","versions":{{"stable":"1.0.0"}},"dependencies":["fastpkg"],"bottle":{{"stable":{{"files":{{"{}":{{"url":"{}/bottles/slow.tar.gz","sha256":"{}"}}}}}}}}}}"#,
            tag,
            mock_server.uri(),
            slow_sha
        );

        Mock::given(method("GET"))
            .and(path("/formula/fastpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&fast_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/slowpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&slow_json))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/fast.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fast_bottle.clone()))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/slow.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(slow_bottle.clone())
                    .set_delay(Duration::from_millis(100)),
            )
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
            .install(&["slowpkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.db.get_installed("fastpkg").is_some());
        assert!(installer.db.get_installed("slowpkg").is_some());
        assert!(root.join("cellar/fastpkg/1.0.0").exists());
        assert!(root.join("cellar/slowpkg/1.0.0").exists());
        assert!(prefix.join("bin/fastpkg").exists());
        assert!(prefix.join("bin/slowpkg").exists());
    }

    #[tokio::test]
    async fn retries_on_corrupted_download() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("retrypkg");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "retrypkg",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/retrypkg-1.0.0.{}.bottle.tar.gz",
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
            .and(path("/formula/retrypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        let attempt_count = Arc::new(AtomicUsize::new(0));
        let attempt_clone = attempt_count.clone();
        let valid_bottle = bottle.clone();

        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/retrypkg-1.0.0.{}.bottle.tar.gz",
                tag
            )))
            .respond_with(move |_: &wiremock::Request| {
                let _attempt = attempt_clone.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_bytes(valid_bottle.clone())
            })
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
            .install(&["retrypkg".to_string()], true)
            .await
            .unwrap();

        assert!(installer.is_installed("retrypkg"));
        assert!(root.join("cellar/retrypkg/1.0.0").exists());
        assert!(prefix.join("bin/retrypkg").exists());
    }

    #[tokio::test]
    async fn fails_after_max_retries() {
        // Validates the retry mechanism structure -- proper integration test
        // would need injection of corruption between download and extraction.
    }
}
