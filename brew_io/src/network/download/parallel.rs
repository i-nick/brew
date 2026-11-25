use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore, mpsc};

use crate::storage::blob::BlobCache;
use brew_core::Error;

use super::single::Downloader;
use super::{
    DEFAULT_DOWNLOAD_JOBS, DownloadProgressCallback, DownloadResult, GLOBAL_DOWNLOAD_CONCURRENCY,
};

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub sha256: String,
    pub name: String,
}

/// How one request passed to [`ParallelDownloader::download_each`] ended.
#[derive(Debug)]
pub struct DownloadOutcome {
    pub name: String,
    pub sha256: String,
    pub result: Result<PathBuf, Error>,
}

type InflightMap = HashMap<String, Arc<tokio::sync::broadcast::Sender<Result<PathBuf, String>>>>;

/// Downloads many bottles at once under two independent limits:
///
/// - **jobs**: how many packages download at the same time. A package holds
///   one job slot from start to finish.
/// - **connections**: how many HTTP connections are open across all packages.
///   Owned by the inner [`Downloader`], which takes one permit per connection
///   (one for a plain download, one per chunk for a large bottle).
///
/// The two must stay separate: if a package's slot and its chunks' permits
/// came from one semaphore, packages holding every permit would wait forever
/// for chunks that can never start.
pub struct ParallelDownloader {
    downloader: Arc<Downloader>,
    jobs: Arc<Semaphore>,
    job_limit: usize,
    inflight: Arc<Mutex<InflightMap>>,
}

impl ParallelDownloader {
    pub fn new(blob_cache: BlobCache) -> Self {
        Self::with_limits(
            blob_cache,
            DEFAULT_DOWNLOAD_JOBS,
            GLOBAL_DOWNLOAD_CONCURRENCY,
        )
    }

    /// Downloads at most `jobs` packages at a time over at most `connections`
    /// HTTP connections. Both limits are raised to 1 if given as 0.
    pub fn with_limits(blob_cache: BlobCache, jobs: usize, connections: usize) -> Self {
        let job_limit = jobs.max(1);
        let connections = Arc::new(Semaphore::new(connections.max(1)));
        Self {
            downloader: Arc::new(Downloader::with_semaphore(blob_cache, Some(connections))),
            jobs: Arc::new(Semaphore::new(job_limit)),
            job_limit,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Maximum number of packages downloaded at the same time.
    pub fn job_limit(&self) -> usize {
        self.job_limit
    }

    /// Whether a blob for `sha256` is already in the cache (not re-verified).
    pub fn has_blob(&self, sha256: &str) -> bool {
        self.downloader.has_blob(sha256)
    }

    pub fn remove_blob(&self, sha256: &str) -> bool {
        self.downloader.remove_blob(sha256)
    }

    pub async fn download_single(
        &self,
        request: DownloadRequest,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<PathBuf, Error> {
        self.download_task(request, progress).await
    }

    /// Downloads every request, at most [`job_limit`](Self::job_limit) at a
    /// time, and reports each one as it finishes. Unlike
    /// [`download_all`](Self::download_all), a failure is reported alongside
    /// the request's name and never stops the remaining downloads.
    pub fn download_each(
        &self,
        requests: Vec<DownloadRequest>,
        progress: Option<DownloadProgressCallback>,
    ) -> mpsc::Receiver<DownloadOutcome> {
        let (tx, rx) = mpsc::channel(requests.len().max(1));

        for req in requests {
            let tx = tx.clone();
            let name = req.name.clone();
            let sha256 = req.sha256.clone();
            let task = self.download_task(req, progress.clone());

            tokio::spawn(async move {
                let result = task.await;
                let _ = tx
                    .send(DownloadOutcome {
                        name,
                        sha256,
                        result,
                    })
                    .await;
            });
        }

        rx
    }

    pub async fn download_all(
        &self,
        requests: Vec<DownloadRequest>,
    ) -> Result<Vec<PathBuf>, Error> {
        self.download_all_with_progress(requests, None).await
    }

    pub async fn download_all_with_progress(
        &self,
        requests: Vec<DownloadRequest>,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<Vec<PathBuf>, Error> {
        let handles: Vec<_> = requests
            .into_iter()
            .map(|req| tokio::spawn(self.download_task(req, progress.clone())))
            .collect();

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            let result = handle.await.map_err(Error::network("task join error"))??;
            results.push(result);
        }

        Ok(results)
    }

    pub fn download_streaming(
        &self,
        requests: Vec<DownloadRequest>,
        progress: Option<DownloadProgressCallback>,
    ) -> mpsc::Receiver<Result<DownloadResult, Error>> {
        let (tx, rx) = mpsc::channel(requests.len().max(1));

        for (index, req) in requests.into_iter().enumerate() {
            let tx = tx.clone();
            let name = req.name.clone();
            let sha256 = req.sha256.clone();
            let task = self.download_task(req, progress.clone());

            tokio::spawn(async move {
                let result = task.await;
                let _ = tx
                    .send(result.map(|blob_path| DownloadResult {
                        name,
                        sha256,
                        blob_path,
                        index,
                    }))
                    .await;
            });
        }

        rx
    }

    /// An owned, spawnable download of `req` that shares this downloader's
    /// limits and in-flight deduplication.
    fn download_task(
        &self,
        req: DownloadRequest,
        progress: Option<DownloadProgressCallback>,
    ) -> impl Future<Output = Result<PathBuf, Error>> + Send + 'static {
        Self::download_with_dedup(
            self.downloader.clone(),
            self.jobs.clone(),
            self.inflight.clone(),
            req,
            progress,
        )
    }

    async fn download_with_dedup(
        downloader: Arc<Downloader>,
        jobs: Arc<Semaphore>,
        inflight: Arc<Mutex<InflightMap>>,
        req: DownloadRequest,
        progress: Option<DownloadProgressCallback>,
    ) -> Result<PathBuf, Error> {
        let mut receiver = {
            let mut map = inflight.lock().await;

            if let Some(sender) = map.get(&req.sha256) {
                Some(sender.subscribe())
            } else {
                let (tx, _) = tokio::sync::broadcast::channel(1);
                map.insert(req.sha256.clone(), Arc::new(tx));
                None
            }
        };

        if let Some(ref mut rx) = receiver {
            let result = rx
                .recv()
                .await
                .map_err(Error::network("broadcast recv error"))?;

            return result.map_err(|msg| Error::NetworkFailure { message: msg });
        }

        // Waiters for an in-flight blob (above) take no slot; only the task
        // actually downloading does, so duplicates never block real work.
        let _job = jobs
            .acquire()
            .await
            .map_err(Error::network("download job semaphore error"))?;

        let result = downloader
            .download_with_progress(&req.url, &req.sha256, Some(req.name), progress)
            .await;

        {
            let mut map = inflight.lock().await;
            if let Some(sender) = map.remove(&req.sha256) {
                let broadcast_result = match &result {
                    Ok(path) => Ok(path.clone()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = sender.send(broadcast_result);
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::storage::blob::BlobCache;

    use super::super::GLOBAL_DOWNLOAD_CONCURRENCY;
    use super::*;

    const SLOW_RESPONSE: Duration = Duration::from_millis(400);

    /// Mounts `count` small bottles that each take `SLOW_RESPONSE` to answer
    /// and returns requests for them.
    async fn mount_slow_files(server: &MockServer, count: usize) -> Vec<DownloadRequest> {
        let mut requests = Vec::with_capacity(count);
        for i in 0..count {
            let content = format!("bottle {i}").into_bytes();
            let route = format!("/slow{i}.tar.gz");
            requests.push(DownloadRequest {
                url: format!("{}{route}", server.uri()),
                sha256: sha256_hex(&content),
                name: format!("pkg{i}"),
            });
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(content)
                        .set_delay(SLOW_RESPONSE),
                )
                .mount(server)
                .await;
        }
        requests
    }

    async fn time_download_all(jobs: usize, count: usize) -> Duration {
        let mock_server = MockServer::start().await;
        let requests = mount_slow_files(&mock_server, count).await;
        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let downloader =
            ParallelDownloader::with_limits(blob_cache, jobs, GLOBAL_DOWNLOAD_CONCURRENCY);

        let start = std::time::Instant::now();
        let results = downloader.download_all(requests).await.unwrap();
        assert_eq!(results.len(), count);
        start.elapsed()
    }

    #[tokio::test]
    async fn job_limit_bounds_packages_downloading_at_once() {
        // 4 packages, 2 at a time: at least two back-to-back waves.
        let elapsed = time_download_all(2, 4).await;
        assert!(
            elapsed >= SLOW_RESPONSE * 2,
            "4 downloads with 2 jobs finished in {elapsed:?}; the job limit was not enforced"
        );
    }

    #[tokio::test]
    async fn packages_download_in_parallel_up_to_job_limit() {
        // 4 packages, 4 at a time: one wave, well under the serial time.
        let elapsed = time_download_all(4, 4).await;
        assert!(
            elapsed < SLOW_RESPONSE * 4,
            "4 downloads with 4 jobs took {elapsed:?}; they did not run in parallel"
        );
    }

    #[test]
    fn zero_limits_are_raised_to_one() {
        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let downloader = ParallelDownloader::with_limits(blob_cache, 0, 0);
        assert_eq!(downloader.job_limit(), 1);
    }

    #[tokio::test]
    async fn download_each_reports_failures_without_stopping_others() {
        let mock_server = MockServer::start().await;
        let mut requests = mount_slow_files(&mock_server, 2).await;
        requests.push(DownloadRequest {
            url: format!("{}/missing.tar.gz", mock_server.uri()),
            sha256: format!("{:064x}", 7),
            name: "missing".to_string(),
        });

        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let downloader = ParallelDownloader::with_limits(blob_cache, 2, 4);

        let mut rx = downloader.download_each(requests, None);
        let mut ok = Vec::new();
        let mut failed = Vec::new();
        while let Some(outcome) = rx.recv().await {
            match outcome.result {
                Ok(path) => {
                    assert!(path.exists());
                    ok.push(outcome.name);
                }
                Err(_) => failed.push(outcome.name),
            }
        }
        ok.sort();

        assert_eq!(ok, vec!["pkg0".to_string(), "pkg1".to_string()]);
        assert_eq!(failed, vec!["missing".to_string()]);
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Serves `content` at `route`, advertising and honouring byte ranges so
    /// files above the chunking threshold take the chunked-download path.
    async fn mount_ranged_file(server: &MockServer, route: &str, content: Vec<u8>) {
        Mock::given(method("HEAD"))
            .and(path(route.to_string()))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("Accept-Ranges", "bytes")
                    .append_header("Content-Length", content.len().to_string()),
            )
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path(route.to_string()))
            .respond_with(move |req: &wiremock::Request| {
                let Some(range) = req.headers.get("Range") else {
                    return ResponseTemplate::new(200).set_body_bytes(content.clone());
                };
                let range = range.to_str().unwrap().strip_prefix("bytes=").unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
                let chunk = &content[start..=end];
                ResponseTemplate::new(206)
                    .append_header("Content-Length", chunk.len().to_string())
                    .append_header(
                        "Content-Range",
                        format!("bytes {start}-{end}/{}", content.len()),
                    )
                    .set_body_bytes(chunk.to_vec())
            })
            .mount(server)
            .await;
    }

    /// Regression: the per-file slot and the per-chunk connection permits used
    /// to come from one semaphore, so once every permit was held by a file
    /// waiting on its chunks, no chunk could start and the download hung.
    #[tokio::test]
    async fn chunked_downloads_do_not_deadlock_when_connections_are_scarce() {
        let mock_server = MockServer::start().await;
        let mut requests = Vec::new();
        for i in 0..2u8 {
            let content = vec![i; 12 * 1024 * 1024];
            let route = format!("/large{i}.tar.gz");
            requests.push(DownloadRequest {
                url: format!("{}{route}", mock_server.uri()),
                sha256: sha256_hex(&content),
                name: format!("large{i}"),
            });
            mount_ranged_file(&mock_server, &route, content).await;
        }

        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let downloader = ParallelDownloader::with_limits(blob_cache, 2, 1);

        let results =
            tokio::time::timeout(Duration::from_secs(30), downloader.download_all(requests))
                .await
                .expect("download deadlocked")
                .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|path| path.exists()));
    }

    #[tokio::test]
    async fn same_blob_requested_multiple_times_fetches_once() {
        let mock_server = MockServer::start().await;
        let content = b"deduplicated content";

        let actual_sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(content);
            hex::encode(hasher.finalize())
        };

        Mock::given(method("GET"))
            .and(path("/dedup.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(content.to_vec())
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let tmp = TempDir::new().unwrap();
        let blob_cache = BlobCache::new(tmp.path()).unwrap();
        let downloader = ParallelDownloader::new(blob_cache);

        let requests: Vec<_> = (0..5)
            .map(|i| DownloadRequest {
                url: format!("{}/dedup.tar.gz", mock_server.uri()),
                sha256: actual_sha256.clone(),
                name: format!("dedup{i}"),
            })
            .collect();

        let results = downloader.download_all(requests).await.unwrap();

        assert_eq!(results.len(), 5);
        for path in &results {
            assert!(path.exists());
        }
    }
}
