use std::path::Path;

use brew_io::{Installer, TapName, TapRegistry, installed_tap_of};
use console::style;

use crate::ui::StdUi;

/// `b tap` lists tapped repositories; `b tap user/repo` adds one.
pub async fn execute_tap(
    root: &Path,
    installer: &Installer,
    name: Option<String>,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let mut registry = TapRegistry::load(root)?;

    let Some(name) = name else {
        for tap in registry.iter() {
            ui.println(&tap.name).map_err(ui_error)?;
        }
        return Ok(());
    };

    let tap = TapName::parse(&name)?;
    if tap.is_builtin() {
        ui.info(format!(
            "{} is built in and always available.",
            style(&tap).bold()
        ))
        .map_err(ui_error)?;
        return Ok(());
    }

    let refreshing = registry.get(&tap).is_some();
    ui.heading(format!(
        "{} {}...",
        if refreshing { "Updating" } else { "Tapping" },
        style(&tap).bold()
    ))
    .map_err(ui_error)?;

    let index = installer.api_client().fetch_tap_index(&tap).await?;
    let summary = format!(
        "{} ({} {}, {} {})",
        style(&tap).green().bold(),
        index.formulae.len(),
        plural(index.formulae.len(), "formula", "formulae"),
        index.casks.len(),
        plural(index.casks.len(), "cask", "casks"),
    );
    registry.insert(index);
    registry.save()?;

    ui.heading(format!(
        "{} {summary}",
        if refreshing { "Updated" } else { "Tapped" }
    ))
    .map_err(ui_error)?;
    Ok(())
}

/// `b untap user/repo...` removes taps. Refuses while packages installed from
/// the tap remain, unless `force` is set.
pub fn execute_untap(
    root: &Path,
    installer: &Installer,
    names: Vec<String>,
    force: bool,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let mut registry = TapRegistry::load(root)?;
    let installed = installer.list_installed()?;

    for name in names {
        let tap = TapName::parse(&name)?;
        if tap.is_builtin() {
            return Err(brew_core::Error::InvalidArgument {
                message: format!("{tap} is built in and cannot be untapped"),
            });
        }
        if registry.get(&tap).is_none() {
            return Err(brew_core::Error::InvalidArgument {
                message: format!("no tapped repository named '{tap}'"),
            });
        }

        let tap_str = tap.to_string();
        let from_tap: Vec<&str> = installed
            .iter()
            .filter(|keg| installed_tap_of(&keg.name).as_deref() == Some(tap_str.as_str()))
            .map(|keg| keg.name.as_str())
            .collect();
        if !from_tap.is_empty() && !force {
            return Err(brew_core::Error::InvalidArgument {
                message: format!(
                    "refusing to untap {tap} because it has installed packages: {}\n\
                    Uninstall them first, or pass --force to untap anyway.",
                    from_tap.join(", ")
                ),
            });
        }

        registry.remove(&tap);
        registry.save()?;
        ui.heading(format!("Untapped {}", style(&tap).bold()))
            .map_err(ui_error)?;
    }

    Ok(())
}

/// Re-reads the index of every tapped repository (used by `b update`).
/// Failures are reported as warnings so one broken tap doesn't block others.
pub async fn refresh_all(
    root: &Path,
    installer: &Installer,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let mut registry = TapRegistry::load(root)?;
    if registry.is_empty() {
        return Ok(());
    }

    let taps: Vec<String> = registry.iter().map(|tap| tap.name.clone()).collect();
    let mut refreshed = 0usize;
    for name in taps {
        let tap = TapName::parse(&name)?;
        match installer.api_client().fetch_tap_index(&tap).await {
            Ok(index) => {
                registry.insert(index);
                refreshed += 1;
            }
            Err(err) => ui
                .warn(format!(
                    "Failed to update tap {}: {err}",
                    style(&tap).bold()
                ))
                .map_err(ui_error)?,
        }
    }
    registry.save()?;

    ui.heading(format!(
        "Updated {} {}.",
        style(refreshed).green().bold(),
        plural(refreshed, "tap", "taps")
    ))
    .map_err(ui_error)
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

fn ui_error(err: std::io::Error) -> brew_core::Error {
    brew_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
