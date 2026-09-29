use console::style;

use crate::ui::StdUi;

pub async fn execute(
    installer: &mut brew_io::Installer,
    dry_run: bool,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    if dry_run {
        let candidates = installer.cleanup_candidates().await?;
        if candidates.is_empty() {
            ui.info("No unused dependencies to remove.")
                .map_err(ui_error)?;
            return Ok(());
        }

        ui.heading("Unused dependencies that would be removed:")
            .map_err(ui_error)?;
        for candidate in candidates {
            ui.bullet(format!(
                "{} {}",
                style(candidate.name).bold(),
                style(candidate.version).dim()
            ))
            .map_err(ui_error)?;
        }
        return Ok(());
    }

    run_cleanup(installer, ui).await.map(|_| ())
}

pub async fn run_cleanup(
    installer: &mut brew_io::Installer,
    ui: &mut StdUi,
) -> Result<brew_io::CleanupResult, brew_core::Error> {
    let candidates = installer.cleanup_candidates().await?;
    if candidates.is_empty() {
        ui.info("No unused dependencies to remove.")
            .map_err(ui_error)?;
        return Ok(brew_io::CleanupResult {
            removed: Vec::new(),
            removed_store_keys: Vec::new(),
        });
    }

    ui.heading("Removing unused dependencies...")
        .map_err(ui_error)?;
    for candidate in &candidates {
        ui.step_start(format!(
            "{} {}",
            style(&candidate.name).bold(),
            style(&candidate.version).dim()
        ))
        .map_err(ui_error)?;
        match installer.uninstall(&candidate.name, false) {
            Ok(()) => ui.step_ok().map_err(ui_error)?,
            Err(err) => {
                ui.step_fail().map_err(ui_error)?;
                return Err(err);
            }
        }
    }

    let removed_store_keys = installer.gc()?;
    ui.blank_line().map_err(ui_error)?;
    ui.heading(format!(
        "Removed {} unused {} and {} store {}",
        style(candidates.len()).green().bold(),
        if candidates.len() == 1 {
            "dependency"
        } else {
            "dependencies"
        },
        style(removed_store_keys.len()).green().bold(),
        if removed_store_keys.len() == 1 {
            "entry"
        } else {
            "entries"
        }
    ))
    .map_err(ui_error)?;

    Ok(brew_io::CleanupResult {
        removed: candidates,
        removed_store_keys,
    })
}

fn ui_error(err: std::io::Error) -> brew_core::Error {
    brew_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use brew_io::network::api::ApiClient;
    use brew_io::{BlobCache, Cellar, Database, Installer, Linker, Store};
    use tempfile::TempDir;
    use wiremock::MockServer;

    use crate::commands;
    use crate::ui::Ui;

    fn make_installer(root: &Path, prefix: &Path, server: &MockServer) -> Installer {
        fs::create_dir_all(root.join("db")).unwrap();

        let api_client = ApiClient::with_base_url(format!("{}/formula", server.uri())).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(root).unwrap();
        let cellar = Cellar::new(root).unwrap();
        let linker = Linker::new(prefix).unwrap();
        let mut db = Database::open(&root.join("db/brew.sqlite3")).unwrap();

        {
            let tx = db.transaction().unwrap();
            tx.record_install_with_requested("root", "1.0.0", "root_sha", true)
                .unwrap();
            tx.record_install_with_requested("dep", "1.0.0", "dep_sha", false)
                .unwrap();
            tx.commit().unwrap();
        }

        fs::create_dir_all(root.join("cellar/root/1.0.0")).unwrap();
        fs::create_dir_all(root.join("cellar/dep/1.0.0")).unwrap();

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
    async fn dry_run_does_not_uninstall_candidates() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        installer.uninstall("root", false).unwrap();

        let mut ui = Ui::new();
        super::execute(&mut installer, true, &mut ui).await.unwrap();

        assert!(installer.get_installed("dep").is_some());
    }

    #[tokio::test]
    async fn cleanup_command_uninstalls_candidates() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);
        installer.uninstall("root", false).unwrap();

        let mut ui = Ui::new();
        super::execute(&mut installer, false, &mut ui)
            .await
            .unwrap();

        assert!(installer.get_installed("dep").is_none());
    }

    #[tokio::test]
    async fn uninstall_cleanup_removes_target_then_unused_dependencies() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("brew");
        let prefix = tmp.path().join("prefix");
        let mut installer = make_installer(&root, &prefix, &server);

        let mut ui = Ui::new();
        commands::uninstall::execute(
            &mut installer,
            vec!["root".to_string()],
            false,
            true,
            false,
            &mut ui,
        )
        .await
        .unwrap();

        assert!(installer.get_installed("root").is_none());
        assert!(installer.get_installed("dep").is_none());
    }
}
