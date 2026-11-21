//! The reads of a Flywire portal, in-process, against a loopback server, the
//! session in an in-memory vault: the requests' methods, bodies and headers,
//! the cookie write-back, and the refusals. Nothing reads the real keychain
//! or reaches the network.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flywire::cli::ApiMethod;
use flywire::client::{api_request, ApiRequest};
use flywire::portal::{Env, Site};
use flywire::session::{Session, SessionStore};
use flywire::site::Provider;
use pk_cli_auth::vault::{MemoryKeys, Vault};
use pk_cli_config::profile::ProfileName;
use pk_cli_secrets::Secret;
use serde_json::{json, Value};

/// One request the server saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    cookie: Option<String>,
    user_agent: Option<String>,
    body: String,
}

/// A loopback server. A `GET` is answered by its path from `routes`
/// (`GET /path`); a `POST /graphql` by its `operationName`
/// (`POST graphql <operationName>`); anything else is a 404. `set_cookie`
/// goes on every answer.
struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

type Routes = BTreeMap<String, (u16, &'static str, String)>;

fn serve(routes: Routes, set_cookie: Option<&'static str>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(1) => head.push(byte[0]),
                    _ => break,
                }
            }
            let head = String::from_utf8_lossy(&head).to_string();
            let mut words = head.split_whitespace();
            let method = words.next().unwrap_or("").to_string();
            let target = words.next().unwrap_or("").to_string();
            let header = |name: &str| {
                head.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                })
            };
            let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; len];
            let _ = stream.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body).to_string();
            log.lock().unwrap().push(Seen {
                method: method.clone(),
                target: target.clone(),
                cookie: header("cookie"),
                user_agent: header("user-agent"),
                body: body.clone(),
            });
            let key = if method == "POST" && target == "/graphql" {
                let op = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["operationName"].as_str().map(str::to_string))
                    .unwrap_or_default();
                format!("POST graphql {op}")
            } else {
                format!("{method} {target}")
            };
            let (status, mime, body) =
                routes
                    .get(&key)
                    .cloned()
                    .unwrap_or((404, "text/html", "<p>not found</p>".to_string()));
            let mut answer = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            if status == 302 {
                answer.push_str("Location: /app/login\r\n");
            }
            if let Some(c) = set_cookie {
                answer.push_str(&format!("Set-Cookie: {c}\r\n"));
            }
            answer.push_str("\r\n");
            answer.push_str(&body);
            let _ = stream.write_all(answer.as_bytes());
        }
    });
    Server { base, seen }
}

const JSON: &str = "application/json; charset=utf-8";

/// The portal's answers, synthetic: only their shape (docs/api.md).
fn routes() -> Routes {
    let mut r = Routes::new();
    r.insert(
        "POST graphql payableStandingAmount".into(),
        (200, JSON, r#"{"data":{"payableStandingAmount":"12.30"}}"#.into()),
    );
    r.insert(
        "POST graphql expired".into(),
        (
            200,
            JSON,
            r#"{"data":null,"errors":[{"message":"Session Expired/Account Not Logged In"}]}"#.into(),
        ),
    );
    r.insert(
        "POST graphql broken".into(),
        (
            200,
            JSON,
            r#"{"data":null,"errors":[{"message":"Account ACCT-0000001 has no bill 42"}]}"#.into(),
        ),
    );
    r.insert("POST graphql redirected".into(), (302, "text/html", String::new()));
    r.insert(
        "GET /app/bills".into(),
        (200, "text/html", "<div id=\"root\"></div>".into()),
    );
    r
}

fn adult1() -> ProfileName {
    ProfileName::parse("adult1").unwrap()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "flywire-e2e-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn session(cookie: &str) -> Session {
    Session::new(
        Provider::parse("exampleclinic").unwrap(),
        Secret::new(cookie),
        Some("Mozilla/5.0 (test)".into()),
    )
}

/// An environment reading `server`, with adult1's session stored.
fn env(server: &Server, tag: &str) -> Env {
    let store = SessionStore::with_vault(Vault::new(MemoryKeys::new(), temp_dir(tag)));
    store
        .put(&adult1(), &session("_simplee_store2=dummy1; dummy_other=x"))
        .unwrap();
    Env {
        sessions: store,
        base: Some(server.base.clone()),
        pace: Duration::ZERO,
    }
}

const STANDING: &str = "query payableStandingAmount { payableStandingAmount }";

#[test]
fn a_query_posts_the_document_with_the_session_s_headers() {
    let server = serve(routes(), None);
    let env = env(&server, "query");
    let site = Site::open(&env, &adult1()).unwrap();
    let data = site.query("payableStandingAmount", STANDING, json!({})).unwrap();
    assert_eq!(data, json!({"payableStandingAmount": "12.30"}));
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!((seen[0].method.as_str(), seen[0].target.as_str()), ("POST", "/graphql"));
    assert_eq!(seen[0].cookie.as_deref(), Some("_simplee_store2=dummy1; dummy_other=x"));
    assert_eq!(seen[0].user_agent.as_deref(), Some("Mozilla/5.0 (test)"));
    let body: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(body["operationName"], "payableStandingAmount");
    assert_eq!(body["query"], STANDING);
}

#[test]
fn a_mutation_is_never_sent() {
    let server = serve(routes(), None);
    let env = env(&server, "mutation");
    let site = Site::open(&env, &adult1()).unwrap();
    for (op, doc) in [
        ("logout", "mutation logout { logout }"),
        ("verifyCode", "query verifyCode { verifyCode(code: \"1\") }"),
    ] {
        let e = site.query(op, doc, json!({})).err().unwrap();
        assert_eq!(e.exit_code(), 2, "{op}: {e}");
    }
    assert!(server.seen.lock().unwrap().is_empty());
}

#[test]
fn a_signed_out_session_is_exit_3_naming_the_provider() {
    let server = serve(routes(), None);
    let env = env(&server, "signed-out");
    let site = Site::open(&env, &adult1()).unwrap();
    for op in ["expired", "redirected"] {
        let e = site.query(op, &format!("query {op} {{ a }}"), json!({})).err().unwrap();
        assert_eq!(e.exit_code(), 3, "{op}: {e}");
        assert!(
            e.to_string()
                .contains("auth login --browser --provider exampleclinic --profile adult1"),
            "{e}"
        );
    }
}

#[test]
fn other_graphql_errors_are_exit_5_without_their_text() {
    let server = serve(routes(), None);
    let env = env(&server, "errors");
    let site = Site::open(&env, &adult1()).unwrap();
    let e = site.query("broken", "query broken { a }", json!({})).err().unwrap();
    assert_eq!(e.exit_code(), 5, "{e}");
    assert!(e.to_string().contains("1 GraphQL error"), "{e}");
    assert!(!e.to_string().contains("ACCT-0000001"), "{e}");
}

#[test]
fn the_api_returns_the_answer_whole() {
    let server = serve(routes(), None);
    let env = env(&server, "api");
    let site = Site::open(&env, &adult1()).unwrap();
    let post = api_request(
        ApiMethod::Post,
        "/graphql",
        Some(&json!({"operationName": "payableStandingAmount", "query": STANDING}).to_string()),
    )
    .unwrap();
    let a = site.api(&post).unwrap();
    assert_eq!(a.status, 200);
    assert_eq!(
        String::from_utf8_lossy(&a.body),
        r#"{"data":{"payableStandingAmount":"12.30"}}"#
    );
    let a = site.api(&ApiRequest::Get("/app/bills".into())).unwrap();
    assert_eq!(a.content_type.as_deref(), Some("text/html"));
    let expired = api_request(
        ApiMethod::Post,
        "/graphql",
        Some(&json!({"operationName": "expired", "query": "query expired { a }"}).to_string()),
    )
    .unwrap();
    assert_eq!(site.api(&expired).err().unwrap().exit_code(), 3);
}

#[test]
fn no_session_is_exit_3_naming_the_sign_in() {
    let server = serve(routes(), None);
    let env = Env {
        sessions: SessionStore::with_vault(Vault::new(MemoryKeys::new(), temp_dir("none"))),
        base: Some(server.base.clone()),
        pace: Duration::ZERO,
    };
    let e = Site::open(&env, &adult1()).err().unwrap();
    assert_eq!(e.exit_code(), 3);
    assert!(
        e.remedy()
            .unwrap()
            .contains("auth login --browser --provider <PROVIDER> --profile adult1"),
        "{e}"
    );
}

#[test]
fn refreshed_cookies_are_written_back_once() {
    let server = serve(routes(), Some("_simplee_store2=dummy2; Path=/; Secure; HttpOnly"));
    let env = env(&server, "writeback");
    let site = Site::open(&env, &adult1()).unwrap();
    site.query("payableStandingAmount", STANDING, json!({})).unwrap();
    assert!(site.finish().unwrap());
    let stored = env.sessions.get(&adult1()).unwrap().unwrap();
    assert_eq!(stored.cookie_header.expose(), "_simplee_store2=dummy2; dummy_other=x");
    assert_eq!(stored.user_agent.as_deref(), Some("Mozilla/5.0 (test)"));
    assert_eq!(stored.provider.as_str(), "exampleclinic");

    // A sign-in made meanwhile is never overwritten.
    let site = Site::open(&env, &adult1()).unwrap();
    site.query("payableStandingAmount", STANDING, json!({})).unwrap();
    env.sessions.put(&adult1(), &session("_simplee_store2=dummy9")).unwrap();
    assert!(!site.finish().unwrap());
    let stored = env.sessions.get(&adult1()).unwrap().unwrap();
    assert_eq!(stored.cookie_header.expose(), "_simplee_store2=dummy9");
}

#[test]
fn no_answer_names_the_server_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let server = Server {
        base: base.clone(),
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let env = env(&server, "no-answer");
    let e = Site::open(&env, &adult1())
        .unwrap()
        .query("payableStandingAmount", STANDING, json!({}))
        .err()
        .unwrap();
    assert_eq!(e.exit_code(), 5);
    let host = base.trim_start_matches("http://");
    assert!(e.to_string().contains(&format!("got no answer from {host}")), "{e}");
}
