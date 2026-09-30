#![deny(missing_docs)]
//! Default [`Fetcher`](vsources_core::traits::Fetcher) implementation for
//! vsources, built on `wreq` (Chrome TLS/HTTP2/header impersonation — the
//! Rust equivalent of `got-scraping`).
//!
//! Ports `src/utils/Fetcher.js`: per-host request queueing, per-request
//! timeouts with host eviction, cookie persistence, 429 `Retry-After`
//! mapping, and Cloudflare integration — detected challenges can be
//! solved through a
//! [`SolverChain`](vsources_cloudflare::SolverChain) and the earned
//! clearance is replayed automatically, while unsolvable hosts fail fast
//! out of a negative cache.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use vsources_cloudflare::{FlareSolverr, FlareSolverrSolver, SolverChain};
//! use vsources_core::traits::Fetcher;
//! use vsources_net::ChromeFetcher;
//!
//! // Plain: Chrome-impersonating fetches with no solver.
//! let fetcher = ChromeFetcher::builder().build()?;
//!
//! // With a FlareSolverr escape hatch for challenged hosts.
//! let daemon = FlareSolverr::new(
//!     url::Url::parse("http://localhost:8191/")?,
//!     std::sync::Arc::new(ChromeFetcher::builder().build()?),
//! );
//! let chain = SolverChain::new(vec![Arc::new(FlareSolverrSolver::new(daemon))]);
//! let fetcher = ChromeFetcher::builder().cloudflare(chain).build()?;
//! # let _ = fetcher; Ok(())
//! # }
//! ```

pub mod blocked;
pub mod fetcher;
pub mod queue;
pub mod timeouts;

pub use blocked::BlockedHosts;
pub use fetcher::{ChromeFetcher, ChromeFetcherBuilder};
pub use queue::HostSemaphores;
pub use timeouts::TimeoutLedger;
