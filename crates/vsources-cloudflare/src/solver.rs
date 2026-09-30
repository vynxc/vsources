//! The solver abstraction: pluggable Cloudflare bypass strategies.

use async_trait::async_trait;
use url::Url;
use vsources_core::FetchError;

use crate::clearance::Clearance;
use crate::detection::Challenge;

/// Errors produced by clearance solvers.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SolveError {
    /// The underlying HTTP request failed.
    #[error("fetch failed: {0}")]
    Fetch(#[from] FetchError),
    /// The solver ran and failed.
    #[error("solver {solver} failed: {message}")]
    Failed {
        /// The solver's name.
        solver: String,
        /// Why it failed.
        message: String,
    },
    /// The host is cooling down after a recent solver failure.
    #[error("host is cooling down after recent solver failures")]
    CoolingDown,
}

impl SolveError {
    /// A solver failure with a formatted message.
    pub fn failed(solver: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failed {
            solver: solver.into(),
            message: message.into(),
        }
    }
}

/// A Cloudflare bypass strategy.
///
/// Object-safe and `Send + Sync` so hosts can supply their own: a Tauri
/// desktop app can wrap a hidden `WebView` (the `CloudflareKiller`/`FaselHD`
/// approach), an Android app can wrap `android.webkit.WebView`, and
/// headless deployments use the bundled `FlareSolverr` client.
#[async_trait]
pub trait CloudflareSolver: Send + Sync {
    /// The solver's name, for logs and error messages.
    fn name(&self) -> &'static str;

    /// Whether this solver can attempt the given challenge kind.
    ///
    /// Availability checks belong in [`CloudflareSolver::solve`] so this
    /// stays a cheap, synchronous filter.
    fn can_solve(&self, challenge: Challenge) -> bool;

    /// Attempt to clear the challenge at `url`.
    ///
    /// The returned [`Clearance`] must carry the user agent it was earned
    /// with; the HTTP layer is responsible for replaying the pair.
    async fn solve(&self, url: &Url, challenge: Challenge) -> Result<Clearance, SolveError>;
}
