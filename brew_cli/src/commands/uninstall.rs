use crate::ui::StdUi;
use crate::utils::normalize_formula_name;
use console::style;

pub async fn execute(
    installer: &mut brew_io::Installer,
    formulas: Vec<String>,
    all: bool,
    cleanup: bool,
    keep_data: bool,
    ui: &mut StdUi,
) -> Result<(), brew_core::Error> {
    let formulas = if all {
        let installed = installer.list_installed()?;
        if installed.is_empty() {
            ui.info("No formulas installed.").map_err(ui_error)?;
            return Ok(());
        }
        installed.into_iter().map(|k| k.name).collect()
    } else {
        let mut normalized = Vec::with_capacity(formulas.len());
        for formula in formulas {
            normalized.push(normalize_formula_name(&formula)?);
        }
        normalized
    };

    ui.heading(format!(
        "Uninstalling {}...",
        style(formulas.join(", ")).bold()
    ))
    .map_err(ui_error)?;

    let mut errors: Vec<(String, brew_core::Error)> = Vec::new();

    if formulas.len() > 1 {
        for name in &formulas {
            ui.step_start(name).map_err(ui_error)?;
            match installer.uninstall(name, keep_data) {
                Ok(()) => ui.step_ok().map_err(ui_error)?,
                Err(e) => {
                    ui.step_fail().map_err(ui_error)?;
                    errors.push((name.clone(), e));
                }
            }
        }
    } else if let Err(e) = installer.uninstall(&formulas[0], keep_data) {
        errors.push((formulas[0].clone(), e));
    }

    if !errors.is_empty() {
        for (name, err) in &errors {
            ui.error(format!(
                "Failed to uninstall {}: {}",
                style(name).bold(),
                err
            ))
            .map_err(ui_error)?;
        }
        // Return just the first error up. TODO: don't return errors from this fn?
        return Err(errors.remove(0).1);
    }

    if cleanup {
        crate::commands::cleanup::run_cleanup(installer, ui)
            .await
            .map(|_| ())
    } else {
        Ok(())
    }
}

fn ui_error(err: std::io::Error) -> brew_core::Error {
    brew_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}
