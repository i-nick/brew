use std::path::PathBuf;
use std::process::{Command, Output};

struct TestEnv {
    root: tempfile::TempDir,
    /// On macOS, Mach-O binary patching requires the prefix path to be no longer
    /// than the original Homebrew prefix (`/opt/homebrew` = 13 chars). The default
    /// OS temp directory on macOS (`/var/folders/…`) produces paths far too long,
    /// so we create a separate short temp dir in `/tmp` for the prefix.
    prefix_dir: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            root: tempfile::TempDir::new().expect("failed to create temp dir"),
            prefix_dir: tempfile::Builder::new()
                .prefix("b")
                .rand_bytes(3)
                .tempdir_in("/tmp")
                .expect("failed to create short prefix temp dir"),
        }
    }

    fn prefix(&self) -> PathBuf {
        self.prefix_dir.path().to_path_buf()
    }

    fn b(&self, args: &[&str]) -> Output {
        let b = env!("CARGO_BIN_EXE_b");
        Command::new(b)
            .env("BREW_ROOT", self.root.path())
            // Use the short prefix so Mach-O patching stays within the 13-char limit,
            // and prevent a host-level BREW_PREFIX from leaking into the test.
            .env("BREW_PREFIX", self.prefix())
            .env("BREW_AUTO_INIT", "true")
            .args(args)
            .output()
            .unwrap_or_else(|_| panic!("failed to execute {b} command"))
    }

    fn bin_dir(&self) -> PathBuf {
        self.prefix().join("bin")
    }

    fn count_store_entries(&self) -> usize {
        assert!(self.root.path().join("store").is_dir());
        std::fs::read_dir(self.root.path().join("store"))
            .map(|r| r.count())
            .expect("failed to read store directory")
    }

    fn run_binary(&self, name: &str, args: &[&str]) -> Output {
        let bin_path = self.bin_dir().join(name);
        Command::new(&bin_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin_dir().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to execute {}: {e}", bin_path.display()))
    }
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{} failed:\nstdout: {}\nstderr: {}",
        context,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_stdout_contains(output: &Output, needle: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(needle),
        "expected stdout to contain {needle:?}, got: {stdout}"
    );
}

fn assert_stdout_not_contains(output: &Output, needle: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(needle),
        "expected stdout not to contain {needle:?}, got: {stdout}"
    );
}

fn assert_no_installed_symlinks(dir: &std::path::Path) {
    if !dir.exists() {
        return;
    }
    let cellar = dir.join("Cellar");
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.expect("failed to read directory entry");
        if entry.path().starts_with(&cellar) {
            continue;
        }
        assert!(
            !entry.path_is_symlink(),
            "unexpected symlink: {}",
            entry.path().display()
        );
    }
}

#[test]
#[ignore = "integration test"]
fn test_ffmpeg_formula() {
    let t = TestEnv::new();

    assert_success(&t.b(&["install", "ffmpeg"]), "b install ffmpeg");

    // From the upstream formula test:
    // https://github.com/Homebrew/homebrew-core/blob/3076627c980d101ff02a720060c508433c44f293/Formula/f/ffmpeg.rb#L114
    let mp4out = t.root.path().join("video.mp4");
    assert_success(
        &t.run_binary(
            "ffmpeg",
            &[
                "-filter_complex",
                "testsrc=rate=1:duration=5",
                mp4out.to_str().unwrap(),
            ],
        ),
        "ffmpeg create test video",
    );
    assert!(mp4out.exists());
}

#[test]
#[ignore = "integration test"]
fn test_macos_27_installs_golden_gate_bottle() {
    // macOS 27+ maps to "golden_gate". Installs must use those bottles when
    // published and fall back to the newest older tag (tahoe, sequoia, …)
    // for formulas that have not been rebuilt yet.
    let sw_vers = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .expect("failed to run sw_vers");
    let major: u32 = String::from_utf8_lossy(&sw_vers.stdout)
        .trim()
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .expect("failed to parse macOS version");
    if major < 27 {
        eprintln!("skipping: host macOS {major} has bottles for its own codename");
        return;
    }

    let t = TestEnv::new();
    assert_success(&t.b(&["install", "xz"]), "b install xz on macOS 27+");
    assert_success(&t.run_binary("xz", &["--version"]), "xz --version");
}

#[test]
#[ignore = "integration test"]
fn test_curl_keg_only() {
    let t = TestEnv::new();

    assert_success(&t.b(&["install", "curl"]), "b install curl");

    // Keg-only executables are exposed through the brew bin directory,
    // while private support files remain isolated in the Cellar.
    assert!(
        t.bin_dir().join("curl").exists(),
        "curl should be linked into the brew bin directory"
    );

    let output = t.run_binary("curl", &["https://www.githubstatus.com"]);
    assert_success(&output, "curl https://www.githubstatus.com");
    assert_stdout_contains(&output, "GitHub");
}

#[test]
#[ignore = "integration test"]
fn test_install_uninstall_and_reinstall() {
    let t = TestEnv::new();

    assert_success(&t.b(&["install", "jq"]), "b install jq");

    let test_json = t.root.path().join("test.json");
    std::fs::write(&test_json, r#"{"foo":1, "bar":2}"#).expect("failed to write test.json");

    let output = t.run_binary("jq", &[".bar", test_json.to_str().unwrap()]);
    assert_success(&output, "jq .bar test.json");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "2\n");

    assert_success(&t.b(&["uninstall", "jq"]), "b uninstall jq");
    assert_success(&t.b(&["uninstall", "oniguruma"]), "b uninstall oniguruma");
    assert!(!t.bin_dir().join("jq").exists());
    assert_no_installed_symlinks(&t.prefix());

    assert_success(&t.b(&["install", "jq"]), "b install jq (reinstall)");
    assert_success(
        &t.run_binary("jq", &["--version"]),
        "jq --version after reinstall",
    );
}

#[test]
#[ignore = "integration test"]
fn test_list_installed_formulas() {
    let t = TestEnv::new();

    let output = t.b(&["list"]);
    assert_success(&output, "b list (empty)");
    assert_stdout_contains(&output, "No formulas installed");

    assert_success(&t.b(&["install", "jq"]), "b install jq");

    let output = t.b(&["list"]);
    assert_success(&output, "b list");
    assert_stdout_contains(&output, "jq");
    assert_stdout_not_contains(&output, "oniguruma");

    let output = t.b(&["list", "--all"]);
    assert_success(&output, "b list --all");
    assert_stdout_contains(&output, "jq");
    assert_stdout_contains(&output, "oniguruma");

    assert_success(&t.b(&["uninstall", "jq"]), "b uninstall jq");
    assert_success(&t.b(&["uninstall", "oniguruma"]), "b uninstall oniguruma");

    let output = t.b(&["list"]);
    assert_success(&output, "b list (empty)");
    assert_stdout_contains(&output, "No formulas installed");
}

#[test]
#[ignore = "integration test"]
fn test_info_finds_installed_formula() {
    let t = TestEnv::new();

    let output = t.b(&["info", "jq"]);
    assert_success(&output, "b info jq (not installed)");
    assert_stdout_contains(&output, "not installed");

    assert_success(&t.b(&["install", "jq"]), "b install jq");

    let output = t.b(&["info", "jq"]);
    assert_success(&output, "b info jq");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Name:") && !stdout.contains("not installed"),
        "stdout: {stdout}"
    );
}

#[test]
#[ignore = "integration test"]
fn test_gc_removes_unused_store_entries() {
    let t = TestEnv::new();

    assert_success(&t.b(&["gc"]), "b gc (empty)");
    assert_eq!(t.count_store_entries(), 0);

    assert_success(&t.b(&["install", "jq"]), "b install jq");
    let entries_before = t.count_store_entries();
    assert!(entries_before > 0);

    assert_success(&t.b(&["uninstall", "jq"]), "b uninstall jq");
    assert_success(&t.b(&["uninstall", "oniguruma"]), "b uninstall oniguruma");
    assert_eq!(t.count_store_entries(), entries_before);

    assert_success(&t.b(&["gc"]), "b gc");
    assert_eq!(t.count_store_entries(), 0);
}
