use std::path::Path;

use console::style;

use crate::self_update::{self, Updater, Version};
use crate::ui::StdUi;

pub async fn execute(
    root: &Path,
    check: bool,
    target: Option<String>,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let current = Version::current();
    let updater = Updater::new(self_update::releases_url(), None)?;

    let target = match target {
        Some(target) => {
            Version::parse(&target).ok_or_else(|| brew_core::Error::InvalidArgument {
                message: format!("invalid version '{target}': expected X.Y.Z"),
            })?
        }
        None => {
            ui.heading("Checking for updates...").map_err(ui_error)?;
            let latest = updater.latest_version().await?;
            self_update::write_cache(root, latest);
            if latest <= current {
                ui.println(format!(
                    "    {} b {} is up to date",
                    style("✓").green(),
                    style(current).bold()
                ))
                .map_err(ui_error)?;
                return Ok(());
            }
            latest
        }
    };

    if target == current {
        ui.println(format!(
            "    {} b {} is already installed",
            style("✓").green(),
            style(current).bold()
        ))
        .map_err(ui_error)?;
        return Ok(());
    }

    if check {
        ui.println(format!(
            "    b {} is available (you have {}). Run {} to update.",
            style(target).green().bold(),
            current,
            style("b self-update").bold()
        ))
        .map_err(ui_error)?;
        return Ok(());
    }

    let dir = self_update::install_dir()?;
    ui.heading(format!(
        "Updating b {} → {}...",
        current,
        style(target).green().bold()
    ))
    .map_err(ui_error)?;

    ui.step_start(format!("Downloading and verifying v{target}"))
        .map_err(ui_error)?;
    let binaries = match updater.download(target).await {
        Ok(binaries) => binaries,
        Err(err) => {
            ui.step_fail().map_err(ui_error)?;
            return Err(err);
        }
    };
    ui.step_ok().map_err(ui_error)?;

    ui.step_start(format!("Installing into {}", dir.display()))
        .map_err(ui_error)?;
    if let Err(err) = self_update::install_binaries(&dir, &binaries, target) {
        ui.step_fail().map_err(ui_error)?;
        return Err(err);
    }
    ui.step_ok().map_err(ui_error)?;

    ui.println(format!(
        "    {} Updated b to {}",
        style("✓").green(),
        style(target).bold()
    ))
    .map_err(ui_error)?;
    Ok(())
}

fn ui_error(err: std::io::Error) -> brew_core::Error {
    brew_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
