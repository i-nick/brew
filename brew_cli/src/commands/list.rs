use console::style;

pub fn execute(database: &brew_io::Database, all: bool) -> Result<(), brew_core::Error> {
    let installed = if all {
        database.list_installed()?
    } else {
        database.list_requested_installed()?
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
