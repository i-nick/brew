use brew_io::InstalledKeg;
use chrono::{DateTime, Local};
use console::style;

pub fn execute(
    installer: &mut brew_io::Installer,
    formula: String,
) -> Result<(), brew_core::Error> {
    if let Some(keg) = installer.get_installed(&formula) {
        let fields = installed_keg_fields(&keg);
        print_field("Name:", style(&fields.name).bold());
        print_field("Version:", &fields.version);
        print_field("Store key:", &fields.store_key);
        print_field("Installed:", &fields.installed);
    } else {
        println!("Formula '{}' is not installed.", formula);
    }

    Ok(())
}

fn print_field(label: &str, value: impl std::fmt::Display) {
    println!("{:<10}  {}", style(label).dim(), value);
}

struct InstalledKegFields {
    name: String,
    version: String,
    store_key: String,
    installed: String,
}

fn installed_keg_fields(keg: &InstalledKeg) -> InstalledKegFields {
    InstalledKegFields {
        name: keg.name.clone(),
        version: keg.version.clone(),
        store_key: keg.store_key[..12].to_string(),
        installed: format_timestamp(keg.installed_at),
    }
}

fn format_timestamp(timestamp: i64) -> String {
    match DateTime::from_timestamp(timestamp, 0) {
        Some(dt) => {
            let local_dt = dt.with_timezone(&Local);
            let now = Local::now();
            let duration = now.signed_duration_since(local_dt);

            if duration.num_days() > 0 {
                format!(
                    "{} ({} days ago)",
                    local_dt.format("%Y-%m-%d"),
                    duration.num_days()
                )
            } else if duration.num_hours() > 0 {
                format!(
                    "{} ({} hours ago)",
                    local_dt.format("%Y-%m-%d %H:%M"),
                    duration.num_hours()
                )
            } else {
                format!(
                    "{} ({} minutes ago)",
                    local_dt.format("%H:%M"),
                    duration.num_minutes()
                )
            }
        }
        None => "invalid timestamp".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::installed_keg_fields;
    use brew_io::InstalledKeg;

    #[test]
    fn installed_keg_fields_preserves_full_tap_name() {
        let fields = installed_keg_fields(&InstalledKeg {
            name: "hashicorp/tap/terraform".to_string(),
            version: "1.10.0".to_string(),
            store_key: "deadbeefcafebabedeadbeefcafebabe".to_string(),
            installed_at: 1_700_000_000,
            requested: true,
            deps_recorded: true,
        });

        assert_eq!(fields.name, "hashicorp/tap/terraform");
        assert_eq!(fields.version, "1.10.0");
    }
}
