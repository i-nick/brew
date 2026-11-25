//! Concurrent bottle downloads ahead of a one-package-at-a-time install.
//!
//! `upgrade` and `bundle` install packages one by one so each gets its own
//! status line, link decision and error. Left alone, that also serialises
//! their downloads. Prefetching fetches every bottle those installs will need
//! up front, at most [`Installer::download_jobs`] packages at a time, so each
//! install then finds its bottle already in the blob cache.

use std::collections::HashSet;

use brew_core::{Error, InstallMethod};
use tracing::debug;

use crate::network::download::{DownloadOutcome, DownloadProgressCallback, DownloadRequest};

use super::{InstallPlan, Installer};

/// A bottle that could not be prefetched. Nothing is lost: the install that
/// follows downloads it again and reports any error in context.
#[derive(Debug)]
pub struct PrefetchFailure {
    pub name: String,
    pub error: Error,
}

impl Installer {
    /// Maximum number of packages downloaded at the same time.
    pub fn download_jobs(&self) -> usize {
        self.downloader.job_limit()
    }

    /// The bottles that installing `names` would download and that are not in
    /// the blob cache yet, including dependencies, each listed once.
    ///
    /// Best effort: casks, source builds and names that fail to resolve are
    /// left out, since installing them reports the problem properly.
    pub async fn bottles_to_prefetch(&self, names: &[String]) -> Vec<DownloadRequest> {
        let formulas: Vec<String> = names
            .iter()
            .filter(|name| !name.starts_with("cask:"))
            .cloned()
            .collect();
        if formulas.is_empty() {
            return Vec::new();
        }

        // One plan for the whole batch resolves each shared dependency once.
        let plans = match self.plan_with_request_status(&formulas, false, false).await {
            Ok(plan) => vec![plan],
            Err(error) => {
                debug!(%error, "batch prefetch plan failed; planning each package separately");
                self.plan_each(&formulas).await
            }
        };

        let mut seen = HashSet::new();
        plans
            .into_iter()
            .flat_map(|plan| plan.items)
            .filter_map(|item| match item.method {
                InstallMethod::Bottle(bottle) => Some(DownloadRequest {
                    url: bottle.url,
                    sha256: bottle.sha256,
                    name: item.formula.name,
                }),
                InstallMethod::Source(_) => None,
            })
            .filter(|req| !self.downloader.has_blob(&req.sha256) && seen.insert(req.sha256.clone()))
            .collect()
    }

    /// Downloads `requests`, at most [`download_jobs`](Self::download_jobs)
    /// packages at a time, calling `on_done` as each one finishes. Returns the
    /// bottles that failed; the rest are in the blob cache.
    pub async fn prefetch_bottles(
        &self,
        requests: Vec<DownloadRequest>,
        progress: Option<DownloadProgressCallback>,
        mut on_done: impl FnMut(&DownloadOutcome),
    ) -> Vec<PrefetchFailure> {
        let mut rx = self.downloader.download_each(requests, progress);
        let mut failures = Vec::new();

        while let Some(outcome) = rx.recv().await {
            on_done(&outcome);
            if let Err(error) = outcome.result {
                debug!(name = %outcome.name, %error, "bottle prefetch failed");
                failures.push(PrefetchFailure {
                    name: outcome.name,
                    error,
                });
            }
        }

        failures
    }

    async fn plan_each(&self, formulas: &[String]) -> Vec<InstallPlan> {
        let mut plans = Vec::with_capacity(formulas.len());
        for name in formulas {
            match self
                .plan_with_request_status(std::slice::from_ref(name), false, false)
                .await
            {
                Ok(plan) => plans.push(plan),
                Err(error) => debug!(%name, %error, "skipping prefetch for package"),
            }
        }
        plans
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::test_support::*;

    /// Serves formula `name` depending on `deps`, plus its bottle. The bottle
    /// may be requested at most `bottle_fetches` times.
    async fn mount_formula(server: &MockServer, name: &str, deps: &[&str], bottle_fetches: u64) {
        let bottle = create_bottle_tarball(name);
        let sha = sha256_hex(&bottle);
        let tag = get_test_bottle_tag();
        let bottle_path = format!("/bottles/{name}-1.0.0.{tag}.bottle.tar.gz");
        let deps = deps
            .iter()
            .map(|dep| format!("\"{dep}\""))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"name":"{name}","versions":{{"stable":"1.0.0"}},"dependencies":[{deps}],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}{bottle_path}","sha256":"{sha}"}}}}}}}}}}"#,
            server.uri()
        );

        Mock::given(method("GET"))
            .and(path(format!("/formula/{name}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_string(json))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(bottle_path))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .expect(..=bottle_fetches)
            .mount(server)
            .await;
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    fn sorted_request_names(requests: &[crate::DownloadRequest]) -> Vec<String> {
        let mut names: Vec<_> = requests.iter().map(|req| req.name.clone()).collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn prefetch_fetches_shared_dependencies_once_and_installs_reuse_them() {
        let server = MockServer::start().await;
        mount_formula(&server, "alpha", &["shared"], 1).await;
        mount_formula(&server, "beta", &["shared"], 1).await;
        mount_formula(&server, "shared", &[], 1).await;

        let tmp = TempDir::new().unwrap();
        let mut installer = make_installer(
            &tmp.path().join("brew"),
            &tmp.path().join("homebrew"),
            &server,
        );
        let packages = names(&["alpha", "beta"]);

        let requests = installer.bottles_to_prefetch(&packages).await;
        assert_eq!(
            sorted_request_names(&requests),
            names(&["alpha", "beta", "shared"])
        );

        let mut finished = Vec::new();
        let failures = installer
            .prefetch_bottles(requests, None, |outcome| {
                finished.push(outcome.name.clone())
            })
            .await;
        assert!(failures.is_empty(), "unexpected failures: {failures:?}");
        assert_eq!(finished.len(), 3);

        // Everything is cached now: nothing left to prefetch, and installing
        // one package at a time downloads nothing more (`expect` on drop).
        assert!(installer.bottles_to_prefetch(&packages).await.is_empty());
        for package in &packages {
            installer
                .install(std::slice::from_ref(package), true)
                .await
                .unwrap();
        }
        assert!(installer.is_installed("alpha"));
        assert!(installer.is_installed("beta"));
    }

    #[tokio::test]
    async fn unresolvable_names_do_not_block_prefetching_the_rest() {
        let server = MockServer::start().await;
        mount_formula(&server, "alpha", &[], 1).await;
        Mock::given(method("GET"))
            .and(path("/formula/ghost.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let installer = make_installer(
            &tmp.path().join("brew"),
            &tmp.path().join("homebrew"),
            &server,
        );

        let requests = installer
            .bottles_to_prefetch(&names(&["alpha", "ghost", "cask:zed"]))
            .await;
        assert_eq!(sorted_request_names(&requests), names(&["alpha"]));
    }

    #[tokio::test]
    async fn failed_downloads_are_reported_by_name_without_stopping_others() {
        let server = MockServer::start().await;
        mount_formula(&server, "alpha", &[], 1).await;

        let tag = get_test_bottle_tag();
        let broken_json = format!(
            r#"{{"name":"broken","versions":{{"stable":"1.0.0"}},"dependencies":[],"bottle":{{"stable":{{"files":{{"{tag}":{{"url":"{}/bottles/broken.tar.gz","sha256":"{:064x}"}}}}}}}}}}"#,
            server.uri(),
            1
        );
        Mock::given(method("GET"))
            .and(path("/formula/broken.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(broken_json))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/bottles/broken.tar.gz"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let tmp = TempDir::new().unwrap();
        let installer = make_installer(
            &tmp.path().join("brew"),
            &tmp.path().join("homebrew"),
            &server,
        );

        let requests = installer
            .bottles_to_prefetch(&names(&["alpha", "broken"]))
            .await;
        let failures = installer.prefetch_bottles(requests, None, |_| {}).await;

        let failed: Vec<_> = failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(failed, vec!["broken"]);
        assert!(
            installer
                .bottles_to_prefetch(&names(&["alpha"]))
                .await
                .is_empty()
        );
    }
}
