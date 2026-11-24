//! The portal reads, made with a profile's stored session (docs/api.md).
//!
//! - A read is a `POST /graphql` of a `query` the portal's own app sends,
//!   on the profile's provider's portal (`https://<provider>.simpleepay.com`),
//!   or for `flywire api` a `GET` of a path there.
//! - A request carries the session's `Cookie` header and the browser's user
//!   agent captured with it.
//! - It follows no redirect and is bounded in time: 30 s for the whole
//!   answer, 10 s to connect.
//! - One request at a time, at least [`PACE`] after the last answer.
//! - Nothing writes to the portal: every document is checked by
//!   [`graphql::read_only_graphql`] and [`graphql::refused_field`] before it
//!   is sent, and `api` checks its request with [`api_request`].
//! - [`refuse`] turns what is no data into an error: a 401, a redirect or the
//!   portal's signed-out errors are a refused session (exit 3); a 403, a 429,
//!   a 5xx are exit 5.
//! - A `Set-Cookie` on an answer updates the client's jar; the caller writes
//!   the updated session back once the command has read what it needed
//!   ([`Client::updated_cookie_header`]).

use std::cell::{Cell, RefCell};
use std::io::Read as _;
use std::time::{Duration, Instant};

use pk_cli_config::profile::ProfileName;
use pk_cli_core::{CliError, Reason};
use pk_cli_secrets::Secret;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE, COOKIE, RETRY_AFTER, SET_COOKIE, USER_AGENT};
use serde_json::{json, Map, Value};

use crate::cli::{ApiMethod, BIN};
use crate::graphql;
use crate::session::Session;
use crate::site::Provider;

/// Points the reads at a test's loopback server instead of the provider's
/// portal ([`origin`]).
pub const BASE_URL_ENV: &str = "FLYWIRE_BASE_URL";
/// The least time between one answer and the next request.
pub const PACE: Duration = Duration::from_millis(750);
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of an answer read.
const MAX_BODY: u64 = 32 * 1024 * 1024;
/// The portal's GraphQL endpoint.
pub const GRAPHQL: &str = "/graphql";

/// The header the portal's app sends on every request (`source:
/// patient-client`, observed 2026-11-22). Without it `/graphql` answers
/// HTTP 400 with the body `{"errors": "no source found"}`, signed in or not.
pub const SOURCE: (&str, &str) = ("source", "patient-client");

/// A test's loopback origin from [`BASE_URL_ENV`], or `None` for the real
/// portals.
pub fn base_override() -> Result<Option<String>, CliError> {
    base_from(
        std::env::var(BASE_URL_ENV).ok().as_deref(),
        crate::session::is_test_store(),
        crate::session::hermetic(),
    )
}

/// The origin a session is read at: the test's server when one is set, else
/// the session's provider's portal.
pub fn origin(over: Option<&str>, provider: &Provider) -> String {
    over.map_or_else(|| provider.origin(), str::to_string)
}

/// The refusal of a request under a test's keychain service
/// (`session::hermetic`) with no [`BASE_URL_ENV`]: a test run reads only a
/// server on this machine, never the real portal.
fn hermetic_without_base() -> CliError {
    CliError::Usage(format!(
        "{} is set, so this is a test run, and a test run sends requests only to a server on this \
         machine: set {BASE_URL_ENV} to its loopback origin, like http://127.0.0.1:8080 (unset {} \
         to read the real portal)",
        crate::session::KEYCHAIN_SERVICE_ENV,
        crate::session::KEYCHAIN_SERVICE_ENV
    ))
}

/// The check behind [`base_override`], on the values passed in. Under a
/// test's keychain service (`hermetic`) the override is required, so a
/// test that forgets it is refused before it sends anything. The
/// override is for a test's server: a plain `http` origin on a loopback
/// address, and only over a test's own session store
/// (`session::is_test_store`), so no session the owner stored is sent to
/// it. An ambient variable sending a real session elsewhere would be a way
/// to steal it.
pub fn base_from(value: Option<&str>, test_store: bool, hermetic: bool) -> Result<Option<String>, CliError> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        if hermetic {
            return Err(hermetic_without_base());
        }
        return Ok(None);
    };
    let refuse = |why: &str| {
        CliError::Usage(format!(
            "{BASE_URL_ENV} {why}; it exists only to point tests at a server on this machine"
        ))
    };
    if !test_store {
        return Err(refuse(&format!(
            "needs a test's own session store, so no session the owner stored is sent to it: {} naming a \
             `piekstra.flywire.<suffix>` service, and XDG_CONFIG_HOME set away from ~/.config",
            crate::session::KEYCHAIN_SERVICE_ENV
        )));
    }
    let url = reqwest::Url::parse(value).map_err(|_| refuse("is not a URL"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let plain = url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none();
    if !(loopback && plain) {
        return Err(refuse(
            "must be an http origin on a loopback address, like http://127.0.0.1:8080",
        ));
    }
    Ok(Some(url.as_str().trim_end_matches('/').to_string()))
}

/// A session's cookies, in the order the header gave them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jar {
    cookies: Vec<(String, String)>,
    changed: bool,
}

impl Jar {
    pub fn parse(header: &str) -> Jar {
        let cookies = header
            .split(';')
            .filter_map(|pair| {
                let (name, value) = pair.trim().split_once('=')?;
                (!name.trim().is_empty()).then(|| (name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        Jar {
            cookies,
            changed: false,
        }
    }

    pub fn header(&self) -> String {
        self.cookies
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// One `Set-Cookie` value: a new value replaces the cookie's, a new
    /// cookie is added, and an empty value or `Max-Age` of zero or less
    /// removes it. The other attributes (domain, path, expiry) are the
    /// browser's business; the site's are all on its own hosts.
    pub fn apply(&mut self, set_cookie: &str) {
        let mut parts = set_cookie.split(';');
        let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else {
            return;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() {
            return;
        }
        let expired = parts.any(|a| {
            a.trim().split_once('=').is_some_and(|(k, v)| {
                k.trim().eq_ignore_ascii_case("max-age") && v.trim().parse::<i64>().is_ok_and(|n| n <= 0)
            })
        });
        let at = self.cookies.iter().position(|(n, _)| n == name);
        match (at, value.is_empty() || expired) {
            (Some(i), true) => {
                self.cookies.remove(i);
                self.changed = true;
            }
            (Some(i), false) if self.cookies[i].1 != value => {
                self.cookies[i].1 = value.to_string();
                self.changed = true;
            }
            (None, false) => {
                self.cookies.push((name.to_string(), value.to_string()));
                self.changed = true;
            }
            _ => {}
        }
    }

    pub fn changed(&self) -> bool {
        self.changed
    }
}

/// What a read sends and to whom: the session's cookies and user agent.
pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
    jar: RefCell<Jar>,
    pace: Duration,
    last: Cell<Option<Instant>>,
    profile: ProfileName,
    provider: Provider,
}

impl Client {
    /// A client for `profile`'s `session` against `base` (the provider's
    /// portal, or a test's loopback server), `pace` apart.
    pub fn new(session: &Session, profile: &ProfileName, base: &str, pace: Duration) -> Result<Client, CliError> {
        let agent = session
            .user_agent
            .clone()
            .unwrap_or_else(|| format!("{BIN}/{}", env!("CARGO_PKG_VERSION")));
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&agent)
                .map_err(|_| CliError::Other("the stored user agent is not a valid HTTP header value".into()))?,
        );
        headers.insert(SOURCE.0, HeaderValue::from_static(SOURCE.1));
        // A plain client: no cookie store (the jar is ours), no redirects.
        let http = reqwest::blocking::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| CliError::Other(format!("building the HTTP client: {}", e.without_url())))?;
        Ok(Client {
            http,
            base: base.trim_end_matches('/').to_string(),
            jar: RefCell::new(Jar::parse(session.cookie_header.expose())),
            pace,
            last: Cell::new(None),
            profile: profile.clone(),
            provider: session.provider.clone(),
        })
    }

    /// One query the portal's app sends: its `data`, after [`refuse`] and
    /// the answer's `errors`. The document is checked before it is sent.
    pub fn query(&self, operation: &str, document: &str, variables: Value) -> Result<Value, CliError> {
        check_document(document)?;
        let body = json!({ "operationName": operation, "variables": variables, "query": document });
        let answer = self.send(GRAPHQL, Some(&body.to_string()), "application/json", operation)?;
        refuse(&answer, &self.profile, &self.provider)?;
        let doc: Value = serde_json::from_slice(&answer.body).map_err(|_| {
            CliError::Upstream(format!(
                "{} answered HTTP {} with no JSON; the portal may have changed",
                answer.what, answer.status
            ))
        })?;
        if graphql::is_signed_out(&doc) {
            return Err(refused_session(
                &answer.what,
                "nobody is signed in",
                &self.profile,
                &self.provider,
            ));
        }
        if let Some(n) = doc
            .get("errors")
            .and_then(Value::as_array)
            .map(Vec::len)
            .filter(|n| *n > 0)
        {
            return Err(CliError::Upstream(format!(
                "{} answered with {n} GraphQL error(s); `{BIN} api POST /graphql` shows them",
                answer.what
            ))
            .because(Reason::BadRequest));
        }
        match doc.get("data") {
            Some(data) if data.is_object() => Ok(data.clone()),
            _ => Err(CliError::Upstream(format!(
                "{} answered without data; the portal may have changed",
                answer.what
            ))),
        }
    }

    /// `flywire api`: the answer as it came, after the refusals every read
    /// gets.
    pub fn api(&self, request: &ApiRequest) -> Result<Answer, CliError> {
        let answer = match request {
            ApiRequest::Get(path) => self.send(path, None, "application/json,text/html;q=0.9,*/*;q=0.8", "")?,
            ApiRequest::Query(body) => {
                let label = body.get("operationName").and_then(Value::as_str).unwrap_or("");
                self.send(GRAPHQL, Some(&body.to_string()), "application/json", label)?
            }
        };
        refuse(&answer, &self.profile, &self.provider)?;
        if answer.content_type.as_deref().is_some_and(|t| t.contains("json")) {
            if let Ok(doc) = serde_json::from_slice::<Value>(&answer.body) {
                if graphql::is_signed_out(&doc) {
                    return Err(refused_session(
                        &answer.what,
                        "nobody is signed in",
                        &self.profile,
                        &self.provider,
                    ));
                }
            }
        }
        Ok(answer)
    }

    /// The session's cookie header as the answers left it, when one changed it.
    pub fn updated_cookie_header(&self) -> Option<Secret> {
        let jar = self.jar.borrow();
        jar.changed().then(|| Secret::new(jar.header()))
    }

    /// A `GET` of `path`, or with `body` a `POST` of it as JSON. `label`
    /// names a GraphQL operation in errors.
    fn send(&self, path: &str, body: Option<&str>, accept: &str, label: &str) -> Result<Answer, CliError> {
        // The path without its query: a query can carry an account number.
        let route = path.split(['?', '#']).next().unwrap_or(path);
        let what = match (body, label) {
            (None, _) => format!("GET {route}"),
            (Some(_), "") => format!("POST {route}"),
            (Some(_), op) => format!("POST {route} ({op})"),
        };
        if let Some(last) = self.last.get() {
            let since = last.elapsed();
            if since < self.pace {
                std::thread::sleep(self.pace - since);
            }
        }
        let mut cookie = HeaderValue::from_str(&self.jar.borrow().header())
            .map_err(|_| CliError::Other("the session's Cookie header is not a valid HTTP header value".into()))?;
        cookie.set_sensitive(true);
        let url = format!("{}{path}", self.base);
        let request = match body {
            None => self.http.get(url),
            Some(b) => self
                .http
                .post(url)
                .header(CONTENT_TYPE, "application/json")
                .body(b.to_string()),
        };
        let result = request.header(ACCEPT, accept).header(COOKIE, cookie).send();
        let answer = match result {
            Ok(a) => a,
            Err(e) => {
                self.last.set(Some(Instant::now()));
                return Err(if e.is_timeout() {
                    CliError::Upstream(format!("{what} timed out after {} s", TIMEOUT.as_secs()))
                } else {
                    let host = self.base.split_once("://").map_or(self.base.as_str(), |(_, h)| h);
                    CliError::Upstream(format!("{what} got no answer from {host}"))
                });
            }
        };
        let status = answer.status().as_u16();
        let header = |name| {
            answer
                .headers()
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header(CONTENT_TYPE);
        let retry_after = header(RETRY_AFTER);
        {
            let mut jar = self.jar.borrow_mut();
            for v in answer.headers().get_all(SET_COOKIE) {
                if let Ok(v) = v.to_str() {
                    jar.apply(v);
                }
            }
        }
        let mut body = Vec::new();
        let read = answer.take(MAX_BODY + 1).read_to_end(&mut body);
        self.last.set(Some(Instant::now()));
        if read.is_err() {
            return Err(CliError::Upstream(format!(
                "{what} answered HTTP {status} and its body did not arrive"
            )));
        }
        if body.len() as u64 > MAX_BODY {
            return Err(CliError::Upstream(format!(
                "{what} answered with more than {} MiB",
                MAX_BODY >> 20
            )));
        }
        Ok(Answer {
            what,
            status,
            content_type,
            retry_after,
            body,
        })
    }
}

/// A document flywire sends: queries only, and no query field that does
/// more than read. Exit 2 otherwise.
pub fn check_document(document: &str) -> Result<(), CliError> {
    if !graphql::read_only_graphql(document) {
        return Err(CliError::Usage(format!(
            "{BIN} is read-only: it sends GraphQL `query` documents only, never a mutation or subscription"
        )));
    }
    if let Some(field) = graphql::refused_field(document) {
        return Err(CliError::Usage(format!(
            "{BIN} is read-only: `{field}` does more than read (a check, a verification, a payment or a \
             financing partner's lookup), so it is never sent"
        )));
    }
    Ok(())
}

/// What `flywire api` sends, checked by [`api_request`].
#[derive(Debug, Clone, PartialEq)]
pub enum ApiRequest {
    /// `GET` of a path on the portal.
    Get(String),
    /// `POST /graphql` of this body: `query`, `variables` and
    /// `operationName` only.
    Query(Value),
}

/// Words that, as a path segment or a query key, name something other than
/// a read. A `GET` of a form page changes nothing, but `api` keeps clear of
/// them all the same.
const ACTION_WORDS: &[&str] = &[
    "pay",
    "autopay",
    "plan",
    "checkout",
    "wallet",
    "card",
    "bank",
    "echeck",
    "method",
    "login",
    "logout",
    "signin",
    "signout",
    "subscription",
    "unsubscribe",
    "create",
    "cancel",
    "update",
    "delete",
    "edit",
    "save",
    "submit",
    "send",
    "message",
    "paperless",
    "preference",
    "setting",
    "contact",
    "dispute",
    "enroll",
    "verify",
    "activation",
    "graphql",
];

/// Whether a path segment or query key is, or starts with, an action word.
/// `payments` and `payables` are reads.
fn names_an_action(word: &str) -> bool {
    let w = word.to_ascii_lowercase();
    if matches!(w.as_str(), "payments" | "payables" | "paymenthistory") {
        return false;
    }
    ACTION_WORDS.iter().any(|a| w.starts_with(a))
}

/// `flywire api <METHOD> <PATH> [--data JSON]` as a read, or exit 2.
/// Nothing is read or sent before this passes.
///
/// - `GET` of a path (leading `/`) on the profile's portal, naming no
///   action and not `/graphql`.
/// - `POST /graphql` with `--data` a JSON object of `query` (a document
///   [`check_document`] accepts), and optionally `variables` (an object)
///   and `operationName` (a string): no batches, no persisted queries.
pub fn api_request(method: ApiMethod, path: &str, data: Option<&str>) -> Result<ApiRequest, CliError> {
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(CliError::Usage("the path holds a space or a control character".into()));
    }
    if path.contains("://") {
        return Err(CliError::Usage(format!(
            "`{BIN} api` reads the profile's own portal; give a path on it (`/graphql`, `/app/...`)"
        )));
    }
    if !path.starts_with('/') {
        return Err(CliError::Usage("the path starts with `/`".into()));
    }
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    if route.contains("..") || route.contains('%') || route.contains("//") {
        return Err(CliError::Usage("the path holds `..`, `//` or a %-escape".into()));
    }
    match method {
        ApiMethod::Get => {
            if data.is_some() {
                return Err(CliError::Usage("--data goes with POST /graphql".into()));
            }
            let action = route.split('/').any(names_an_action)
                || query
                    .split('&')
                    .any(|pair| names_an_action(pair.split('=').next().unwrap_or("")));
            if action {
                return Err(CliError::Usage(format!(
                    "{BIN} is read-only: that path names GraphQL, a payment, a sign-in or a change; send a \
                     query with `{BIN} api POST /graphql --data ...`"
                )));
            }
            Ok(ApiRequest::Get(path.to_string()))
        }
        ApiMethod::Post => {
            if !route.eq_ignore_ascii_case(GRAPHQL) || !query.is_empty() {
                return Err(CliError::Usage(format!(
                    "`{BIN} api POST` sends a GraphQL query to {GRAPHQL} only"
                )));
            }
            let Some(data) = data else {
                return Err(CliError::Usage(
                    "POST /graphql needs --data '{\"query\": \"query ...\", \"variables\": {...}}'".into(),
                ));
            };
            let body: Map<String, Value> =
                serde_json::from_str(data).map_err(|_| CliError::Usage("--data is not one JSON object".into()))?;
            if let Some(extra) = body
                .keys()
                .find(|k| !matches!(k.as_str(), "query" | "variables" | "operationName"))
            {
                return Err(CliError::Usage(format!(
                    "--data holds `{extra}`; it takes `query`, `variables` and `operationName` only"
                )));
            }
            let Some(document) = body.get("query").and_then(Value::as_str) else {
                return Err(CliError::Usage(
                    "--data needs `query`, the GraphQL document, as a string".into(),
                ));
            };
            check_document(document)?;
            if body.get("variables").is_some_and(|v| !v.is_object()) {
                return Err(CliError::Usage("`variables` is a JSON object".into()));
            }
            if body.get("operationName").is_some_and(|v| !v.is_string()) {
                return Err(CliError::Usage("`operationName` is a string".into()));
            }
            Ok(ApiRequest::Query(Value::Object(body)))
        }
    }
}

/// An answer, read whole.
pub struct Answer {
    /// `GET /path` or `POST /graphql (operation)`, no query.
    pub what: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub retry_after: Option<String>,
    pub body: Vec<u8>,
}

fn login_remedy(profile: &ProfileName, provider: &Provider) -> String {
    format!("{BIN} auth login --browser --provider {provider} --profile {profile} --overwrite")
}

fn refused_session(what: &str, said: &str, profile: &ProfileName, provider: &Provider) -> CliError {
    CliError::Auth(format!(
        "the portal refused profile `{profile}`'s session ({what}: {said}): it has signed out or expired"
    ))
    .because(Reason::CredentialRefused)
    .with_remedy(login_remedy(profile, provider))
}

/// The answers that are no data whatever the read: a refused session, the
/// edge's page, too many requests, the portal's own failures.
pub fn refuse(a: &Answer, profile: &ProfileName, provider: &Provider) -> Result<(), CliError> {
    let what = &a.what;
    let status = a.status;
    let html = a.content_type.as_deref().is_some_and(|t| t.contains("html"));
    match status {
        200..=299 => Ok(()),
        400 if crate::site::is_sourceless(&a.body) => Err(crate::site::sourceless(what)),
        401 => Err(refused_session(what, "HTTP 401", profile, provider)),
        300..=399 => Err(refused_session(
            what,
            &format!("a redirect, HTTP {status}"),
            profile,
            provider,
        )),
        403 if html => Err(CliError::Upstream(format!(
            "the portal's edge refused {what} (HTTP 403 with its error page)"
        ))
        .because(Reason::ClientBlocked)
        .with_remedy(format!(
            "sign in again with `{}`, which keeps the browser's user agent",
            login_remedy(profile, provider)
        ))),
        403 => Err(CliError::Upstream(format!("the portal refused {what} (HTTP 403)")).because(Reason::Forbidden)),
        429 => {
            let e = CliError::Upstream(format!("the portal answered {what} with HTTP 429: too many requests"));
            Err(match &a.retry_after {
                Some(after) => e.with_remedy(format!("try again after {} s", after.trim())),
                None => e.with_remedy("try again in a few minutes"),
            })
        }
        500..=599 => Err(
            CliError::Upstream(format!("the portal answered {what} with HTTP {status}")).with_remedy("try again later"),
        ),
        400 => Err(CliError::Upstream(format!(
            "the portal refused {what} as a bad request (HTTP 400); it may have changed"
        ))
        .because(Reason::BadRequest)),
        _ => Err(CliError::Upstream(format!(
            "the portal answered {what} with HTTP {status}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adult1() -> ProfileName {
        ProfileName::parse("adult1").unwrap()
    }

    fn example() -> Provider {
        Provider::parse("exampleclinic").unwrap()
    }

    #[test]
    fn a_test_run_without_a_base_is_refused_before_any_request() {
        for test_store in [true, false] {
            let e = base_from(None, test_store, true).unwrap_err();
            assert_eq!(e.exit_code(), 2);
            let m = e.to_string();
            assert!(
                m.contains(BASE_URL_ENV) && m.contains(crate::session::KEYCHAIN_SERVICE_ENV),
                "{m}"
            );
        }
        assert_eq!(
            base_from(Some("http://127.0.0.1:8"), true, true).unwrap().as_deref(),
            Some("http://127.0.0.1:8")
        );
    }

    #[test]
    fn the_origin_is_the_provider_s_portal_unless_a_test_points_it_at_loopback() {
        assert_eq!(base_from(None, false, false).unwrap(), None);
        assert_eq!(base_from(Some("  "), false, false).unwrap(), None);
        assert_eq!(origin(None, &example()), "https://exampleclinic.simpleepay.com");
        for ok in [
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/",
            "http://localhost:9",
            "http://[::1]:9",
        ] {
            let b = base_from(Some(ok), true, false).unwrap().unwrap();
            assert!(!b.ends_with('/'), "{b}");
            assert_eq!(origin(Some(&b), &example()), b);
        }
        // Without a test's own store, a real session could travel.
        assert_eq!(
            base_from(Some("http://127.0.0.1:8080"), false, false)
                .unwrap_err()
                .exit_code(),
            2
        );
        for bad in [
            "https://exampleclinic.simpleepay.com.example.com",
            "http://example.com",
            "http://127.0.0.1:1@example.com",
            "http://user@localhost:8",
            "https://127.0.0.1:8",
            "http://127.0.0.1:8/api",
            "http://127.0.0.1:8/?x=1",
            "http://127.0.0.2:8",
            "127.0.0.1:8",
        ] {
            assert_eq!(base_from(Some(bad), true, false).unwrap_err().exit_code(), 2, "{bad}");
        }
    }

    #[test]
    fn a_sourceless_400_is_exit_5_naming_the_header() {
        let e = refuse(
            &answer(400, "application/json", r#"{"errors":"no source found"}"#),
            &adult1(),
            &example(),
        )
        .unwrap_err();
        assert_eq!(e.exit_code(), 5);
        assert!(e.to_string().contains("\"source: patient-client\""), "{e}");
        // Another 400 is not read as a missing header.
        let other = refuse(
            &answer(400, "application/json", r#"{"errors":[{"message":"bad"}]}"#),
            &adult1(),
            &example(),
        );
        assert!(!other.is_err_and(|e| e.to_string().contains("no source found")));
    }

    fn answer(status: u16, content_type: &str, body: &str) -> Answer {
        Answer {
            what: "POST /graphql (payables)".into(),
            status,
            content_type: Some(content_type.into()),
            retry_after: None,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn refuse_turns_what_is_no_data_into_errors() {
        assert!(refuse(&answer(200, "application/json", "{}"), &adult1(), &example()).is_ok());
        for (status, mime, exit) in [
            (401, "application/json", 3),
            (302, "text/html", 3),
            (403, "text/html", 5),
            (403, "application/json", 5),
            (429, "text/html", 5),
            (503, "text/html", 5),
            (400, "application/json", 5),
        ] {
            let e = refuse(&answer(status, mime, ""), &adult1(), &example()).err().unwrap();
            assert_eq!(e.exit_code(), exit, "{status}: {e}");
        }
        let e = refuse(&answer(302, "text/html", ""), &adult1(), &example())
            .err()
            .unwrap();
        assert!(
            e.to_string()
                .contains("auth login --browser --provider exampleclinic --profile adult1"),
            "{e}"
        );
    }

    #[test]
    fn set_cookie_updates_the_jar() {
        let mut jar = Jar::parse("a=1; b=2");
        jar.apply("a=1; Path=/");
        assert!(!jar.changed(), "the same value changes nothing");
        jar.apply("a=3; Path=/; Secure");
        jar.apply("c=4");
        jar.apply("b=; Max-Age=0");
        assert!(jar.changed());
        assert_eq!(jar.header(), "a=3; c=4");
        assert_eq!(jar.get("c"), Some("4"));
        jar.apply("c=5; Max-Age=-1");
        assert_eq!(jar.header(), "a=3");
    }

    #[test]
    fn the_api_sends_reads_only() {
        let query =
            r#"{"query":"query payables { payables { payables { id } } }","variables":{},"operationName":"payables"}"#;
        match api_request(ApiMethod::Post, "/graphql", Some(query)).unwrap() {
            ApiRequest::Query(body) => assert_eq!(body["operationName"], "payables"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            api_request(ApiMethod::Get, "/app/bills", None).unwrap(),
            ApiRequest::Get("/app/bills".into())
        );
        assert!(api_request(ApiMethod::Get, "/app/payments?page=2", None).is_ok());
        let refused = |method: ApiMethod, path: &str, data: Option<&str>| {
            let e = api_request(method, path, data).err().unwrap();
            assert_eq!(e.exit_code(), 2, "{path} {data:?}: {e}");
            e.to_string()
        };
        refused(
            ApiMethod::Post,
            "/graphql",
            Some(r#"{"query":"mutation logout { logout }"}"#),
        );
        refused(
            ApiMethod::Post,
            "/graphql",
            Some(r#"{"query":"query A { a } mutation B { b }"}"#),
        );
        refused(
            ApiMethod::Post,
            "/graphql",
            Some(r#"{"query":"query validateCreditCard { validateCreditCard(n: \"x\") }"}"#),
        );
        refused(ApiMethod::Post, "/graphql", Some(r#"[{"query":"query { a }"}]"#));
        refused(
            ApiMethod::Post,
            "/graphql",
            Some(r#"{"query":"query { a }","extensions":{"persistedQuery":{"sha256Hash":"x"}}}"#),
        );
        refused(ApiMethod::Post, "/graphql", Some(r#"{"variables":{}}"#));
        refused(
            ApiMethod::Post,
            "/graphql",
            Some(r#"{"query":"query { a }","variables":[]}"#),
        );
        refused(ApiMethod::Post, "/graphql", Some("not json"));
        refused(ApiMethod::Post, "/graphql", None);
        refused(ApiMethod::Post, "/graphql?x=1", Some(query));
        refused(ApiMethod::Post, "/app/bills", Some(query));
        refused(ApiMethod::Get, "/graphql?query=mutation%20x", None);
        refused(ApiMethod::Get, "/graphql", None);
        refused(ApiMethod::Get, "/app/bills", Some("{}"));
        for path in [
            "/app/login",
            "/app/pay",
            "/app/payment-plan",
            "/providers/exampleclinic/subscriptions/go_paperless",
            "/email_addresses/unsubscribe",
            "/app/bills?activation_token=x",
            "/app/../graphql",
            "/app/%2e%2e/graphql",
            "//example.com/x",
            "https://exampleclinic.simpleepay.com/app/bills",
            "app/bills",
            "/app/bills ?x=1",
        ] {
            refused(ApiMethod::Get, path, None);
        }
    }
}
