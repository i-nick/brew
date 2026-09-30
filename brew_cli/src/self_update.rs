//! Self-update: replace the installed `b`/`bx` binaries with a GitHub release.
//!
//! Releases publish `b-darwin-arm64`, `bx-darwin-arm64` and `SHA256SUMS`. An
//! update downloads all three from one pinned tag, verifies the checksums,
//! smoke-tests the new `b`, then atomically renames each binary into place
//! (rolling back if any swap fails).

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brew_core::Error;
use sha2::{Digest, Sha256};

pub const RELEASES_URL: &str = "https://github.com/i-nick/brew/releases";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Swapped in this order so `b`, the binary users run, is replaced last.
const BINARIES: [&str; 2] = ["bx", "b"];
const ASSET_SUFFIX: &str = "-darwin-arm64";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";

const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CHECK_CACHE_FILE: &str = "cache/self-update-check";

/// Base URL for releases; `BREW_RELEASES_URL` overrides it (used by tests).
pub fn releases_url() -> String {
    std::env::var("BREW_RELEASES_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| RELEASES_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    /// Parse `1.2.3` or `v1.2.3`.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let value = value.strip_prefix('v').unwrap_or(value);
        let mut parts = value.split('.').map(|part| part.parse::<u64>().ok());
        let version = Version(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }

    pub fn current() -> Self {
        Self::parse(CURRENT_VERSION).expect("CARGO_PKG_VERSION is a valid version")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

pub struct Updater {
    base_url: String,
    client: reqwest::Client,
    no_redirect_client: reqwest::Client,
}

impl Updater {
    pub fn new(base_url: String, timeout: Option<Duration>) -> Result<Self, Error> {
        let builder = || {
            let builder = reqwest::Client::builder().user_agent(format!("brew/{CURRENT_VERSION}"));
            match timeout {
                Some(timeout) => builder.timeout(timeout),
                None => builder,
            }
        };
        let client = builder().build().map_err(network_error)?;
        let no_redirect_client = builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(network_error)?;

        Ok(Self {
            base_url,
            client,
            no_redirect_client,
        })
    }

    /// Resolve the latest release from the `releases/latest` redirect, which
    /// avoids the GitHub API and its unauthenticated rate limit.
    pub async fn latest_version(&self) -> Result<Version, Error> {
        let url = format!("{}/latest", self.base_url);
        let response = self
            .no_redirect_client
            .get(&url)
            .send()
            .await
            .map_err(network_error)?;

        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|_| response.status().is_redirection())
            .ok_or_else(|| Error::NetworkFailure {
                message: format!(
                    "could not determine the latest release from {url} (HTTP {})",
                    response.status()
                ),
            })?;

        location
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .and_then(Version::parse)
            .ok_or_else(|| Error::NetworkFailure {
                message: format!("unexpected latest release location: {location}"),
            })
    }

    /// Download and verify every binary of release `version`.
    pub async fn download(&self, version: Version) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let checksums = self.fetch(version, CHECKSUMS_ASSET).await?;
        let checksums = parse_checksums(&String::from_utf8_lossy(&checksums));

        let mut binaries = Vec::with_capacity(BINARIES.len());
        for name in BINARIES {
            let asset = format!("{name}{ASSET_SUFFIX}");
            let expected = checksums.get(&asset).ok_or_else(|| Error::NetworkFailure {
                message: format!("{CHECKSUMS_ASSET} for v{version} has no entry for {asset}"),
            })?;
            let bytes = self.fetch(version, &asset).await?;
            verify_checksum(&bytes, expected)?;
            binaries.push((name.to_string(), bytes));
        }
        Ok(binaries)
    }

    async fn fetch(&self, version: Version, asset: &str) -> Result<Vec<u8>, Error> {
        let url = format!("{}/download/v{version}/{asset}", self.base_url);
        let response = self.client.get(&url).send().await.map_err(network_error)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NetworkFailure {
                message: format!("release v{version} has no {asset} (404 at {url})"),
            });
        }
        let response = response.error_for_status().map_err(network_error)?;
        Ok(response.bytes().await.map_err(network_error)?.to_vec())
    }
}

/// Parse `shasum -a 256` output: `<hex>  <name>` (or `<hex> *<name>`).
pub fn parse_checksums(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (hash, name) = line.trim().split_once(char::is_whitespace)?;
            let name = name.trim_start().trim_start_matches('*');
            Some((name.to_string(), hash.to_ascii_lowercase()))
        })
        .collect()
}

fn verify_checksum(bytes: &[u8], expected: &str) -> Result<(), Error> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected {
        return Err(Error::ChecksumMismatch {
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Directory holding the running `b`, if it is safe to update in place.
pub fn install_dir() -> Result<PathBuf, Error> {
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|err| Error::FileError {
            message: format!("failed to locate the running b binary: {err}"),
        })?;
    let dir = check_install_location(&exe)?;
    if !crate::init::is_writable(&dir) {
        return Err(Error::FileError {
            message: format!(
                "{} is not writable; reinstall with install.sh or fix its permissions",
                dir.display()
            ),
        });
    }
    Ok(dir)
}

fn check_install_location(exe: &Path) -> Result<PathBuf, Error> {
    let refuse = |reason: &str| Error::InvalidArgument {
        message: format!("cannot self-update {}: {reason}", exe.display()),
    };

    if exe.file_name().and_then(|name| name.to_str()) != Some("b") {
        return Err(refuse("only the installed `b` binary can self-update"));
    }

    let components: Vec<_> = exe
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if components
        .windows(2)
        .any(|pair| pair[0] == "target" && matches!(pair[1], "debug" | "release"))
    {
        return Err(refuse(
            "it is a cargo build output; rebuild from source instead",
        ));
    }

    let dir = exe
        .parent()
        .ok_or_else(|| refuse("it has no parent directory"))?;
    if dir.ends_with(".cargo/bin") {
        return Err(refuse(
            "it was installed with `cargo install`; update it the same way",
        ));
    }

    Ok(dir.to_path_buf())
}

/// Stage, smoke-test and atomically swap `binaries` into `dir`.
pub fn install_binaries(
    dir: &Path,
    binaries: &[(String, Vec<u8>)],
    version: Version,
) -> Result<(), Error> {
    let mut staged = Vec::with_capacity(binaries.len());
    let result = stage_all(dir, binaries, &mut staged)
        .and_then(|()| smoke_test(&staged, version))
        .and_then(|()| swap_all(dir, &staged));

    for (_, staged_path) in &staged {
        let _ = fs::remove_file(staged_path);
    }
    result
}

fn stage_all(
    dir: &Path,
    binaries: &[(String, Vec<u8>)],
    staged: &mut Vec<(String, PathBuf)>,
) -> Result<(), Error> {
    for (name, bytes) in binaries {
        let path = dir.join(format!(".{name}.new"));
        staged.push((name.clone(), path.clone()));

        let mut file = fs::File::create(&path).map_err(file_error(&path))?;
        file.write_all(bytes).map_err(file_error(&path))?;
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(file_error(&path))?;
        file.sync_all().map_err(file_error(&path))?;
    }
    Ok(())
}

/// Run the new `b --version` before touching the installed binaries.
fn smoke_test(staged: &[(String, PathBuf)], version: Version) -> Result<(), Error> {
    let Some((_, b_path)) = staged.iter().find(|(name, _)| name == "b") else {
        return Ok(());
    };

    let output = Command::new(b_path)
        .arg("--version")
        .output()
        .map_err(|err| Error::ExecutionError {
            message: format!("downloaded b failed to run: {err}"),
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = stdout.split_whitespace().nth(1).and_then(Version::parse);

    if !output.status.success() || reported != Some(version) {
        return Err(Error::ExecutionError {
            message: format!(
                "downloaded b did not report version {version} (got {:?})",
                stdout.trim()
            ),
        });
    }
    Ok(())
}

fn swap_all(dir: &Path, staged: &[(String, PathBuf)]) -> Result<(), Error> {
    // (target, backup if the target existed)
    let mut swapped: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();

    for (name, staged_path) in staged {
        let target = dir.join(name);
        match swap_one(dir, name, staged_path, &target) {
            Ok(backup) => swapped.push((target, backup)),
            Err(err) => {
                roll_back(&swapped);
                return Err(err);
            }
        }
    }

    for (_, backup) in &swapped {
        if let Some(backup) = backup {
            let _ = fs::remove_file(backup);
        }
    }
    Ok(())
}

fn swap_one(
    dir: &Path,
    name: &str,
    staged_path: &Path,
    target: &Path,
) -> Result<Option<PathBuf>, Error> {
    let backup = if target.exists() {
        let backup = dir.join(format!(".{name}.old"));
        let _ = fs::remove_file(&backup);
        fs::hard_link(target, &backup).map_err(file_error(&backup))?;
        Some(backup)
    } else {
        None
    };

    // rename() is atomic and safe while the old binary is running: the
    // running process keeps the old inode.
    if let Err(err) = fs::rename(staged_path, target) {
        if let Some(backup) = &backup {
            let _ = fs::remove_file(backup);
        }
        return Err(file_error(target)(err));
    }
    Ok(backup)
}

fn roll_back(swapped: &[(PathBuf, Option<PathBuf>)]) {
    for (target, backup) in swapped.iter().rev() {
        match backup {
            Some(backup) => {
                let _ = fs::rename(backup, target);
            }
            None => {
                let _ = fs::remove_file(target);
            }
        }
    }
}

// --- Update notices ---------------------------------------------------------

/// Latest release seen by the last check: `<unix seconds> <version>`.
struct CheckCache {
    checked_at: u64,
    latest: Version,
}

fn cache_path(root: &Path) -> PathBuf {
    root.join(CHECK_CACHE_FILE)
}

fn read_cache(root: &Path) -> Option<CheckCache> {
    let text = fs::read_to_string(cache_path(root)).ok()?;
    let (checked_at, latest) = text.trim().split_once(' ')?;
    Some(CheckCache {
        checked_at: checked_at.parse().ok()?,
        latest: Version::parse(latest)?,
    })
}

pub fn write_cache(root: &Path, latest: Version) {
    let path = cache_path(root);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, format!("{} {latest}\n", now_secs()));
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Return a newer release than the running one, checking the network at most
/// once per [`CHECK_INTERVAL`] unless `force` is set. Failures are silent.
pub async fn newer_release(root: &Path, base_url: String, force: bool) -> Option<Version> {
    let current = Version::current();
    let cached = read_cache(root);

    let fresh = cached.as_ref().is_some_and(|cache| {
        now_secs().saturating_sub(cache.checked_at) < CHECK_INTERVAL.as_secs()
    });

    let latest = if fresh && !force {
        cached.map(|cache| cache.latest)
    } else {
        let checked = match Updater::new(base_url, Some(Duration::from_secs(3))) {
            Ok(updater) => updater.latest_version().await.ok(),
            Err(_) => None,
        };
        if let Some(latest) = checked {
            write_cache(root, latest);
        }
        checked.or(cached.map(|cache| cache.latest))
    };

    latest.filter(|latest| *latest > current)
}

fn network_error(err: reqwest::Error) -> Error {
    Error::NetworkFailure {
        message: err.to_string(),
    }
}

fn file_error(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |err| Error::FileError {
        message: format!("{}: {err}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fake_b(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"b {version}\"\n").into_bytes()
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    async fn mock_release(server: &MockServer, version: &str, b: &[u8], bx: &[u8]) {
        let sums = format!("{}  b-darwin-arm64\n{}  bx-darwin-arm64\n", sha(b), sha(bx));
        let base = format!("/download/v{version}");
        for (asset, body) in [
            ("SHA256SUMS", sums.into_bytes()),
            ("b-darwin-arm64", b.to_vec()),
            ("bx-darwin-arm64", bx.to_vec()),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("{base}/{asset}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .mount(server)
                .await;
        }
    }

    #[test]
    fn version_parse_and_order() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.10.0"), Some(Version(0, 10, 0)));
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("1.2.x"), None);
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert_eq!(Version(1, 2, 3).to_string(), "1.2.3");
    }

    #[test]
    fn parses_shasum_output() {
        let sums = parse_checksums("ABC  b-darwin-arm64\ndef *bx-darwin-arm64\n\n");
        assert_eq!(sums["b-darwin-arm64"], "abc");
        assert_eq!(sums["bx-darwin-arm64"], "def");
    }

    #[test]
    fn refuses_cargo_builds_and_other_binaries() {
        assert!(check_install_location(Path::new("/repo/target/release/b")).is_err());
        assert!(check_install_location(Path::new("/Users/me/.cargo/bin/b")).is_err());
        assert!(check_install_location(Path::new("/Users/me/.local/bin/bx")).is_err());
        assert_eq!(
            check_install_location(Path::new("/Users/me/.local/bin/b")).unwrap(),
            PathBuf::from("/Users/me/.local/bin")
        );
    }

    #[tokio::test]
    async fn latest_version_reads_redirect_location() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "location",
                "https://github.com/i-nick/brew/releases/tag/v0.3.0",
            ))
            .mount(&server)
            .await;

        let updater = Updater::new(server.uri(), None).unwrap();
        assert_eq!(updater.latest_version().await.unwrap(), Version(0, 3, 0));
    }

    #[tokio::test]
    async fn download_rejects_checksum_mismatch() {
        let server = MockServer::start().await;
        mock_release(&server, "0.3.0", &fake_b("0.3.0"), b"bx").await;
        Mock::given(method("GET"))
            .and(path("/download/v0.3.1/SHA256SUMS"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{}  b-darwin-arm64\n{}  bx-darwin-arm64\n",
                "0".repeat(64),
                "0".repeat(64)
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/download/v0.3.1/bx-darwin-arm64"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered".to_vec()))
            .mount(&server)
            .await;

        let updater = Updater::new(server.uri(), None).unwrap();
        assert!(updater.download(Version(0, 3, 0)).await.is_ok());
        assert!(matches!(
            updater.download(Version(0, 3, 1)).await,
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn download_missing_release_is_an_error() {
        let server = MockServer::start().await;
        let updater = Updater::new(server.uri(), None).unwrap();
        assert!(updater.download(Version(9, 9, 9)).await.is_err());
    }

    #[tokio::test]
    async fn full_update_replaces_binaries() {
        let server = MockServer::start().await;
        let new_b = fake_b("0.3.0");
        mock_release(&server, "0.3.0", &new_b, b"new bx").await;

        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("b"), fake_b("0.2.0")).unwrap();
        fs::write(tmp.path().join("bx"), b"old bx").unwrap();

        let updater = Updater::new(server.uri(), None).unwrap();
        let binaries = updater.download(Version(0, 3, 0)).await.unwrap();
        install_binaries(tmp.path(), &binaries, Version(0, 3, 0)).unwrap();

        assert_eq!(fs::read(tmp.path().join("b")).unwrap(), new_b);
        assert_eq!(fs::read(tmp.path().join("bx")).unwrap(), b"new bx");
        let mode = fs::metadata(tmp.path().join("b"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "leftover files: {leftovers:?}");
    }

    #[test]
    fn failed_smoke_test_leaves_install_untouched() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("b"), b"old b").unwrap();
        fs::write(tmp.path().join("bx"), b"old bx").unwrap();

        // Reports the wrong version.
        let binaries = vec![
            ("bx".to_string(), b"new bx".to_vec()),
            ("b".to_string(), fake_b("0.2.9")),
        ];
        assert!(install_binaries(tmp.path(), &binaries, Version(0, 3, 0)).is_err());

        assert_eq!(fs::read(tmp.path().join("b")).unwrap(), b"old b");
        assert_eq!(fs::read(tmp.path().join("bx")).unwrap(), b"old bx");
        assert!(!tmp.path().join(".b.new").exists());
        assert!(!tmp.path().join(".bx.new").exists());
    }

    #[test]
    fn failed_swap_rolls_back_earlier_binaries() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("bx"), b"old bx").unwrap();
        // A directory at the `b` target makes its rename fail after `bx` swapped.
        fs::create_dir(tmp.path().join("b")).unwrap();
        fs::write(tmp.path().join("b/keep"), b"x").unwrap();

        let staged_bx = tmp.path().join(".bx.new");
        let staged_b = tmp.path().join(".b.new");
        fs::write(&staged_bx, b"new bx").unwrap();
        fs::write(&staged_b, b"new b").unwrap();
        let staged = vec![("bx".to_string(), staged_bx), ("b".to_string(), staged_b)];

        assert!(swap_all(tmp.path(), &staged).is_err());
        assert_eq!(fs::read(tmp.path().join("bx")).unwrap(), b"old bx");
        assert!(!tmp.path().join(".bx.old").exists());
    }

    #[tokio::test]
    async fn newer_release_uses_fresh_cache_without_network() {
        let tmp = TempDir::new().unwrap();
        let newer = Version(u64::MAX, 0, 0);
        write_cache(tmp.path(), newer);

        // Unreachable URL: a fresh cache must not hit the network.
        let found = newer_release(tmp.path(), "http://127.0.0.1:9".to_string(), false).await;
        assert_eq!(found, Some(newer));
    }

    #[tokio::test]
    async fn newer_release_refreshes_stale_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "/releases/tag/v0.0.1"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("cache")).unwrap();
        fs::write(
            cache_path(tmp.path()),
            format!("0 {}\n", Version(u64::MAX, 0, 0)),
        )
        .unwrap();

        // v0.0.1 is older than the running build, so no notice.
        assert_eq!(newer_release(tmp.path(), server.uri(), false).await, None);
        assert_eq!(read_cache(tmp.path()).unwrap().latest, Version(0, 0, 1));
    }
}
