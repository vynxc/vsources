//! `FlareSolverr` v3 client and the solver adapter over it.
//!
//! Ports `IReader`'s `FlareSolverrClient`: a proxy server that solves
//! Cloudflare challenges headlessly (<https://github.com/FlareSolverr/FlareSolverr>).
//! Talks to the daemon over the same [`Fetcher`] abstraction the rest of
//! the SDK uses, so embedders can swap the HTTP layer wholesale.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;
use vsources_core::FetchError;
use vsources_core::traits::{FetchRequest, Fetcher};

use crate::clearance::{CF_BM_COOKIE, CF_CLEARANCE_COOKIE, Clearance};
use crate::detection::Challenge;
use crate::solver::{CloudflareSolver, SolveError};

/// Errors from talking to a `FlareSolverr` daemon.
#[derive(Debug, thiserror::Error)]
pub enum FlareSolverrError {
    /// The HTTP request to the daemon failed.
    #[error("fetch failed: {0}")]
    Fetch(#[from] FetchError),
    /// The daemon answered with an error status.
    #[error("FlareSolverr error: {0}")]
    Api(String),
    /// The daemon's response could not be serialized or parsed.
    #[error("FlareSolverr response was malformed: {0}")]
    Malformed(String),
}

/// Default overall timeout for a solve request (60 s, like upstream).
const DEFAULT_SOLVE_TIMEOUT: Duration = Duration::from_secs(60);

/// `Duration::as_millis` as `u64`, saturating at `u64::MAX` instead of
/// truncating on absurdly large timeouts.
fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
/// Default HTTP timeout for daemon round-trips.
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(75);

/// The command `FlareSolverr` should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `request.get` — fetch a URL.
    Get,
    /// `request.post` — fetch a URL with a form body.
    Post,
}

impl Command {
    /// The command name in the daemon's protocol.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "request.get",
            Self::Post => "request.post",
        }
    }
}

impl Serialize for Command {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// One cookie exchanged with the daemon (`FlareSolverr`'s schema).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Cookie domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Cookie path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Expiry (epoch seconds, float like the daemon's schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    /// Value length, when the daemon reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `HttpOnly` flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    /// Secure flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    /// Session cookie flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<bool>,
    /// `SameSite` policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

impl Cookie {
    /// A cookie with just a name and a value.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            domain: None,
            path: None,
            expires: None,
            size: None,
            http_only: None,
            secure: None,
            session: None,
            same_site: None,
        }
    }
}

/// An upstream proxy for the daemon to route through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proxy {
    /// Proxy URL (`http://user:pass@host:port`).
    pub url: String,
}

/// A solve request in the daemon's protocol.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolveRequest {
    /// The command to run.
    pub cmd: Command,
    /// The URL to fetch and clear.
    pub url: String,
    /// Overall timeout for the solve, in milliseconds.
    pub max_timeout: u64,
    /// An existing session id to reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Cookies to seed the browser context with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cookies: Vec<Cookie>,
    /// Return only the cookies, skipping page HTML.
    #[serde(default)]
    pub return_only_cookies: bool,
    /// An upstream proxy to route through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<Proxy>,
    /// Form body for `request.post`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_data: Option<String>,
}

impl SolveRequest {
    /// A `request.get` for `url`.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            cmd: Command::Get,
            url: url.into(),
            max_timeout: millis_u64(DEFAULT_SOLVE_TIMEOUT),
            session: None,
            cookies: Vec::new(),
            return_only_cookies: false,
            proxy: None,
            post_data: None,
        }
    }

    /// A `request.post` for `url` with a form body.
    pub fn post(url: impl Into<String>, post_data: impl Into<String>) -> Self {
        Self {
            cmd: Command::Post,
            post_data: Some(post_data.into()),
            ..Self::get(url)
        }
    }

    /// Reuse a daemon session.
    #[must_use]
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Set the overall solve timeout in milliseconds.
    #[must_use]
    pub fn with_max_timeout(mut self, max_timeout_ms: u64) -> Self {
        self.max_timeout = max_timeout_ms;
        self
    }

    /// Skip returning page HTML.
    #[must_use]
    pub fn with_return_only_cookies(mut self) -> Self {
        self.return_only_cookies = true;
        self
    }

    /// Route the daemon through a proxy.
    #[must_use]
    pub fn with_proxy(mut self, proxy: impl Into<String>) -> Self {
        self.proxy = Some(Proxy { url: proxy.into() });
        self
    }

    /// Seed a cookie into the browser context.
    #[must_use]
    pub fn with_cookie(mut self, cookie: Cookie) -> Self {
        self.cookies.push(cookie);
        self
    }
}

/// The page the daemon fetched after clearing the challenge.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Solution {
    /// The final URL after redirects.
    pub url: String,
    /// The final HTTP status.
    pub status: u16,
    /// Response headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The page body (absent when `returnOnlyCookies` was set).
    #[serde(default)]
    pub response: Option<String>,
    /// The cookies earned, including `cf_clearance`.
    #[serde(default)]
    pub cookies: Vec<Cookie>,
    /// The user agent the page was fetched with.
    pub user_agent: String,
}

impl Solution {
    /// The named cookie's value, when present.
    #[must_use]
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|cookie| cookie.name == name)
            .map(|cookie| cookie.value.as_str())
    }
}

/// The daemon's envelope response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SolveResponse {
    /// `ok` or `error`.
    pub status: String,
    /// Failure detail, when the status is `error`.
    #[serde(default)]
    pub message: String,
    /// When the daemon started.
    #[serde(default)]
    pub start_timestamp: Option<u64>,
    /// When the daemon finished.
    #[serde(default)]
    pub end_timestamp: Option<u64>,
    /// The daemon's version.
    #[serde(default)]
    pub version: Option<String>,
    /// The solution, on success.
    #[serde(default)]
    pub solution: Option<Solution>,
}

impl SolveResponse {
    /// Whether the daemon reports success.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    /// How long the solve took, when timestamps are present.
    #[must_use]
    pub fn duration_ms(&self) -> Option<u64> {
        Some(self.end_timestamp? - self.start_timestamp?)
    }
}

/// A client for a `FlareSolverr` daemon.
#[derive(Clone)]
pub struct FlareSolverr {
    base: Url,
    fetcher: Arc<dyn Fetcher>,
    http_timeout: Duration,
}

impl FlareSolverr {
    /// Create a client for the daemon at `base`.
    ///
    /// `base` is the daemon root (e.g. `http://localhost:8191`); the client
    /// joins `/v1` and `/health` onto it.
    #[must_use]
    pub fn new(base: Url, fetcher: Arc<dyn Fetcher>) -> Self {
        Self {
            base,
            fetcher,
            http_timeout: DEFAULT_HTTP_TIMEOUT,
        }
    }

    /// Create a client from the `FLARESOLVERR_URL` environment variable.
    ///
    /// Returns `None` when the variable is unset or invalid.
    #[must_use]
    pub fn from_env(fetcher: Arc<dyn Fetcher>) -> Option<Self> {
        let base = std::env::var("FLARESOLVERR_URL").ok()?;
        let base = Url::parse(&base).ok()?;
        Some(Self::new(base, fetcher))
    }

    /// Override the HTTP timeout for daemon round-trips.
    #[must_use]
    pub fn with_http_timeout(mut self, timeout: Duration) -> Self {
        self.http_timeout = timeout;
        self
    }

    /// Run a solve request.
    pub async fn solve(&self, request: &SolveRequest) -> Result<SolveResponse, FlareSolverrError> {
        let body = serde_json::to_string(request)
            .map_err(|err| FlareSolverrError::Malformed(err.to_string()))?;
        let response: SolveResponse = self.post_json(&body).await?;
        if !response.is_ok() {
            return Err(FlareSolverrError::Api(response.message));
        }
        Ok(response)
    }

    /// Create a persistent daemon session.
    pub async fn create_session(&self, session: &str) -> Result<(), FlareSolverrError> {
        self.session_command("sessions.create", session).await
    }

    /// Destroy a persistent daemon session.
    pub async fn destroy_session(&self, session: &str) -> Result<(), FlareSolverrError> {
        self.session_command("sessions.destroy", session).await
    }

    /// List the daemon's active sessions.
    pub async fn list_sessions(&self) -> Result<Vec<String>, FlareSolverrError> {
        let body = r#"{"cmd":"sessions.list"}"#;
        let response: SessionsResponse = self.post_json(body).await?;
        if response.status != "ok" {
            return Err(FlareSolverrError::Api(response.message));
        }
        Ok(response.sessions)
    }

    /// Whether the daemon answers its health check.
    pub async fn is_available(&self) -> bool {
        let Ok(url) = self.endpoint("health") else {
            return false;
        };
        match self
            .fetcher
            .request(FetchRequest::get(url).with_timeout(self.http_timeout))
            .await
        {
            Ok(response) => response.is_success(),
            Err(_) => false,
        }
    }

    /// The daemon's reported version, when available.
    pub async fn version(&self) -> Option<String> {
        let url = self.endpoint("health").ok()?;
        let response = self
            .fetcher
            .request(FetchRequest::get(url).with_timeout(self.http_timeout))
            .await
            .ok()?;
        response.json::<HealthResponse>().ok()?.version
    }

    /// POST a JSON command to the daemon's `/v1` endpoint.
    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        body: &str,
    ) -> Result<T, FlareSolverrError> {
        let url = self.endpoint("v1")?;
        let request = FetchRequest::post(url, body)
            .with_header("Content-Type", "application/json")
            .with_timeout(self.http_timeout);
        let response = self.fetcher.request(request).await?;
        response.json::<T>().map_err(FlareSolverrError::Fetch)
    }

    /// Run a session lifecycle command.
    async fn session_command(&self, cmd: &str, session: &str) -> Result<(), FlareSolverrError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct SessionCommand<'a> {
            cmd: &'a str,
            session: &'a str,
        }
        let command = SessionCommand { cmd, session };
        let body = serde_json::to_string(&command)
            .map_err(|err| FlareSolverrError::Malformed(err.to_string()))?;
        let response: SolveResponse = self.post_json(&body).await?;
        if !response.is_ok() {
            return Err(FlareSolverrError::Api(response.message));
        }
        Ok(())
    }

    /// Join `path` onto the daemon base.
    fn endpoint(&self, path: &str) -> Result<Url, FlareSolverrError> {
        self.base
            .join(path)
            .map_err(|_| FlareSolverrError::Malformed(format!("invalid daemon URL for {path:?}")))
    }
}

/// Wrap a JSON decode failure as a malformed-response error.
struct FetchSolverError(FetchError);

impl From<FetchSolverError> for FlareSolverrError {
    fn from(error: FetchSolverError) -> Self {
        Self::Fetch(error.0)
    }
}

/// The daemon's `/health` payload.
#[derive(Debug, Deserialize)]
struct HealthResponse {
    #[serde(default)]
    version: Option<String>,
}

/// The `sessions.list` payload.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionsResponse {
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    sessions: Vec<String>,
}

/// A [`CloudflareSolver`] backed by a `FlareSolverr` daemon.
///
/// Ports `IReader`'s `FlareSolverrStrategy`: asks the daemon for cookies
/// only, extracts `cf_clearance`, and pairs it with the daemon's user
/// agent.
pub struct FlareSolverrSolver {
    client: FlareSolverr,
    max_timeout: Duration,
}

impl FlareSolverrSolver {
    /// Wrap a daemon client.
    #[must_use]
    pub fn new(client: FlareSolverr) -> Self {
        Self {
            client,
            max_timeout: DEFAULT_SOLVE_TIMEOUT,
        }
    }

    /// Override the per-solve timeout.
    #[must_use]
    pub fn with_max_timeout(mut self, max_timeout: Duration) -> Self {
        self.max_timeout = max_timeout;
        self
    }
}

#[async_trait]
impl CloudflareSolver for FlareSolverrSolver {
    fn name(&self) -> &'static str {
        "flaresolverr"
    }

    fn can_solve(&self, challenge: Challenge) -> bool {
        // FlareSolverr cannot fix IP blocks or censorship; it can retry
        // for them but the daemon check happens in `solve`.
        challenge.is_solvable()
    }

    async fn solve(&self, url: &Url, _challenge: Challenge) -> Result<Clearance, SolveError> {
        let solver = self.name();
        if !self.client.is_available().await {
            return Err(SolveError::failed(solver, "FlareSolverr is not available"));
        }
        let request = SolveRequest::get(url.as_str())
            .with_max_timeout(millis_u64(self.max_timeout))
            .with_return_only_cookies();
        let response = self
            .client
            .solve(&request)
            .await
            .map_err(|err| SolveError::failed(solver, err.to_string()))?;
        let Some(solution) = response.solution else {
            return Err(SolveError::failed(
                solver,
                "FlareSolverr returned no solution",
            ));
        };
        let Some(cf_clearance) = solution.cookie(CF_CLEARANCE_COOKIE) else {
            return Err(SolveError::failed(
                solver,
                "FlareSolverr solved the challenge but returned no cf_clearance cookie",
            ));
        };
        let host = url
            .host_str()
            .ok_or_else(|| SolveError::failed(solver, "the challenge URL has no host"))?;
        Ok(Clearance::new(
            cf_clearance,
            solution.cookie(CF_BM_COOKIE).map(str::to_string),
            solution.user_agent.clone(),
            host,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use vsources_core::traits::FetchResponse;

    /// A fetcher that routes the daemon's endpoints.
    struct MockDaemon {
        /// Serve this payload for `/v1`.
        solve_body: &'static str,
        /// Serve this payload for `/health`.
        health_body: &'static str,
        /// HTTP status for `/health`.
        health_status: u16,
    }

    impl MockDaemon {
        const SOLVED: &'static str = r#"{
            "status": "ok",
            "message": "Challenge solved!",
            "startTimestamp": 1000,
            "endTimestamp": 2500,
            "version": "v3.3.21",
            "solution": {
                "url": "https://example.com/",
                "status": 200,
                "cookies": [
                    {"name": "cf_clearance", "value": "clear-token", "domain": ".example.com"},
                    {"name": "__cf_bm", "value": "bm-token"}
                ],
                "userAgent": "Mozilla/5.0 (X11; Linux x86_64) Chrome/126"
            }
        }"#;

        const ERRORED: &'static str = r#"{
            "status": "error",
            "message": "Error: Failed to solve the challenge",
            "startTimestamp": 1000,
            "endTimestamp": 3000,
            "version": "v3.3.21"
        }"#;

        const SESSIONS: &'static str = r#"{
            "status": "ok",
            "message": "",
            "sessions": ["session-a", "session-b"]
        }"#;
    }

    #[async_trait::async_trait]
    impl Fetcher for MockDaemon {
        async fn request(&self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
            let path = request.url.path();
            let (status, body) = match path {
                "/v1" => (200, self.solve_body),
                "/health" => (self.health_status, self.health_body),
                _ => (404, ""),
            };
            Ok(FetchResponse {
                url: request.url,
                status,
                headers: BTreeMap::new(),
                body: body.to_string(),
            })
        }
    }

    fn client(solve_body: &'static str) -> FlareSolverr {
        FlareSolverr::new(
            Url::parse("http://localhost:8191/").unwrap_or_else(|_| panic!("valid URL")),
            Arc::new(MockDaemon {
                solve_body,
                health_body: r#"{"status":"ok","version":"v3.3.21"}"#,
                health_status: 200,
            }),
        )
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|_| panic!("{s} is a valid URL"))
    }

    #[tokio::test]
    async fn solves_challenges_into_clearances() {
        let solver = FlareSolverrSolver::new(client(MockDaemon::SOLVED));
        let result = solver
            .solve(&url("https://example.com/"), Challenge::Interstitial)
            .await;
        let clearance = result.unwrap_or_else(|e| panic!("the solve must succeed: {e}"));
        assert_eq!(clearance.cf_clearance, "clear-token");
        assert_eq!(clearance.cf_bm.as_deref(), Some("bm-token"));
        assert_eq!(clearance.host, "example.com");
        assert!(
            clearance
                .cookie_header()
                .contains("cf_clearance=clear-token")
        );
        assert_eq!(
            clearance.user_agent,
            "Mozilla/5.0 (X11; Linux x86_64) Chrome/126"
        );
    }

    #[tokio::test]
    async fn surfaces_api_errors() {
        let solver = FlareSolverrSolver::new(client(MockDaemon::ERRORED));
        let result = solver
            .solve(&url("https://example.com/"), Challenge::Interstitial)
            .await;
        assert!(matches!(result, Err(SolveError::Failed { .. })));
    }

    #[tokio::test]
    async fn rejects_solutions_without_clearance() {
        let no_clearance = r#"{
            "status": "ok",
            "solution": {
                "url": "https://example.com/",
                "status": 200,
                "cookies": [],
                "userAgent": "agent"
            }
        }"#;
        let solver = FlareSolverrSolver::new(client(no_clearance));
        let result = solver
            .solve(&url("https://example.com/"), Challenge::Interstitial)
            .await;
        assert!(matches!(result, Err(SolveError::Failed { .. })));
    }

    #[tokio::test]
    async fn reports_daemon_unavailability() {
        let solver = FlareSolverrSolver::new(FlareSolverr::new(
            url("http://localhost:8191/"),
            Arc::new(MockDaemon {
                solve_body: MockDaemon::SOLVED,
                health_body: "",
                health_status: 503,
            }),
        ));
        let result = solver
            .solve(&url("https://example.com/"), Challenge::Interstitial)
            .await;
        let Err(SolveError::Failed { message, .. }) = result else {
            panic!("an unavailable daemon must fail the solve");
        };
        assert!(message.contains("not available"));
    }

    #[tokio::test]
    async fn session_lifecycle_and_listing() {
        let client = client(MockDaemon::SESSIONS);
        let sessions = client
            .list_sessions()
            .await
            .unwrap_or_else(|e| panic!("listing must succeed: {e}"));
        assert_eq!(sessions, ["session-a", "session-b"]);
        let version = client.version().await;
        assert_eq!(version.as_deref(), Some("v3.3.21"));
        assert!(client.is_available().await);
    }

    #[tokio::test]
    async fn solve_durations_come_from_the_daemon() {
        let response = client(MockDaemon::SOLVED)
            .solve(&SolveRequest::get("https://example.com/"))
            .await
            .unwrap_or_else(|e| panic!("the solve must succeed: {e}"));
        assert!(response.is_ok());
        assert_eq!(response.duration_ms(), Some(1500));
        assert_eq!(response.version.as_deref(), Some("v3.3.21"));
    }

    #[test]
    fn requests_serialize_to_the_daemon_protocol() {
        let request = SolveRequest::post("https://example.com/", "a=1")
            .with_session("s1")
            .with_max_timeout(1234)
            .with_return_only_cookies()
            .with_proxy("http://proxy:8080")
            .with_cookie(Cookie::new("seed", "x"));
        let body = serde_json::to_string(&request)
            .unwrap_or_else(|e| panic!("the request must serialize: {e}"));
        assert!(body.contains(r#""cmd":"request.post""#));
        assert!(body.contains(r#""url":"https://example.com/""#));
        assert!(body.contains(r#""maxTimeout":1234"#));
        assert!(body.contains(r#""session":"s1""#));
        assert!(body.contains(r#""returnOnlyCookies":true"#));
        assert!(body.contains(r#""proxy":{"url":"http://proxy:8080"}"#));
        assert!(body.contains(r#""postData":"a=1""#));
        assert!(body.contains(r#""name":"seed""#));
        // GET requests leave optional fields out entirely.
        let body = serde_json::to_string(&SolveRequest::get("https://example.com/"))
            .unwrap_or_else(|e| panic!("the request must serialize: {e}"));
        assert!(!body.contains("session"));
        assert!(!body.contains("proxy"));
        assert!(!body.contains("postData"));
        assert!(!body.contains("cookies"));
    }
}
