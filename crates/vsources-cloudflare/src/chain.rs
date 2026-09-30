//! A prioritized solver chain with clearance caching and failure cooldown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use url::Url;

use crate::clearance::Clearance;
use crate::cooldown::HostCooldown;
use crate::detection::Challenge;
use crate::solver::{CloudflareSolver, SolveError};

/// Clears challenges by trying solvers in order, caching what works.
///
/// Order is priority: the first solver that can handle the challenge
/// runs first. A successful clearance is cached per host until it
/// expires; a failed chain run cools the host down so the HTTP layer can
/// fail fast instead of re-solving on every request.
pub struct SolverChain {
    solvers: Vec<Arc<dyn CloudflareSolver>>,
    cooldown: HostCooldown,
    clearances: Mutex<HashMap<String, Clearance>>,
}

impl SolverChain {
    /// Build a chain that tries `solvers` in order.
    #[must_use]
    pub fn new(solvers: Vec<Arc<dyn CloudflareSolver>>) -> Self {
        Self {
            solvers,
            cooldown: HostCooldown::new(),
            clearances: Mutex::new(HashMap::new()),
        }
    }

    /// Use a custom failure cooldown.
    #[must_use]
    pub fn with_cooldown(mut self, cooldown: HostCooldown) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// A cached, unexpired clearance for the URL's host.
    ///
    /// Expired entries are dropped on read.
    #[must_use]
    pub fn cached_clearance(&self, url: &Url) -> Option<Clearance> {
        let host = url.host_str()?;
        let mut clearances = lock(&self.clearances);
        match clearances.get(host) {
            Some(clearance) if clearance.is_valid(Instant::now()) => Some(clearance.clone()),
            Some(_) => {
                let host = host.to_string();
                clearances.remove(&host);
                None
            }
            None => None,
        }
    }

    /// Solve a detected challenge.
    ///
    /// Returns the cached clearance when one is still valid, then tries
    /// each solver that accepts the challenge. A universal failure cools
    /// the host down before the error is returned.
    pub async fn solve(&self, url: &Url, challenge: Challenge) -> Result<Clearance, SolveError> {
        let Some(host) = url.host_str() else {
            return Err(SolveError::failed("chain", "the challenge URL has no host"));
        };

        if let Some(clearance) = self.cached_clearance(url) {
            return Ok(clearance);
        }
        if self.cooldown.is_in_cooldown(host) {
            return Err(SolveError::CoolingDown);
        }

        let mut last_error = None;
        for solver in &self.solvers {
            if !solver.can_solve(challenge) {
                continue;
            }
            match solver.solve(url, challenge).await {
                Ok(clearance) => {
                    lock(&self.clearances).insert(host.to_string(), clearance.clone());
                    return Ok(clearance);
                }
                Err(error) => {
                    tracing::warn!(
                        solver = solver.name(),
                        host,
                        "cloudflare solver failed: {error}"
                    );
                    last_error = Some(error);
                }
            }
        }

        // Every attempt failed (or no solver accepted the challenge).
        self.cooldown.cool_down(host);
        Err(last_error
            .unwrap_or_else(|| SolveError::failed("chain", "no solver accepted the challenge")))
    }

    /// Record an out-of-band failure (for example, a cached clearance
    /// that stopped working): cools the host down.
    pub fn record_failure(&self, url: &Url) {
        if let Some(host) = url.host_str() {
            self.cooldown.cool_down(host);
        }
    }

    /// Drop the cached clearance for the URL's host.
    pub fn invalidate(&self, url: &Url) {
        if let Some(host) = url.host_str() {
            lock(&self.clearances).remove(host);
        }
    }

    /// Forget every cached clearance.
    pub fn clear(&self) {
        lock(&self.clearances).clear();
    }
}

/// Lock a mutex, recovering from poisoning by keeping the inner value.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::solver::SolveError;

    /// A solver with scripted outcomes.
    struct MockSolver {
        name: &'static str,
        accepts: bool,
        result: Result<Clearance, SolveError>,
        /// Number of solve calls observed.
        calls: Mutex<u32>,
    }

    impl MockSolver {
        fn succeeding(name: &'static str) -> Self {
            Self {
                name,
                accepts: true,
                result: Ok(Clearance::new("token", None, "agent", "example.com")),
                calls: Mutex::new(0),
            }
        }

        fn failing(name: &'static str) -> Self {
            Self {
                name,
                accepts: true,
                result: Err(SolveError::failed(name, "scripted failure")),
                calls: Mutex::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            *lock(&self.calls)
        }
    }

    #[async_trait::async_trait]
    impl CloudflareSolver for MockSolver {
        fn name(&self) -> &'static str {
            self.name
        }

        fn can_solve(&self, challenge: Challenge) -> bool {
            self.accepts && challenge.is_solvable()
        }

        async fn solve(&self, _url: &Url, _challenge: Challenge) -> Result<Clearance, SolveError> {
            *lock(&self.calls) += 1;
            self.result.clone()
        }
    }

    fn url() -> Url {
        Url::parse("https://example.com/").unwrap_or_else(|_| panic!("valid URL"))
    }

    #[tokio::test]
    async fn solves_and_caches_clearances() {
        let primary = Arc::new(MockSolver::succeeding("primary"));
        let backup = Arc::new(MockSolver::succeeding("backup"));
        let chain = SolverChain::new(vec![primary.clone(), backup.clone()]);

        let first = chain
            .solve(&url(), Challenge::Interstitial)
            .await
            .unwrap_or_else(|e| panic!("the chain must solve: {e}"));
        assert_eq!(first.cf_clearance, "token");
        // The second solve is served from the cache; no solver runs.
        let second = chain
            .solve(&url(), Challenge::Interstitial)
            .await
            .unwrap_or_else(|e| panic!("the cached clearance must serve: {e}"));
        assert_eq!(second.cf_clearance, "token");
        assert_eq!(primary.call_count(), 1);
        assert_eq!(backup.call_count(), 0);
        assert!(chain.cached_clearance(&url()).is_some());
    }

    #[tokio::test]
    async fn falls_back_to_the_next_solver() {
        let failing = Arc::new(MockSolver::failing("failing"));
        let succeeding = Arc::new(MockSolver::succeeding("succeeding"));
        let chain = SolverChain::new(vec![failing.clone(), succeeding.clone()]);

        let clearance = chain
            .solve(&url(), Challenge::Interstitial)
            .await
            .unwrap_or_else(|e| panic!("the backup must solve: {e}"));
        assert_eq!(clearance.cf_clearance, "token");
        assert_eq!(failing.call_count(), 1);
        assert_eq!(succeeding.call_count(), 1);
    }

    #[tokio::test]
    async fn cools_down_after_universal_failure() {
        let failing = Arc::new(MockSolver::failing("failing"));
        let chain = SolverChain::new(vec![failing.clone()]);

        let first = chain.solve(&url(), Challenge::Interstitial).await;
        assert!(matches!(first, Err(SolveError::Failed { .. })));
        // Immediate retry fails fast without hitting the solver again.
        let second = chain.solve(&url(), Challenge::Interstitial).await;
        assert!(matches!(second, Err(SolveError::CoolingDown)));
        assert_eq!(failing.call_count(), 1);
    }

    #[tokio::test]
    async fn unsolvable_challenges_never_reach_solvers() {
        let solver = Arc::new(MockSolver::succeeding("solver"));
        let chain = SolverChain::new(vec![solver.clone()]);
        let result = chain.solve(&url(), Challenge::Censor).await;
        assert!(matches!(result, Err(SolveError::Failed { .. })));
        assert_eq!(solver.call_count(), 0);
    }

    #[tokio::test]
    async fn invalidation_forces_a_resolve() {
        let solver = Arc::new(MockSolver::succeeding("solver"));
        let chain = SolverChain::new(vec![solver.clone()]);

        let _ = chain
            .solve(&url(), Challenge::Interstitial)
            .await
            .unwrap_or_else(|e| panic!("the chain must solve: {e}"));
        chain.invalidate(&url());
        assert!(chain.cached_clearance(&url()).is_none());
        let _ = chain
            .solve(&url(), Challenge::Interstitial)
            .await
            .unwrap_or_else(|e| panic!("the chain must solve again: {e}"));
        assert_eq!(solver.call_count(), 2);
    }

    #[tokio::test]
    async fn expired_clearances_are_not_served() {
        let solver = Arc::new(MockSolver::succeeding("solver"));
        let chain = SolverChain::new(vec![solver]);
        let _ = chain
            .solve(&url(), Challenge::Interstitial)
            .await
            .unwrap_or_else(|e| panic!("the chain must solve: {e}"));
        // Expire the cached clearance directly (same module).
        lock(&chain.clearances)
            .values_mut()
            .for_each(|clearance| clearance.valid_for = Duration::ZERO);
        assert!(chain.cached_clearance(&url()).is_none());
    }
}
