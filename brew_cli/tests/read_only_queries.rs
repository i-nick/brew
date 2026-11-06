//! Metadata queries must be usable without installation/cache initialization or write access.

use std::collections::BTreeMap;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

use brew_io::Database;

fn query(root: &Path, prefix: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_b"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("NO_COLOR", "1")
        .env("BREW_ROOT", root)
        .env("BREW_PREFIX", prefix)
        .env("BREW_AUTO_INIT", "true")
        .args(args)
        .output()
        .unwrap()
}

fn success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    mode: u32,
    modified: SystemTime,
    data: Option<Vec<u8>>,
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            (
                entry.path().strip_prefix(root).unwrap().to_path_buf(),
                Entry {
                    mode: metadata.permissions().mode(),
                    modified: metadata.modified().unwrap(),
                    data: metadata.is_file().then(|| fs::read(entry.path()).unwrap()),
                },
            )
        })
        .collect()
}

struct ReadOnly(Vec<(PathBuf, Permissions)>);

impl ReadOnly {
    fn new(paths: &[&Path]) -> Self {
        let mut guard = Self(Vec::new());
        for path in paths {
            let metadata = fs::metadata(path).unwrap();
            guard.0.push((path.to_path_buf(), metadata.permissions()));
            fs::set_permissions(
                path,
                Permissions::from_mode(if metadata.is_dir() { 0o555 } else { 0o444 }),
            )
            .unwrap();
        }
        guard
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        for (path, permissions) in &self.0 {
            let _ = fs::set_permissions(path, permissions.clone());
        }
    }
}

#[test]
fn list_and_info_read_existing_metadata_without_changing_the_installation() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("installation");
    let db_dir = root.join("db");
    fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("brew.sqlite3");
    let prefix = temp.path().join("unused-prefix");
    {
        let mut database = Database::open(&db_path).unwrap();
        let tx = database.transaction().unwrap();
        tx.record_install_with_requested("git", "2.50.0", &"a".repeat(64), true)
            .unwrap();
        tx.record_install_with_requested("gettext", "0.24", &"b".repeat(64), false)
            .unwrap();
        tx.record_install("hashicorp/tap/terraform", "1.10.0", &"c".repeat(64))
            .unwrap();
        tx.commit().unwrap();
    }
    let _read_only = ReadOnly::new(&[&root, &db_dir, &db_path]);
    let before = snapshot(&root);

    let listed = success(&query(&root, &prefix, &["list"]));
    assert!(listed.contains("git 2.50.0"));
    assert!(listed.contains("hashicorp/tap/terraform 1.10.0"));
    assert!(!listed.contains("gettext"));
    let all = success(&query(&root, &prefix, &["list", "--all"]));
    assert!(all.contains("gettext 0.24"));
    let info = success(&query(&root, &prefix, &["info", "git"]));
    assert!(info.contains("git") && info.contains("2.50.0"));
    assert!(info.contains(&"a".repeat(12)));
    let tap = success(&query(&root, &prefix, &["info", "terraform"]));
    assert!(tap.contains("hashicorp/tap/terraform"));
    let missing = success(&query(&root, &prefix, &["info", "missing"]));
    assert!(missing.contains("not installed"));

    assert_eq!(snapshot(&root), before);
    assert!(!prefix.exists());
    for name in ["store", "cache", "locks", "Cellar", ".b_write_test"] {
        assert!(!root.join(name).exists(), "query created {name}");
    }
}

#[test]
fn metadata_queries_do_not_initialize_missing_roots_even_with_auto_init() {
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("missing-installation");
    let prefix = temp.path().join("missing-prefix");
    for args in [vec!["list"], vec!["list", "--all"], vec!["info", "git"]] {
        let output = query(&root, &prefix, &args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("read-only"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.exists());
        assert!(!prefix.exists());
    }
}
