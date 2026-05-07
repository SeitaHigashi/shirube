/// Auto-update module: checks GitHub Releases for a newer version and
/// atomically replaces the running binary. Re-exec is handled by the caller.
use anyhow::Context;
use self_update::backends::github::Update;
use self_update::Status;
use tracing::{debug, info, warn};

/// Handles self-update from GitHub Releases.
///
/// Uses the `self_update` crate for atomic binary replacement.
/// After `check_and_update()` returns `Status::Updated`, the caller
/// is responsible for re-exec'ing the new binary.
pub struct AutoUpdater {
    /// GitHub repository in "owner/repo" format (e.g. "seita/shirube")
    owner: &'static str,
    repo: &'static str,
}

impl AutoUpdater {
    /// Create a new AutoUpdater targeting the given GitHub repository.
    pub fn new(owner: &'static str, repo: &'static str) -> Self {
        Self { owner, repo }
    }

    /// Check GitHub releases/latest and apply the update if a newer version exists.
    ///
    /// Returns `Status::Updated(version)` when the binary was replaced,
    /// or `Status::UpToDate(version)` when already on the latest version.
    /// This function performs synchronous I/O — run it inside `spawn_blocking`.
    pub fn check_and_update(&self) -> anyhow::Result<Status> {
        let current = env!("CARGO_PKG_VERSION");
        debug!(current, owner = self.owner, repo = self.repo, "checking for update");

        let mut builder = Update::configure();
        builder
            .repo_owner(self.owner)
            .repo_name(self.repo)
            .bin_name("shirube")
            .show_download_progress(false)
            .current_version(current);

        // Optional GitHub token to raise API rate limit (60 → 5000 req/hr)
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            builder.auth_token(&token);
        }

        let status = builder
            .build()
            .context("failed to build self_update")?
            .update()
            .context("self_update failed")?;

        match &status {
            Status::Updated(v) => info!(version = %v, "updated shirube binary"),
            Status::UpToDate(v) => debug!(version = %v, "shirube is up to date"),
        }

        Ok(status)
    }
}

/// Spawn a background tokio task that checks for updates every `interval_secs`.
///
/// On `Status::Updated`, re-execs the process with the same arguments so the
/// new binary takes over transparently.
pub fn spawn_update_loop(
    owner: &'static str,
    repo: &'static str,
    interval_secs: u64,
) {
    tokio::spawn(async move {
        loop {
            // Sleep first so that the process is fully initialised before the
            // first check, and to avoid hammering the GitHub API on every boot.
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;

            // self_update uses synchronous I/O — run it off the async executor.
            let result = tokio::task::spawn_blocking({
                let updater = AutoUpdater::new(owner, repo);
                move || updater.check_and_update()
            })
            .await;

            match result {
                Ok(Ok(Status::Updated(v))) => {
                    info!("updated to v{v} — re-execing");
                    re_exec();
                }
                Ok(Ok(Status::UpToDate(_))) => {} // already logged at debug level
                Ok(Err(e)) => warn!("auto-update check failed: {e:#}"),
                Err(e) => warn!("auto-update task panicked: {e}"),
            }
        }
    });
}

/// Replace the current process with the (possibly updated) binary at the same path.
///
/// This function does not return on success. On failure it logs the error and
/// returns so the existing process continues running.
fn re_exec() {
    use std::os::unix::process::CommandExt;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!("re-exec: could not resolve current exe: {e}");
            return;
        }
    };

    // Pass through all original arguments (skip argv[0] which is the binary path)
    let err = std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .exec(); // replaces the current process; only returns on error

    warn!("re-exec failed: {err}");
}
