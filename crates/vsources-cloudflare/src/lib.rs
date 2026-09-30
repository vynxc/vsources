#![deny(missing_docs)]
//! Cloudflare challenge detection and bypass architecture for vsources.
//!
//! Ships a `FlareSolverr` v3 client plus an object-safe [`CloudflareSolver`]
//! trait designed so that WebView-based solvers (CloudflareKiller-style)
//! can be implemented later in Tauri or Kotlin clients without touching
//! this crate's integration points.
//!
//! The pieces:
//!
//! - [`detection`] classifies a response into a [`Challenge`] kind.
//! - [`clearance`] is the solved state (cookies + the user agent that
//!   earned them, with a validity window).
//! - [`solver`] is the pluggable strategy; [`flaresolverr`] is the
//!   headless implementation.
//! - [`chain`] tries solvers in priority order, caching clearances and
//!   cooling hosts down after failures.
//! - [`cooldown`] is the shared host-scoped failure window.
//!
//! ```
//! # use std::sync::Arc;
//! # use vsources_core::traits::Fetcher;
//! # use vsources_cloudflare::{FlareSolverr, FlareSolverrSolver, SolverChain};
//! # fn demo(fetcher: Arc<dyn Fetcher>) {
//! let client = FlareSolverr::new(
//!     url::Url::parse("http://localhost:8191/").expect("valid URL"),
//!     fetcher,
//! );
//! let chain = SolverChain::new(vec![Arc::new(FlareSolverrSolver::new(client))]);
//! # }
//! ```

pub mod chain;
pub mod clearance;
pub mod cooldown;
pub mod detection;
pub mod flaresolverr;
pub mod solver;

pub use chain::SolverChain;
pub use clearance::{CF_BM_COOKIE, CF_CLEARANCE_COOKIE, Clearance};
pub use cooldown::HostCooldown;
pub use detection::{Challenge, detect};
pub use flaresolverr::{
    Command, Cookie, FlareSolverr, FlareSolverrError, FlareSolverrSolver, Proxy, Solution,
    SolveRequest, SolveResponse,
};
pub use solver::{CloudflareSolver, SolveError};
