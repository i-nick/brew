use console::style;

pub fn execute(installer: &mut brew_io::Installer, all: bool) -> Result<(), brew_core::Error> {
    let installed = if all {
        installer.list_installed()?
    } else {
        installer.list_requested_installed()?
    };

    if installed.is_empty() {
        println!("No formulas installed.");
    } else {
        for keg in installed {
            println!("{} {}", style(&keg.name).bold(), style(&keg.version).dim());
        }
    }

    Ok(())
}
