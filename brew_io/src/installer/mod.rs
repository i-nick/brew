mod cask;
pub mod install;

pub use install::doctor::{DiagnosticReport, RepairSummary};
pub use install::{
    CleanupCandidate, CleanupResult, ExecuteResult, InstallPlan, Installer, IsolatedLinkReason,
    LinkOutcome, OutdatedPackage, create_installer,
};
