#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("brew only supports macOS on Apple Silicon (aarch64-apple-darwin)");

pub mod build;
pub mod cellar;
pub(crate) mod checksum;
pub mod extraction;
pub mod installer;
pub mod network;
pub mod path;
pub mod progress;
pub mod ssl;
pub mod storage;

pub use build::{BuildExecutor, DepInfo};
pub use cellar::{Cellar, LinkedFile, Linker, MaterializedKeg};
pub use extraction::extract_tarball;
pub use installer::{
    CleanupCandidate, CleanupResult, DiagnosticReport, ExecuteResult, InstallPlan, Installer,
    IsolatedLinkReason, LinkOutcome, OutdatedPackage, RepairSummary, create_installer,
};
pub use network::{
    ApiCache, ApiClient, DownloadProgressCallback, DownloadRequest, Downloader, PackageSearchKind,
    PackageSearchResult, ParallelDownloader, create_api_client_with_cache,
    create_api_client_with_optional_cache,
};
pub use path::validate_privileged_path;
pub use progress::{InstallProgress, ProgressCallback};
pub use ssl::{find_ca_bundle_from_prefix, find_ca_dir};
pub use storage::{BlobCache, Database, InstalledKeg, KegFileRecord, Store, StoreRef};
