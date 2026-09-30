//! Clearance cookies and their validity window.

use std::time::{Duration, Instant};

/// The `cf_clearance` cookie name.
pub const CF_CLEARANCE_COOKIE: &str = "cf_clearance";
/// The `__cf_bm` cookie name.
pub const CF_BM_COOKIE: &str = "__cf_bm";
/// How long a clearance stays valid: Cloudflare rotates `cf_clearance`
/// roughly every 15 minutes, so a conservative window avoids replaying
/// cookies the edge has already forgotten.
const DEFAULT_VALIDITY: Duration = Duration::from_mins(15);

/// A solved Cloudflare challenge: the clearance cookies plus the user
/// agent that earned them.
///
/// The pair must travel together — Cloudflare binds the clearance to the
/// requesting user agent, so replaying the cookie with a different one
/// fails the challenge again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clearance {
    /// The `cf_clearance` cookie value.
    pub cf_clearance: String,
    /// The `__cf_bm` cookie value, when the solver captured one.
    pub cf_bm: Option<String>,
    /// The user agent the challenge was solved with.
    pub user_agent: String,
    /// The host the clearance is valid for.
    pub host: String,
    /// When the clearance was obtained.
    pub obtained_at: Instant,
    /// How long the clearance stays valid.
    pub valid_for: Duration,
}

impl Clearance {
    /// Build a clearance for `host`.
    pub fn new(
        cf_clearance: impl Into<String>,
        cf_bm: Option<String>,
        user_agent: impl Into<String>,
        host: impl Into<String>,
    ) -> Self {
        Self {
            cf_clearance: cf_clearance.into(),
            cf_bm,
            user_agent: user_agent.into(),
            host: host.into(),
            obtained_at: Instant::now(),
            valid_for: DEFAULT_VALIDITY,
        }
    }

    /// Override the validity window.
    #[must_use]
    pub fn with_valid_for(mut self, valid_for: Duration) -> Self {
        self.valid_for = valid_for;
        self
    }

    /// Whether the clearance is still valid at `now`.
    #[must_use]
    pub fn is_valid(&self, now: Instant) -> bool {
        now.duration_since(self.obtained_at) < self.valid_for
    }

    /// The `Cookie` header value that replays this clearance.
    #[must_use]
    pub fn cookie_header(&self) -> String {
        let mut header = format!("{CF_CLEARANCE_COOKIE}={}", self.cf_clearance);
        if let Some(cf_bm) = &self.cf_bm {
            header.push_str("; ");
            header.push_str(CF_BM_COOKIE);
            header.push('=');
            header.push_str(cf_bm);
        }
        header
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearances_expire() {
        let clearance = Clearance::new("token", Some("bm".to_string()), "agent", "example.com");
        assert!(clearance.is_valid(Instant::now()));
        assert!(!clearance.is_valid(Instant::now() + clearance.valid_for + Duration::from_secs(1)));
    }

    #[test]
    fn renders_cookie_headers() {
        let with_bm = Clearance::new(
            "token",
            Some("bm-value".to_string()),
            "agent",
            "example.com",
        );
        assert_eq!(
            with_bm.cookie_header(),
            "cf_clearance=token; __cf_bm=bm-value"
        );
        let without_bm = Clearance::new("token", None, "agent", "example.com");
        assert_eq!(without_bm.cookie_header(), "cf_clearance=token");
    }
}
