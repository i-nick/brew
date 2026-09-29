use std::fs;
use std::path::{Path, PathBuf};

use brew_core::{BuildPlan, Error, formula_token};
use tracing::warn;

use crate::progress::InstallProgress;

use super::{Installer, LinkFormulaRequest, LinkOutcome, PlannedInstall, dependency_cellar_path};

impl Installer {
    pub(super) async fn install_from_source(
        &mut self,
        item: &PlannedInstall,
        build_plan: &BuildPlan,
        link: bool,
        report: &impl Fn(InstallProgress),
    ) -> Result<Option<LinkOutcome>, Error> {
        let install_name = &item.install_name;
        let formula_name = &item.formula.name;
        let version = item.formula.effective_version();
        let previous_install = self.db.get_installed(install_name);
        let previous_keg_path = previous_install.as_ref().map(|previous| {
            self.cellar
                .keg_path(formula_token(&previous.name), &previous.version)
        });

        let ruby_source_path =
            item.formula
                .ruby_source_path
                .as_deref()
                .ok_or_else(|| Error::ExecutionError {
                    message: format!("no ruby_source_path for formula '{formula_name}'"),
                })?;

        let cache_dir = self.prefix.join("tmp").join("rb_cache");
        let formula_rb_checksum = item
            .formula
            .ruby_source_checksum
            .as_ref()
            .map(|checksum| checksum.sha256.as_str());

        let formula_rb = self
            .api_client
            .fetch_formula_rb(ruby_source_path, &cache_dir, formula_rb_checksum)
            .await?;

        let mut installed_deps = std::collections::HashMap::new();
        for dep_name in build_plan
            .runtime_dependencies
            .iter()
            .chain(build_plan.build_dependencies.iter())
        {
            if let Some(keg) = self.db.get_installed(dep_name) {
                installed_deps.insert(
                    dep_name.clone(),
                    crate::build::DepInfo {
                        cellar_path: dependency_cellar_path(&self.cellar, &keg.name, &keg.version),
                        isolated_prefix: self
                            .is_isolated_linked(dep_name)
                            .ok()
                            .filter(|is_linked| *is_linked)
                            .map(|_| self.isolated_prefix(dep_name).display().to_string()),
                    },
                );
            }
        }

        let keg_path = self.cellar.keg_path(formula_name, &version);
        let previous_keg_backup =
            Self::backup_existing_source_keg(&keg_path, formula_name, &version)?;

        let executor = crate::build::BuildExecutor::new(self.prefix.clone());
        if let Err(build_err) = executor
            .execute(build_plan, &formula_rb, &installed_deps)
            .await
        {
            if let Some(backup_path) = previous_keg_backup.as_ref() {
                Self::restore_source_keg_from_backup(
                    &keg_path,
                    backup_path,
                    formula_name,
                    &version,
                )?;
            }
            return Err(build_err);
        }

        if let Some(backup_path) = previous_keg_backup.as_ref() {
            Self::remove_source_keg_backup(backup_path, formula_name, &version)?;
        }

        report(InstallProgress::UnpackCompleted {
            name: formula_name.clone(),
        });

        let store_key = format!("source:{formula_name}:{version}");

        if let Some(previous_keg_path) = previous_keg_path.as_ref() {
            let _ = self.linker.unlink_keg(previous_keg_path);
            let _ = self.linker.unlink_isolated(install_name);
        }

        let tx = self.db.transaction().inspect_err(|_| {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
        })?;

        if let Err(e) = tx.record_formula_install_with_dependencies(
            install_name,
            &version,
            &store_key,
            item.requested,
            &item.formula.dependencies,
        ) {
            drop(tx);
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
            return Err(e);
        }

        if let Err(e) = tx.commit() {
            Self::cleanup_materialized(&self.cellar, formula_name, &version);
            return Err(e);
        }

        if let Err(e) = self.linker.link_opt(&keg_path) {
            warn!(formula = %install_name, error = %e, "failed to create opt link");
        }

        let link_outcome = self.link_formula_keg(
            LinkFormulaRequest {
                install_name,
                formula: &item.formula,
                version: &version,
                keg_path: &keg_path,
                link,
                requested: item.requested,
            },
            report,
        )?;

        report(InstallProgress::InstallCompleted {
            name: formula_name.clone(),
        });

        if let Some(previous) = previous_install
            && previous.version != version
        {
            self.cellar
                .remove_keg(formula_token(&previous.name), &previous.version)?;
        }
        Ok(link_outcome)
    }

    fn backup_existing_source_keg(
        keg_path: &Path,
        formula_name: &str,
        version: &str,
    ) -> Result<Option<PathBuf>, Error> {
        if !keg_path.exists() {
            return Ok(None);
        }

        let backup_path = Self::source_keg_backup_path(keg_path);
        if backup_path.exists() {
            fs::remove_dir_all(&backup_path).map_err(|e| Error::StoreCorruption {
                message: format!(
                    "failed to remove stale source-build backup for '{}@{}': {}",
                    formula_name, version, e
                ),
            })?;
        }

        fs::rename(keg_path, &backup_path).map_err(|e| Error::StoreCorruption {
            message: format!(
                "failed to backup existing keg for '{}@{}': {}",
                formula_name, version, e
            ),
        })?;

        Ok(Some(backup_path))
    }

    fn restore_source_keg_from_backup(
        keg_path: &Path,
        backup_path: &Path,
        formula_name: &str,
        version: &str,
    ) -> Result<(), Error> {
        if keg_path.exists() {
            fs::remove_dir_all(keg_path).map_err(|e| Error::StoreCorruption {
                message: format!(
                    "failed to remove failed source-build output for '{}@{}': {}",
                    formula_name, version, e
                ),
            })?;
        }

        fs::rename(backup_path, keg_path).map_err(|e| Error::StoreCorruption {
            message: format!(
                "failed to restore previous keg for '{}@{}': {}",
                formula_name, version, e
            ),
        })
    }

    fn remove_source_keg_backup(
        backup_path: &Path,
        formula_name: &str,
        version: &str,
    ) -> Result<(), Error> {
        if !backup_path.exists() {
            return Ok(());
        }

        fs::remove_dir_all(backup_path).map_err(|e| Error::StoreCorruption {
            message: format!(
                "failed to remove source-build backup for '{}@{}': {}",
                formula_name, version, e
            ),
        })
    }

    fn source_keg_backup_path(keg_path: &Path) -> PathBuf {
        let backup_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = keg_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "keg".to_string());

        keg_path.with_file_name(format!("{name}.b-backup-{backup_suffix}"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::Installer;

    #[test]
    fn source_keg_backup_can_restore_previous_installation() {
        let tmp = TempDir::new().unwrap();
        let keg_path = tmp.path().join("cellar").join("example").join("1.0.0");
        fs::create_dir_all(&keg_path).unwrap();
        fs::write(keg_path.join("old.txt"), "old").unwrap();

        let backup = Installer::backup_existing_source_keg(&keg_path, "example", "1.0.0").unwrap();
        let backup = backup.expect("backup path should exist");

        assert!(!keg_path.exists());
        assert!(backup.exists());

        fs::create_dir_all(&keg_path).unwrap();
        fs::write(keg_path.join("new.txt"), "new").unwrap();

        Installer::restore_source_keg_from_backup(&keg_path, &backup, "example", "1.0.0").unwrap();

        assert!(keg_path.join("old.txt").exists());
        assert!(!keg_path.join("new.txt").exists());
        assert!(!backup.exists());
    }

    #[test]
    fn backup_existing_source_keg_returns_none_when_keg_is_missing() {
        let tmp = TempDir::new().unwrap();
        let missing_keg = tmp.path().join("cellar").join("example").join("1.0.0");

        let backup =
            Installer::backup_existing_source_keg(&missing_keg, "example", "1.0.0").unwrap();

        assert!(backup.is_none());
    }
}
