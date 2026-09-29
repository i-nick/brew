use brew_core::formula_token;
use brew_io::Installer;
use console::style;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::utils::{normalize_formula_name, suggest_missing_formula_matches};

/// Prepare a package for execution by ensuring it's installed
/// Returns the path to the executable
pub async fn prepare_execution(
    installer: &mut Installer,
    formula: &str,
) -> Result<PathBuf, brew_core::Error> {
    let normalized = normalize_formula_name(formula)?;

    let was_installed = installer.is_installed(&normalized);

    if !was_installed {
        eprintln!(
            "{} Installing {} temporarily...",
            style("==>").cyan().bold(),
            style(&normalized).green()
        );

        let plan = installer
            .plan_unrequested(std::slice::from_ref(&normalized))
            .await?;
        installer.execute(plan, false).await?;
    }

    let installed =
        installer
            .get_installed(&normalized)
            .ok_or_else(|| brew_core::Error::NotInstalled {
                name: normalized.clone(),
            })?;

    let executable_name = formula_token(&installed.name);
    let keg_path = installer.keg_path(executable_name, &installed.version);
    let bin_dir = keg_path.join("bin");
    let bin_path = select_executable(&bin_dir, executable_name)?;

    Ok(bin_path)
}

pub async fn execute(
    installer: &mut Installer,
    formula: String,
    args: Vec<String>,
) -> Result<(), brew_core::Error> {
    eprintln!(
        "{} Running {}...",
        style("==>").cyan().bold(),
        style(&formula).bold()
    );

    let bin_path = match prepare_execution(installer, &formula).await {
        Ok(path) => path,
        Err(e) => {
            let _ = suggest_missing_formula_matches(installer, &e).await;
            return Err(e);
        }
    };

    eprintln!(
        "{} Executing {}...",
        style("==>").cyan().bold(),
        style(&formula).green()
    );

    let mut cmd = Command::new(&bin_path);
    cmd.args(&args);

    if let Some(prefix_path) = detect_runtime_prefix(&bin_path) {
        if let Some(ca_bundle) = brew_io::find_ca_bundle_from_prefix(&prefix_path) {
            cmd.env("CURL_CA_BUNDLE", &ca_bundle);
            cmd.env("SSL_CERT_FILE", &ca_bundle);
        }

        if let Some(ca_dir) = brew_io::find_ca_dir(&prefix_path) {
            cmd.env("SSL_CERT_DIR", &ca_dir);
        }

        let lib_path = prefix_path.join("lib");
        if let Ok(existing_fallback) = std::env::var("DYLD_FALLBACK_LIBRARY_PATH") {
            cmd.env(
                "DYLD_FALLBACK_LIBRARY_PATH",
                format!("{}:{}", lib_path.display(), existing_fallback),
            );
        } else {
            cmd.env("DYLD_FALLBACK_LIBRARY_PATH", lib_path);
        }
    }

    let err = cmd.exec();

    Err(brew_core::Error::ExecutionError {
        message: format!("failed to execute '{}': {}", formula, err),
    })
}

fn select_executable(bin_dir: &Path, preferred_name: &str) -> Result<PathBuf, brew_core::Error> {
    let preferred = bin_dir.join(preferred_name);
    if is_executable_file(&preferred) {
        return Ok(preferred);
    }

    let entries = fs::read_dir(bin_dir).map_err(|_| brew_core::Error::ExecutionError {
        message: format!("no executable directory found at '{}'", bin_dir.display()),
    })?;
    let mut executables = Vec::new();
    for entry in entries {
        let entry = entry.map_err(brew_core::Error::file(
            "failed to read executable directory",
        ))?;
        let path = entry.path();
        if is_executable_file(&path) {
            executables.push(path);
        }
    }

    match executables.len() {
        1 => Ok(executables.remove(0)),
        0 => Err(brew_core::Error::ExecutionError {
            message: format!("executable '{}' not found", preferred_name),
        }),
        _ => {
            let names = executables
                .iter()
                .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
                .collect::<Vec<_>>()
                .join(", ");
            Err(brew_core::Error::ExecutionError {
                message: format!("package has multiple executables; choose one of: {}", names),
            })
        }
    }
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn detect_runtime_prefix(bin_path: &Path) -> Option<PathBuf> {
    let env_prefix = std::env::var("BREW_PREFIX").ok();
    detect_runtime_prefix_with_env(bin_path, env_prefix.as_deref())
}

fn detect_runtime_prefix_with_env(bin_path: &Path, env_prefix: Option<&str>) -> Option<PathBuf> {
    if let Some(prefix) = env_prefix {
        return Some(PathBuf::from(prefix));
    }

    for ancestor in bin_path.ancestors() {
        let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == "Cellar" || name == "cellar" {
            return ancestor.parent().map(Path::to_path_buf);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use brew_io::{ApiClient, BlobCache, Cellar, Database, Linker, Store};
    use std::fs;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn create_bottle_tarball(formula_name: &str) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        use tar::Builder;

        let mut builder = Builder::new(Vec::new());

        let content = format!("#!/bin/sh\necho {}", formula_name);
        let content_bytes = content.as_bytes();

        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{}/1.0.0/bin/{}", formula_name, formula_name))
            .unwrap();
        header.set_size(content_bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

        builder.append(&header, content_bytes).unwrap();

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    fn get_test_bottle_tag() -> &'static str {
        "arm64_sonoma"
    }

    #[tokio::test]
    async fn run_installs_package_if_not_present() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("testrun");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "testrun",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/testrun.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/testrun.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/bottles/testrun.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri()).unwrap();
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

        assert!(!installer.is_installed("testrun"));

        let bin_path = prepare_execution(&mut installer, "testrun").await.unwrap();

        assert!(installer.is_installed("testrun"));
        assert!(!prefix.join("bin/testrun").exists());

        assert!(bin_path.exists());
        assert!(bin_path.ends_with("bin/testrun"));

        let output = std::process::Command::new(&bin_path).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "testrun");
    }

    #[tokio::test]
    async fn run_reuses_already_installed_package() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        let bottle = create_bottle_tarball("alreadyinstalled");
        let bottle_sha = sha256_hex(&bottle);

        let tag = get_test_bottle_tag();
        let formula_json = format!(
            r#"{{
                "name": "alreadyinstalled",
                "versions": {{ "stable": "1.0.0" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{}": {{
                                "url": "{}/bottles/alreadyinstalled.tar.gz",
                                "sha256": "{}"
                            }}
                        }}
                    }}
                }}
            }}"#,
            tag,
            mock_server.uri(),
            bottle_sha
        );

        Mock::given(method("GET"))
            .and(path("/alreadyinstalled.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&formula_json))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/bottles/alreadyinstalled.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri()).unwrap();
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
            .install(&["alreadyinstalled".to_string()], false)
            .await
            .unwrap();
        assert!(installer.is_installed("alreadyinstalled"));

        let bin_path = prepare_execution(&mut installer, "alreadyinstalled")
            .await
            .unwrap();

        assert!(bin_path.exists());
        assert!(bin_path.ends_with("bin/alreadyinstalled"));

        let output = std::process::Command::new(&bin_path).output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "alreadyinstalled"
        );
    }

    #[tokio::test]
    async fn run_fails_for_missing_formula() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();

        Mock::given(method("GET"))
            .and(path("/nonexistent.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("homebrew");
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(mock_server.uri()).unwrap();
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

        let result = prepare_execution(&mut installer, "nonexistent").await;
        assert!(result.is_err());
    }

    #[test]
    fn ssl_cert_paths_use_prefix() {
        let prefix = "/opt/test/prefix";
        let ca_bundle = format!(
            "{}/opt/ca-certificates/share/ca-certificates/cacert.pem",
            prefix
        );
        let ca_dir = format!("{}/etc/ca-certificates", prefix);

        assert_eq!(
            ca_bundle,
            "/opt/test/prefix/opt/ca-certificates/share/ca-certificates/cacert.pem"
        );
        assert_eq!(ca_dir, "/opt/test/prefix/etc/ca-certificates");
    }

    #[test]
    fn detect_runtime_prefix_prefers_env_var() {
        let bin_path = PathBuf::from("/tmp/prefix/Cellar/foo/1.0.0/bin/foo");
        let detected = detect_runtime_prefix_with_env(&bin_path, Some("/env/prefix"));
        assert_eq!(detected, Some(PathBuf::from("/env/prefix")));
    }

    #[test]
    fn detect_runtime_prefix_from_cellar_path() {
        let bin_path = PathBuf::from("/opt/brew/prefix/Cellar/foo/1.0.0/bin/foo");
        let detected = detect_runtime_prefix_with_env(&bin_path, None);
        assert_eq!(detected, Some(PathBuf::from("/opt/brew/prefix")));
    }

    #[test]
    fn detect_runtime_prefix_from_lowercase_cellar_path() {
        let bin_path = PathBuf::from("/opt/brew/cellar/foo/1.0.0/bin/foo");
        let detected = detect_runtime_prefix_with_env(&bin_path, None);
        assert_eq!(detected, Some(PathBuf::from("/opt/brew")));
    }

    #[test]
    fn select_executable_falls_back_to_only_bin_entry() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let rg = bin_dir.join("rg");
        fs::write(&rg, "#!/bin/sh\n").unwrap();

        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&rg).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&rg, permissions).unwrap();
        }

        let selected = select_executable(&bin_dir, "ripgrep").unwrap();
        assert_eq!(selected, rg);
    }

    #[test]
    fn select_executable_errors_when_multiple_fallbacks_exist() {
        let tmp = TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        for name in ["foo", "bar"] {
            let path = bin_dir.join(name);
            fs::write(&path, "#!/bin/sh\n").unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&path, permissions).unwrap();
            }
        }

        let err = select_executable(&bin_dir, "pkg").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("multiple executables"), "got: {message}");
        assert!(message.contains("foo"), "got: {message}");
        assert!(message.contains("bar"), "got: {message}");
    }
}
