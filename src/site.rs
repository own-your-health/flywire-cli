//! A Flywire patient portal on simpleepay.com (the Simplee platform Flywire
//! bought in 2020): which provider's portal, what `auth login --browser`
//! reads out of the Chrome window, and the one read that confirms a session
//! before it is stored.
//!
//! Each provider has its own portal, `https://<provider>.simpleepay.com`, a
//! single-page app at `/app` that reads and writes through `POST /graphql`
//! on the same host. A patient signs in as a guest: the account (or patient
//! reference) number from a statement, the patient's or guarantor's date of
//! birth and, for some providers, a second detail (ZIP code, the last four
//! of an SSN). The session is the `HttpOnly` cookie [`SESSION_COOKIE`]; the
//! site sets cookies before anyone signs in, so the capture reads the
//! host's whole jar and [`confirm_session`] makes one read with it, the
//! query `payableStandingAmount`, which only a signed-in session answers
//! with data (docs/api.md).

use std::io::Read;
use std::time::Duration;

use pk_cli_browser::{BrowserError, Capture, Captured, Confirm, HostRule};
use pk_cli_core::CliError;
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE, COOKIE};
use serde_json::{json, Value};

use crate::cli::BIN;

/// The provider's name in the browser crate's status lines and errors.
pub const LABEL: &str = "Flywire";
/// The domain every provider's portal is a host under.
pub const DOMAIN: &str = "simpleepay.com";
/// What signing in involves, for the opening status line.
pub const HINT: &str = "the account number from your statement and the patient's date of birth";
/// The cookie that carries a portal session (`HttpOnly`).
pub const SESSION_COOKIE: &str = "_simplee_store2";
/// The read that shows whether a jar is signed in: a query a visitor gets
/// no data for.
pub const SESSION_CHECK: &str = "query payableStandingAmount { payableStandingAmount }";

/// The bound on the whole read, connecting included. The browser crate's poll
/// (and Ctrl-C) waits for it.
const TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of an answer's body read for [`judge`].
const MAX_BODY: u64 = 1024 * 1024;

/// A provider's portal: the subdomain of simpleepay.com its statements and
/// emails link to (`https://<provider>.simpleepay.com/app/...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider(String);

impl Provider {
    /// A provider name: one DNS label, 1 to 63 lowercase letters, digits and
    /// `-`, not starting or ending with `-`. A portal URL or host on
    /// simpleepay.com gives its first label. Anything else is exit 2.
    pub fn parse(raw: &str) -> Result<Provider, CliError> {
        let s = raw.trim().to_ascii_lowercase();
        let s = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .unwrap_or(&s);
        let host = s.split(['/', '?', '#']).next().unwrap_or("");
        let label = match host.strip_suffix(&format!(".{DOMAIN}")) {
            Some(sub) => sub,
            None if host.contains('.') => {
                return Err(CliError::Usage(format!(
                    "`{raw}` is not a portal on {DOMAIN}; give the provider's name, the part before .{DOMAIN}"
                )))
            }
            None => host,
        };
        let ok = (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label != "www";
        if !ok {
            return Err(CliError::Usage(format!(
                "`{raw}` is not a provider name: the part before .{DOMAIN} in your portal's address \
                 (letters, digits and `-`)"
            )));
        }
        Ok(Provider(label.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `<provider>.simpleepay.com`.
    pub fn host(&self) -> String {
        format!("{}.{DOMAIN}", self.0)
    }

    /// The portal's origin, `https://<provider>.simpleepay.com`.
    pub fn origin(&self) -> String {
        format!("https://{}", self.host())
    }

    /// Where the Chrome window opens: the portal's sign-in page.
    pub fn start_url(&self) -> String {
        format!("{}/app/login", self.origin())
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What `auth login --browser` reads: a page on the provider's host, its
/// whole cookie jar (with [`SESSION_COOKIE`] in it) and the browser's user
/// agent. With `verify`, the tab stays open until [`confirm_candidate`]
/// says the jar is a signed-in one.
pub fn capture(provider: &Provider, verify: bool) -> Result<Capture, BrowserError> {
    let host = provider.host();
    let capture = Capture::new(LABEL, HostRule::exact(&host)?)
        .cookie(provider.start_url(), SESSION_COOKIE)
        .cookie_jar(&host)
        .with_user_agent()
        .hint(HINT);
    Ok(if verify {
        let origin = provider.origin();
        capture.confirm(move |candidate: &Captured| confirm_candidate(&origin, candidate))
    } else {
        capture
    })
}

/// The capture's confirm hook: [`confirm_session`] on the candidate's jar.
pub fn confirm_candidate(origin: &str, candidate: &Captured) -> Confirm {
    match candidate.cookie_header() {
        Some(header) => confirm_session(origin, header.expose(), candidate.user_agent()),
        None => Confirm::NotYet(format!("the browser holds no {DOMAIN} cookie yet")),
    }
}

/// One `POST /graphql` of [`SESSION_CHECK`] on `origin` (the portal, or a
/// test's loopback server) with `cookie_header`, judged by [`judge`]. A
/// 429, a 5xx, a 403 with an HTML page, no answer or an answer that does not
/// arrive whole is `Undecided`. A redirect is `NotYet`. `user_agent` is the
/// browser's, for a session captured from one.
pub fn confirm_session(origin: &str, cookie_header: &str, user_agent: Option<&str>) -> Confirm {
    let url = format!("{}/graphql", origin.trim_end_matches('/'));
    let what = "POST /graphql (payableStandingAmount)".to_string();
    let client = match client(user_agent) {
        Ok(client) => client,
        Err(e) => return Confirm::Fail(e),
    };
    let Ok(mut cookie) = HeaderValue::from_str(cookie_header) else {
        return Confirm::Fail(CliError::Other(
            "the session's Cookie header is not a valid HTTP header value".into(),
        ));
    };
    cookie.set_sensitive(true);
    let body = json!({ "operationName": "payableStandingAmount", "variables": {}, "query": SESSION_CHECK });
    let answer = match client
        .post(&url)
        .header(COOKIE, cookie)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
    {
        Ok(answer) => answer,
        Err(e) if e.is_timeout() => return Confirm::undecided(format!("{what} timed out")),
        Err(_) => return Confirm::undecided(format!("{what} got no answer")),
    };
    if let Some(said) = pk_cli_http::inconclusive(&answer) {
        return Confirm::Undecided {
            reason: format!("{what} answered HTTP {}", said.status),
            retry_after: said.retry_after,
        };
    }
    let status = answer.status().as_u16();
    if answer.status().is_redirection() {
        return Confirm::NotYet(format!(
            "{what} answered with a redirect (HTTP {status}): not signed in"
        ));
    }
    let content_type = answer
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut body = Vec::new();
    if answer.take(MAX_BODY + 1).read_to_end(&mut body).is_err() {
        return Confirm::undecided(format!("{what} answered HTTP {status} and its body did not arrive"));
    }
    if body.len() as u64 > MAX_BODY {
        return Confirm::undecided(format!("{what} answered with more than 1 MiB"));
    }
    judge(status, content_type.as_deref(), &body)
}

/// How the answer to [`SESSION_CHECK`] shows a signed-in patient.
///
/// `Ready` for `{"data": {"payableStandingAmount": <a value>}}` with no
/// `errors`. `NotYet` for a 4xx, or an answer whose `errors` say nobody is
/// signed in ([`crate::graphql::is_signed_out`]). Undecided for a 2xx that
/// is not JSON or says neither. It sees only answers that
/// [`confirm_session`] has not already decided.
pub fn judge(status: u16, content_type: Option<&str>, body: &[u8]) -> Confirm {
    let what = "POST /graphql (payableStandingAmount)";
    if (400..500).contains(&status) {
        return Confirm::NotYet(format!("{what} answered HTTP {status}: not signed in"));
    }
    if !(200..300).contains(&status) {
        return Confirm::undecided(format!("{what} answered HTTP {status}"));
    }
    if !content_type.is_some_and(|t| t.contains("json")) {
        return Confirm::undecided(format!("{what} answered HTTP {status} without JSON"));
    }
    let Ok(doc) = serde_json::from_slice::<Value>(body) else {
        return Confirm::undecided(format!("{what} answered HTTP {status} with JSON that does not parse"));
    };
    if crate::graphql::is_signed_out(&doc) {
        return Confirm::NotYet(format!("{what} answered that nobody is signed in"));
    }
    let has_errors = doc
        .get("errors")
        .is_some_and(|e| e.as_array().is_some_and(|a| !a.is_empty()));
    match doc.pointer("/data/payableStandingAmount") {
        Some(v) if !v.is_null() && !has_errors => Confirm::Ready,
        _ => Confirm::undecided(format!("{what} answered without the account's amount")),
    }
}

/// The confirm read's client: no redirects followed, bounded in time.
fn client(user_agent: Option<&str>) -> Result<reqwest::blocking::Client, CliError> {
    let mut builder = pk_cli_http::builder(BIN, env!("CARGO_PKG_VERSION"))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT);
    if let Some(agent) = user_agent {
        builder = builder.user_agent(agent);
    }
    builder
        .build()
        .map_err(|e| CliError::Other(format!("building the HTTP client: {}", e.without_url())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn example() -> Provider {
        Provider::parse("exampleclinic").unwrap()
    }

    #[test]
    fn a_provider_is_one_label_on_simpleepay() {
        for (raw, want) in [
            ("exampleclinic", "exampleclinic"),
            ("ExampleClinic", "exampleclinic"),
            ("example-clinic2", "example-clinic2"),
            ("exampleclinic.simpleepay.com", "exampleclinic"),
            ("https://exampleclinic.simpleepay.com/app/login", "exampleclinic"),
        ] {
            assert_eq!(Provider::parse(raw).unwrap().as_str(), want, "{raw}");
        }
        for bad in [
            "",
            "-x",
            "x-",
            "a b",
            "a.b",
            "www",
            "exampleclinic.example.com",
            "https://evil.example/x.simpleepay.com",
            "a.b.simpleepay.com",
            "x_y",
        ] {
            assert_eq!(Provider::parse(bad).err().unwrap().exit_code(), 2, "{bad}");
        }
        assert_eq!(example().start_url(), "https://exampleclinic.simpleepay.com/app/login");
    }

    /// The capture `pk-cli-browser` checks before Chrome starts, checked
    /// offline, with and without the confirm hook.
    #[test]
    fn the_browser_capture_is_well_formed() {
        for verify in [true, false] {
            let capture = capture(&example(), verify).unwrap();
            capture.validate().unwrap();
            assert_eq!(capture.label(), LABEL);
        }
    }

    #[test]
    fn no_jar_is_not_signed_in_yet() {
        let candidate = Captured::for_test(LABEL, "exampleclinic.simpleepay.com")
            .user_agent("Mozilla/5.0 (test)")
            .build();
        match confirm_candidate("https://exampleclinic.simpleepay.com", &candidate) {
            Confirm::NotYet(reason) => assert!(reason.contains("no simpleepay.com cookie"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn judge_reads_the_answers() {
        let json = Some("application/json; charset=utf-8");
        assert!(matches!(
            judge(200, json, br#"{"data":{"payableStandingAmount":0}}"#),
            Confirm::Ready
        ));
        assert!(matches!(
            judge(200, json, br#"{"data":{"payableStandingAmount":"12.30"}}"#),
            Confirm::Ready
        ));
        assert!(matches!(
            judge(200, json, br#"{"data":null,"errors":[{"message":"Unauthorized"}]}"#),
            Confirm::NotYet(_)
        ));
        assert!(matches!(judge(401, json, b"{}"), Confirm::NotYet(r) if r.contains("HTTP 401")));
        for (status, mime, body) in [
            (200, json, &br#"{"data":{"payableStandingAmount":null}}"#[..]),
            (200, json, &br#"{"data":{}}"#[..]),
            (200, json, &b"{"[..]),
            (200, Some("text/html"), &b"<p>edge</p>"[..]),
            (304, None, &b""[..]),
        ] {
            assert!(
                matches!(judge(status, mime, body), Confirm::Undecided { .. }),
                "{status} {:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// Serves one canned answer on loopback and hands back the request.
    fn serve_once(answer: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head).to_lowercase();
            let len: usize = text
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |v| v.trim().parse().unwrap());
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(answer.as_bytes()).unwrap();
            format!("{text}{}", String::from_utf8_lossy(&body).to_lowercase())
        });
        (origin, handle)
    }

    #[test]
    fn the_confirm_read_posts_the_query_with_the_jar() {
        let (origin, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\n\r\n{\"data\":{\"payableStandingAmount\":0}}",
        );
        assert!(matches!(
            confirm_session(&origin, "a=b; c=d", Some("Mozilla/5.0 (test)")),
            Confirm::Ready
        ));
        let seen = server.join().unwrap();
        assert!(seen.starts_with("post /graphql "), "{seen}");
        assert!(seen.contains("\r\ncookie: a=b; c=d\r\n"), "{seen}");
        assert!(seen.contains("\r\nuser-agent: mozilla/5.0 (test)\r\n"), "{seen}");
        assert!(seen.contains("query payablestandingamount"), "{seen}");

        let (origin, server) =
            serve_once("HTTP/1.1 302 Found\r\nLocation: https://example.com/app/login\r\nContent-Length: 0\r\n\r\n");
        assert!(matches!(confirm_session(&origin, "a=b", None), Confirm::NotYet(r) if r.contains("redirect")));
        server.join().unwrap();

        let (origin, server) =
            serve_once("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 0\r\n\r\n");
        match confirm_session(&origin, "a=b", None) {
            Confirm::Undecided { retry_after, .. } => assert_eq!(retry_after, Some(Duration::from_secs(30))),
            other => panic!("{other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn no_answer_is_undecided() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        match confirm_session(&format!("http://127.0.0.1:{port}"), "a=b", None) {
            Confirm::Undecided { reason, .. } => assert!(reason.ends_with("got no answer"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }
}
