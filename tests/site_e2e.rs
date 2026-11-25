//! The reads of a Flywire portal, in-process, against a loopback server, the
//! session in an in-memory vault: the requests' methods, bodies and headers,
//! the cookie write-back, the refusals, and what each reader command prints
//! from the synthetic answers in `tests/fixtures/graphql`. Nothing reads the
//! real keychain or reaches the network.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flywire::cli::{AccountArg, ApiMethod, DownloadArgs};
use flywire::client::{api_request, ApiRequest};
use flywire::portal::{Env, Site, Warnings};
use flywire::readers::{self, Downloaded};
use flywire::session::{Session, SessionStore};
use flywire::site::Provider;
use pk_cli_auth::vault::{MemoryKeys, Vault};
use pk_cli_config::profile::ProfileName;
use pk_cli_core::RangeArgs;
use pk_cli_secrets::Secret;
use pk_med_records::{Item, RecordsReader};
use serde_json::{json, Value};

/// One request the server saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    cookie: Option<String>,
    user_agent: Option<String>,
    source: Option<String>,
    body: String,
}

/// A loopback server. A `GET` is answered by its path from `routes`
/// (`GET /path`); a `POST /graphql` by its `operationName`, first with the
/// id its variables name (`POST graphql <operationName> <id>`: `id`,
/// `statementId` or the first of `payableIds`), then without
/// (`POST graphql <operationName>`); anything else is a 404. A `POST
/// /graphql` without `source: patient-client` is answered as the portal
/// answers it, HTTP 400 `{"errors":"no source found"}`. `set_cookie` goes on
/// every answer.
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
                source: header("source"),
                body: body.clone(),
            });
            let sourceless =
                method == "POST" && target == "/graphql" && header("source").as_deref() != Some("patient-client");
            let keys = if method == "POST" && target == "/graphql" {
                let doc = serde_json::from_str::<Value>(&body).unwrap_or_default();
                let op = doc["operationName"].as_str().unwrap_or_default().to_string();
                let vars = &doc["variables"];
                let id = [&vars["id"], &vars["statementId"], &vars["payableIds"][0]]
                    .into_iter()
                    .find_map(|v| v.as_str().map(str::to_string));
                let mut keys = Vec::new();
                if let Some(id) = id {
                    keys.push(format!("POST graphql {op} {id}"));
                }
                keys.push(format!("POST graphql {op}"));
                keys
            } else {
                vec![format!("{method} {target}")]
            };
            let (status, mime, body) = if sourceless {
                (400, JSON, r#"{"errors":"no source found"}"#.to_string())
            } else {
                keys.iter().find_map(|k| routes.get(k).cloned()).unwrap_or((
                    404,
                    "text/html",
                    "<p>not found</p>".to_string(),
                ))
            };
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
        warnings: Warnings::kept(),
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
    assert_eq!(seen[0].source.as_deref(), Some("patient-client"));
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
        warnings: Warnings::kept(),
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

// ------------------------------------------------------------- readers

/// Every answer in `tests/fixtures/graphql`: `<operationName>.json`, or
/// `<operationName>.<id>.json` for the one id it answers. `signedOut.json`
/// and `error.json` answer only where a test puts them.
fn fixture_routes() -> Routes {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graphql");
    let mut r = Routes::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        if stem == "signedOut" || stem == "error" {
            continue;
        }
        let key = match stem.split_once('.') {
            Some((op, id)) => format!("POST graphql {op} {id}"),
            None => format!("POST graphql {stem}"),
        };
        r.insert(key, (200, JSON, std::fs::read_to_string(&path).unwrap()));
    }
    r
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/graphql")
            .join(name),
    )
    .unwrap()
}

/// The requests the server saw, as `(operationName, variables)`.
fn operations(server: &Server) -> Vec<(String, Value)> {
    server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            let v: Value = serde_json::from_str(&s.body).unwrap();
            (v["operationName"].as_str().unwrap().to_string(), v["variables"].clone())
        })
        .collect()
}

fn range(since: Option<&str>, until: Option<&str>) -> RangeArgs {
    RangeArgs {
        since: since.map(str::to_string),
        until: until.map(str::to_string),
        ..RangeArgs::default()
    }
}

fn ids(v: &Value, key: &str) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i[key].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn patients_list_is_the_one_patient_the_sign_in_is() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "patients");
    let site = Site::open(&env, &adult1()).unwrap();
    let v = readers::patients(&site).unwrap();
    assert_eq!(v["schema"], "patient-list/v1");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let id = items[0]["id"].as_str().unwrap();
    assert!(id.starts_with("patient-exampleclinic-"), "{id}");
    assert!(!id.contains("PAY-"), "the portal's account id never shows: {id}");
    assert_eq!(items[0]["organization"], "org-exampleclinic");
    assert_eq!(items[0]["relationship"], "self");
    assert!(items[0].get("name").is_none_or(Value::is_null));
    // Stable: the same sign-in is the same patient.
    assert_eq!(readers::patients(&site).unwrap()["items"][0]["id"], id);
    let ops = operations(&server);
    assert_eq!(ops[0], ("payables".into(), json!({"filterClosedBills": false})));
    assert_eq!(
        ops[1],
        ("payableGuarantors".into(), json!({"filterClosedBills": false}))
    );
}

fn patient_id(site: &Site) -> String {
    readers::patients(site).unwrap()["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn the_patient_id_holds_while_accounts_come_and_go() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "patient-id");
    let id = patient_id(&Site::open(&env, &adult1()).unwrap());

    // A new account whose id sorts first, and the first one gone.
    let mut routes = fixture_routes();
    for (op, file) in [
        ("payables", "payables.json"),
        ("payableGuarantors", "payableGuarantors.json"),
    ] {
        let mut v: Value = serde_json::from_str(&fixture(file)).unwrap();
        let list = v["data"]["payables"]["payables"].as_array_mut().unwrap();
        let mut added = list[1].clone();
        added["id"] = "PAY-0000000".into();
        list.remove(0);
        list.push(added);
        routes.insert(format!("POST graphql {op}"), (200, JSON, v.to_string()));
    }
    let changed = serve(routes, None);
    let env2 = self::env(&changed, "patient-id-changed");
    assert_eq!(patient_id(&Site::open(&env2, &adult1()).unwrap()), id);

    // Another profile is another patient id, so a profile reused for
    // another person never merges two people's records.
    let other = ProfileName::parse("adult2").unwrap();
    env.sessions
        .put(&other, &session("_simplee_store2=dummy1; dummy_other=x"))
        .unwrap();
    assert_ne!(patient_id(&Site::open(&env, &other).unwrap()), id);
}

#[test]
fn no_guarantor_or_no_accounts_is_no_patient_id() {
    let guarantors = fixture("payableGuarantors.json");
    let none = r#"{"data":{"payables":{"payables":[]}}}"#.to_string();
    let cases = [
        // No guarantor named, the guarantor query refused, no accounts.
        ("payableGuarantors", guarantors.replace("\"GUAR-0000001\"", "null"), 5),
        ("payableGuarantors", fixture("error.json"), 5),
        ("payables", none, 4),
    ];
    for (op, answer, code) in cases {
        let mut routes = fixture_routes();
        routes.insert(format!("POST graphql {op}"), (200, JSON, answer));
        let server = serve(routes, None);
        let env = env(&server, "no-patient");
        let site = Site::open(&env, &adult1()).unwrap();
        assert_eq!(readers::patients(&site).err().unwrap().exit_code(), code);
        let mut out = Vec::new();
        let e = readers::export(&site, &server.base, &mut out, None, None, None)
            .err()
            .unwrap();
        assert_eq!(e.exit_code(), code, "{e}");
        assert!(out.is_empty());
        // The other readers need no patient id, and never ask for it.
        readers::accounts_list(&site).unwrap();
        readers::bills_list(&site, None, false, &RangeArgs::default()).unwrap();
        if op == "payableGuarantors" {
            let e = readers::patients(&site).err().unwrap();
            assert!(e.to_string().contains("named no guarantor"), "{e}");
        }
    }
    // A portal that fails the query is its own error, which says to retry.
    let mut routes = fixture_routes();
    routes.insert(
        "POST graphql payableGuarantors".into(),
        (503, "text/html", "<p>unavailable</p>".into()),
    );
    let server = serve(routes, None);
    let failing = env(&server, "guarantor-503");
    let site = Site::open(&failing, &adult1()).unwrap();
    let e = readers::patients(&site).err().unwrap();
    assert_eq!(e.exit_code(), 5);
    assert!(e.to_string().contains("HTTP 503"), "{e}");
    assert!(!e.to_string().contains("guarantor"), "{e}");
    let server = serve(fixture_routes(), None);
    let env = env(&server, "no-guarantor-read");
    let site = Site::open(&env, &adult1()).unwrap();
    readers::accounts_list(&site).unwrap();
    readers::payments_list(&site, None, &RangeArgs::default()).unwrap();
    readers::documents(&site, None, &RangeArgs::default()).unwrap();
    assert!(!operations(&server).iter().any(|(op, _)| op == "payableGuarantors"));
}

#[test]
fn accounts_list_shows_what_is_owed_on_each() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "accounts");
    let site = Site::open(&env, &adult1()).unwrap();
    let v = readers::accounts_list(&site).unwrap();
    assert_eq!(v["schema"], "account-list/v1");
    assert_eq!(
        v["items"],
        json!([
            {"id": "PAY-0000001", "number": "ACCT-0000001", "provider": "Example Clinic", "owes": "45.00",
             "pending_insurance": "0.00", "bills": 3, "open_bills": 1},
            {"id": "PAY-0000002", "number": "ACCT-0000002", "provider": "Example Clinic", "owes": "238.49",
             "pending_insurance": "77.22", "bills": 3, "open_bills": 2},
        ])
    );
    // The answers agree: an account's standing amount is its bills' amounts
    // due, and the sign-in's is the accounts'.
    let answer = |name: &str| serde_json::from_str::<Value>(&fixture(name)).unwrap();
    let accounts = answer("payables.json")["data"]["payables"]["payables"].clone();
    let mut standing = 0;
    for a in accounts.as_array().unwrap() {
        let due = a["bills"].as_array().unwrap().iter().map(|b| b["amountDue"].as_i64());
        let due: i64 = due.map(Option::unwrap).sum();
        assert_eq!(a["standingAmount"], due, "{}", a["id"]);
        standing += due;
    }
    assert_eq!(
        answer("payableStandingAmount.json")["data"]["payableStandingAmount"],
        standing
    );
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].cookie.as_deref(), Some("_simplee_store2=dummy1; dummy_other=x"));
}

#[test]
fn bills_list_is_newest_first_and_filters() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "bills");
    let site = Site::open(&env, &adult1()).unwrap();
    let v = readers::bills_list(&site, None, false, &RangeArgs::default()).unwrap();
    assert_eq!(v["schema"], "bill-list/v1");
    assert_eq!(
        ids(&v, "id"),
        [
            "BILL-0000003",
            "BILL-0000002",
            "BILL-0000001",
            "BILL-0000004",
            "BILL-0000006",
            "BILL-0000005"
        ]
    );
    assert_eq!(
        v["items"][2],
        json!({"id": "BILL-0000001", "account": "ACCT-0000001", "date": "2026-01-05", "description": "Office visit",
               "owes": "45.00", "pending_insurance": "0.00", "status": "open", "claim": "CLM-0000001",
               "visit_type": "PB", "open": true})
    );
    assert_eq!(
        v["items"][5],
        json!({"id": "BILL-0000005", "account": "ACCT-0000002", "date": "2021-09-20", "description": "Fee B",
               "owes": "67.13", "pending_insurance": "77.22", "status": "open", "claim": null,
               "visit_type": "PB", "open": true})
    );
    // A date-time is its day; a null amount is null.
    assert_eq!(v["items"][1]["date"], "2026-01-10");
    assert_eq!(v["items"][0]["pending_insurance"], Value::Null);

    let open = readers::bills_list(&site, None, true, &RangeArgs::default()).unwrap();
    assert_eq!(ids(&open, "id"), ["BILL-0000001", "BILL-0000004", "BILL-0000005"]);
    let ranged = readers::bills_list(&site, None, false, &range(Some("2026-1-10"), Some("2026-01-15"))).unwrap();
    assert_eq!(ids(&ranged, "id"), ["BILL-0000003", "BILL-0000002"]);
    let one = readers::bills_list(&site, Some("ACCT-0000002"), false, &RangeArgs::default()).unwrap();
    assert_eq!(ids(&one, "id"), ["BILL-0000004", "BILL-0000006", "BILL-0000005"]);
    let limited = readers::bills_list(
        &site,
        None,
        false,
        &RangeArgs {
            limit: Some(1),
            ..RangeArgs::default()
        },
    );
    assert_eq!(ids(&limited.unwrap(), "id"), ["BILL-0000003"]);
    let e = readers::bills_list(&site, Some("ACCT-0000009"), false, &RangeArgs::default())
        .err()
        .unwrap();
    assert_eq!(e.exit_code(), 4, "{e}");
}

#[test]
fn bills_show_reads_the_bill_s_page() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "bill");
    let site = Site::open(&env, &adult1()).unwrap();
    let v = readers::bill_show(&site, "BILL-0000001", None).unwrap();
    assert_eq!(v.as_object().unwrap().keys().next().map(String::as_str), Some("schema"));
    assert_eq!(v["schema"], "bill/v1");
    assert_eq!(v["visit_number"], "VISIT-0000001");
    assert_eq!(
        v["totals"],
        json!({"billed": "250.00", "insurance_paid": "150.00", "insurance_adjustments": "50.00",
               "patient_paid": "5.00", "patient_adjustments": "0.00", "pending_insurance": "0.00",
               "insurance_responsibility": "150.00", "owes": "45.00"})
    );
    assert_eq!(
        v["plans"],
        json!([{"priority": 1, "insurance": "Example Health Plan PPO", "payer": "Example Health Plan", "plan": "PPO"}])
    );
    assert_eq!(
        v["charges"],
        json!([
            {"date": "2026-01-05", "description": "Office visit, established patient", "code": "99213",
             "code_system": "cpt", "code_display": "Office or outpatient visit", "amount": "200.00"},
            {"date": "2026-01-05", "description": "Blood draw", "code": "36415", "code_system": "cpt",
             "code_display": "Venipuncture", "amount": "50.00"},
        ])
    );
    // A numeric revenue code keeps its leading zero; a null date stays null.
    let lab = readers::bill_show(&site, "BILL-0000002", Some("ACCT-0000001")).unwrap();
    assert_eq!(lab["charges"][0]["code"], "0300");
    assert_eq!(lab["charges"][0]["code_system"], "revenue");
    assert_eq!(lab["charges"][0]["date"], Value::Null);
    assert_eq!(lab["totals"]["patient_adjustments"], Value::Null);
    // A bill that waits on a second plan: the plans by their priority,
    // whatever the answer's order, and what is pending.
    let pending = readers::bill_show(&site, "BILL-0000005", None).unwrap();
    assert_eq!(
        pending["plans"],
        json!([
            {"priority": 1, "insurance": "Example Health Plan PPO", "payer": "Example Health Plan", "plan": "PPO"},
            {"priority": 2, "insurance": null, "payer": "Example Secondary Plan", "plan": null},
        ])
    );
    assert_eq!(
        pending["totals"],
        json!({"billed": "804.69", "insurance_paid": "599.83", "insurance_adjustments": "21.15",
               "patient_paid": "0.00", "patient_adjustments": "39.36", "pending_insurance": "77.22",
               "insurance_responsibility": "677.05", "owes": "67.13"})
    );
    assert_eq!(
        (&pending["charges"][0]["code"], &pending["charges"][1]["code"]),
        (&Value::Null, &json!("12345"))
    );
    // A revenue code given as text reads as one given as a number does.
    let text = readers::bill_show(&site, "BILL-0000006", None).unwrap();
    assert_eq!(
        (&text["charges"][0]["code"], &text["charges"][0]["code_system"]),
        (&json!("1234"), &json!("revenue"))
    );
    let ops = operations(&server);
    assert_eq!(ops[1], ("getSingleBill".into(), json!({"id": "BILL-0000001"})));
    for (id, account) in [("BILL-0000009", None), ("BILL-0000004", Some("ACCT-0000001"))] {
        let e = readers::bill_show(&site, id, account).err().unwrap();
        assert_eq!(e.exit_code(), 4, "{id}: {e}");
    }
}

#[test]
fn payments_list_is_newest_first_settled_or_not() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "payments");
    let site = Site::open(&env, &adult1()).unwrap();
    let v = readers::payments_list(&site, None, &RangeArgs::default()).unwrap();
    assert_eq!(v["schema"], "payment-list/v2");
    assert_eq!(
        readers::PAYMENT_COLUMNS,
        ["date", "account", "amount", "method", "status", "settled", "state"]
    );
    assert_eq!(
        v["items"],
        json!([
            {"account": "ACCT-0000001", "date": "2026-01-30", "amount": "45.00", "method": "Visa",
             "status": "Scheduled", "settled": false, "state": "scheduled"},
            {"account": "ACCT-0000001", "date": "2026-01-05", "amount": "5.00", "method": "Visa",
             "status": "Paid", "settled": true, "state": "settled"},
            {"account": "ACCT-0000002", "date": "2023-08-31", "amount": "70.16", "method": "Discover",
             "status": "Paid", "settled": true, "state": "settled"},
            {"account": "ACCT-0000002", "date": "2022-06-25", "amount": "69.64", "method": "PayPal",
             "status": "Declined", "settled": false, "state": "failed"},
            {"account": "ACCT-0000002", "date": "2022-06-21", "amount": "47.71", "method": "PayPal",
             "status": "Paid", "settled": true, "state": "settled"},
            {"account": "ACCT-0000002", "date": "2022-06-10", "amount": "153.74", "method": "Visa",
             "status": "Failed", "settled": false, "state": "failed"},
            {"account": "ACCT-0000002", "date": "2021-12-05", "amount": "274.16", "method": "Visa",
             "status": "Returned", "settled": false, "state": "failed"},
            {"account": "ACCT-0000002", "date": "2019-07-26", "amount": "62.77", "method": "money_order",
             "status": "Paid", "settled": true, "state": "settled"},
        ])
    );
    let ops = operations(&server);
    assert_eq!(
        ops[1],
        ("previousPayments".into(), json!({"payableIds": ["PAY-0000001"]}))
    );
    let since = readers::payments_list(&site, Some("PAY-0000001"), &range(Some("2026-01-06"), None)).unwrap();
    assert_eq!(ids(&since, "date"), ["2026-01-30"]);
}

#[test]
fn documents_list_shows_the_statements_with_a_file() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "documents");
    let site = Site::open(&env, &adult1()).unwrap();
    let (_, docs) = readers::documents(&site, None, &RangeArgs::default()).unwrap();
    let listed: Vec<(&str, Option<&str>)> = docs.iter().map(|(d, _)| (d.id.as_str(), d.date.as_deref())).collect();
    // STMT-0000002 has no statement id or upload, and STMT-0000004 no
    // upload: the app lists them nowhere.
    assert_eq!(
        listed,
        [
            ("statement-ACCT-0000001-STMT-0000001", Some("2026-01-25")),
            ("statement-ACCT-0000002-STMT-0000003", Some("2019-06-05")),
        ]
    );
    assert_eq!(docs[0].0.category.as_deref(), Some("statement"));
    let ops = operations(&server);
    assert_eq!(
        ops[1],
        (
            "pastStatementsInfo".into(),
            json!({"payableIds": ["PAY-0000001"], "accountNumber": "ACCT-0000001"})
        )
    );
}

fn download_args(id: Option<&str>, all: bool, output: Option<std::path::PathBuf>) -> DownloadArgs {
    DownloadArgs {
        id: id.map(str::to_string),
        all,
        output,
        overwrite: false,
        account: AccountArg { account: None },
        range: RangeArgs::default(),
    }
}

#[test]
fn documents_download_decodes_the_statement_file() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "download");
    let site = Site::open(&env, &adult1()).unwrap();
    let dir = temp_dir("download-out");
    std::fs::create_dir_all(&dir).unwrap();

    let one = readers::download(&site, &download_args(Some("STMT-0000001"), false, Some(dir.clone()))).unwrap();
    let Downloaded::One { saved, as_sent } = one else {
        panic!("{one:?}")
    };
    assert!(!as_sent);
    let name = "2026-01-25_exampleclinic-statement_STMT-0000001.pdf";
    assert_eq!(saved.file.as_deref(), Some(name));
    let bytes = std::fs::read(dir.join(name)).unwrap();
    assert!(bytes.starts_with(b"%PDF-1.4"), "decoded from base64");
    assert!(pk_cli_documents::verify_download(&bytes, Some("pdf")).is_ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // Saving it again over the file needs --overwrite.
    let again = readers::download(&site, &download_args(Some("STMT-0000001"), false, Some(dir.clone())));
    assert_eq!(again.err().unwrap().exit_code(), 2);

    // --all: the PDF already there is skipped, the unrecognized one saved as sent.
    let all = readers::download(&site, &download_args(None, true, Some(dir.clone()))).unwrap();
    let Downloaded::Batch { batch, as_sent } = all else {
        panic!("{all:?}")
    };
    let raw = "2019-06-05_exampleclinic-statement_STMT-0000003.bin";
    assert_eq!(as_sent, [raw]);
    assert_eq!(batch.count, 1);
    assert_eq!(batch.skipped.len(), 1);
    assert_eq!(batch.skipped[0].id, "statement-ACCT-0000001-STMT-0000001");
    assert_eq!(
        std::fs::read_to_string(dir.join(raw)).unwrap(),
        "<html><body>Example Clinic statement</body></html>"
    );
    // A different file at a statement's name is skipped too, and kept.
    std::fs::write(dir.join(raw), "kept").unwrap();
    let all = readers::download(&site, &download_args(None, true, Some(dir.clone()))).unwrap();
    let Downloaded::Batch { batch, .. } = all else {
        panic!("{all:?}")
    };
    assert_eq!((batch.count, batch.skipped.len()), (0, 2));
    let other = &batch.skipped[1];
    assert_eq!(other.id, "statement-ACCT-0000002-STMT-0000003");
    assert!(other.reason.contains("--overwrite"), "{:?}", batch.skipped);
    assert_eq!(std::fs::read_to_string(dir.join(raw)).unwrap(), "kept");
    let e = readers::download(&site, &download_args(Some("STMT-0000002"), false, Some(dir.clone())));
    assert_eq!(
        e.err().unwrap().exit_code(),
        4,
        "a statement with no file is not listed"
    );
    let ops = operations(&server);
    assert!(ops.contains(&("pastStatementsContent".into(), json!({"statementId": "STMT-0000001"}))));
}

fn read_stream(bytes: &[u8]) -> (pk_med_records::Header, Vec<Value>, pk_med_records::Trailer) {
    let mut reader = RecordsReader::new(std::io::BufReader::new(bytes));
    let header = reader.header().unwrap().clone();
    let mut resources = Vec::new();
    let mut trailer = None;
    for item in reader.by_ref() {
        match item.unwrap() {
            Item::Resource(r) => resources.push(r.value().unwrap()),
            Item::Trailer(t) => trailer = Some(t),
        }
    }
    (header, resources, trailer.unwrap())
}

fn component(parts: &Value, code: &str) -> Option<String> {
    parts.as_array()?.iter().find_map(|p| {
        let coding = &p["code"]["coding"][0];
        (coding["system"] == "urn:pk-med:bill-component" && coding["code"] == code)
            .then(|| p["amount"]["value"].to_string())
    })
}

#[test]
fn the_export_is_bills_payments_and_statements_as_fhir() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "export");
    let site = Site::open(&env, &adult1()).unwrap();
    let patient = readers::patients(&site).unwrap()["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut out = Vec::new();
    let finished = readers::export(
        &site,
        &server.base,
        &mut out,
        Some(&patient),
        Some("org-exampleclinic"),
        None,
    )
    .unwrap();
    assert!(finished.trailer.complete);
    let (header, resources, trailer) = read_stream(&out);
    assert_eq!(header.method(), "portal");
    assert_eq!(header.organization.id, "org-exampleclinic");
    assert_eq!(header.organization.name, "Example Clinic");
    assert_eq!(header.patient.id, patient);
    assert_eq!(header.patient.name, None);

    let types: Vec<&str> = resources.iter().map(|r| r["resourceType"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "Organization",
            "Patient",
            "Account",
            "Account",
            "Invoice",
            "Invoice",
            "Invoice",
            "Invoice",
            "Invoice",
            "Invoice",
            "PaymentReconciliation",
            "PaymentReconciliation",
            "PaymentReconciliation",
            "PaymentReconciliation",
            "DocumentReference",
            "Binary",
        ]
    );
    for r in &resources {
        assert_eq!(r["meta"]["source"], "flywire:site", "{}", r["id"]);
        assert_eq!(r["meta"]["tag"][0]["code"], "mapped-from-patient-portal", "{}", r["id"]);
    }
    let by_id = |id: &str| {
        resources
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("{id}"))
    };

    let office = by_id("bill-ACCT-0000001-BILL-0000001");
    assert_eq!(office["status"], "issued");
    assert_eq!(office["subject"]["reference"], format!("Patient/{patient}"));
    assert_eq!(office["account"]["reference"], "Account/account-ACCT-0000001");
    assert_eq!(office["type"]["text"], "Office visit");
    let contained = office["contained"].as_array().unwrap();
    assert_eq!(contained.len(), 2);
    assert_eq!(contained[0]["resourceType"], "ChargeItem");
    assert_eq!(contained[0]["occurrenceDateTime"], "2026-01-05");
    assert_eq!(
        contained[0]["code"]["coding"][0]["system"],
        "http://www.ama-assn.org/go/cpt"
    );
    assert_eq!(contained[0]["code"]["coding"][0]["code"], "99213");
    assert_eq!(office["lineItem"][1]["chargeItemReference"]["reference"], "#charge-2");
    assert_eq!(
        component(&office["lineItem"][0]["priceComponent"], "billed").as_deref(),
        Some("200.00")
    );
    // Money keeps its two places through the stream: a JSON number.
    assert!(office["lineItem"][0]["priceComponent"][0]["amount"]["value"].is_number());
    let totals = &office["totalPriceComponent"];
    assert_eq!(component(totals, "billed").as_deref(), Some("250.00"));
    assert_eq!(component(totals, "insurance-payment").as_deref(), Some("150.00"));
    assert_eq!(component(totals, "insurance-adjustment").as_deref(), Some("50.00"));
    assert_eq!(component(totals, "patient-payment").as_deref(), Some("5.00"));
    assert_eq!(component(totals, "patient-responsibility").as_deref(), Some("45.00"));
    assert_eq!(office["totalNet"]["value"].to_string(), "45.00");
    assert_eq!(office["totalGross"], office["totalNet"], "no tax on a medical bill");
    assert_eq!(
        office["issuer"]["reference"]
            .as_str()
            .map(|r| r.starts_with("Organization/")),
        Some(true)
    );
    assert!(office["issuer"]["display"].is_string());
    let idents: Vec<&str> = office["identifier"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["value"].as_str().unwrap())
        .collect();
    assert_eq!(idents, ["BILL-0000001", "VISIT-0000001", "CLM-0000001"]);

    let lab = by_id("bill-ACCT-0000001-BILL-0000002");
    assert_eq!(lab["status"], "balanced");
    assert_eq!(
        lab["contained"][0]["occurrenceDateTime"], "2026-01-10",
        "a charge with no date is the bill's"
    );
    assert_eq!(lab["contained"][0]["code"]["coding"][0]["code"], "0300");
    let voided = by_id("bill-ACCT-0000001-BILL-0000003");
    assert_eq!(voided["status"], "cancelled");
    assert_eq!(voided["lineItem"][0]["chargeItemReference"]["reference"], "#charge-1");
    assert_eq!(
        voided["contained"][0]["occurrenceDateTime"], "2026-01-15",
        "a bill without lines holds its date of service in its one ChargeItem"
    );
    assert_eq!(
        component(&voided["lineItem"][0]["priceComponent"], "billed").as_deref(),
        Some("120.00")
    );
    assert_eq!(
        component(&voided["totalPriceComponent"], "billed").as_deref(),
        Some("120.00")
    );
    // The second account's bills, each with what is owed on it.
    for (id, status, net) in [
        ("bill-ACCT-0000002-BILL-0000004", "issued", "171.36"),
        ("bill-ACCT-0000002-BILL-0000005", "issued", "67.13"),
        ("bill-ACCT-0000002-BILL-0000006", "balanced", "0.00"),
    ] {
        let bill = by_id(id);
        assert_eq!(bill["status"], status, "{id}");
        assert_eq!(bill["totalNet"]["value"].to_string(), net, "{id}");
        assert_eq!(bill["account"]["reference"], "Account/account-ACCT-0000002", "{id}");
    }

    let payments: Vec<&Value> = resources
        .iter()
        .filter(|r| r["resourceType"] == "PaymentReconciliation")
        .collect();
    let paid: Vec<(String, String)> = payments
        .iter()
        .map(|p| {
            (
                p["paymentDate"].as_str().unwrap().to_string(),
                p["paymentAmount"]["value"].to_string(),
            )
        })
        .collect();
    assert_eq!(
        paid,
        [
            ("2026-01-05".into(), "5.00".into()),
            ("2023-08-31".into(), "70.16".into()),
            ("2022-06-21".into(), "47.71".into()),
            ("2019-07-26".into(), "62.77".into())
        ]
    );
    assert_eq!(payments[0]["disposition"], "Paid by Visa");
    // Neither a card nor described: worded by its type.
    assert_eq!(payments[3]["disposition"], "Paid by money_order");
    // The portal says which provider a payment paid, not which bills.
    assert_eq!(
        payments[0]["detail"][0]["payee"]["display"],
        office["issuer"]["display"]
    );
    assert!(payments[0]["detail"][0].get("request").is_none());

    let doc = by_id("statement-ACCT-0000001-STMT-0000001");
    assert_eq!(doc["date"], "2026-01-25T00:00:00Z");
    assert_eq!(doc["type"]["coding"][0]["system"], "urn:pk-med:doc-type");
    assert_eq!(doc["type"]["coding"][0]["code"], "bill");
    assert_eq!(
        doc["context"]["related"][0]["reference"],
        "Account/account-ACCT-0000001"
    );
    assert_eq!(doc["content"][0]["attachment"]["contentType"], "application/pdf");
    let binary_ref = doc["content"][0]["attachment"]["url"].as_str().unwrap();
    let binary_at = resources
        .iter()
        .position(|r| format!("Binary/{}", r["id"].as_str().unwrap()) == binary_ref);
    let doc_at = resources.iter().position(|r| r["id"] == doc["id"]).unwrap();
    assert_eq!(binary_at, Some(doc_at + 1), "the Binary follows its DocumentReference");
    let binary = &resources[doc_at + 1];
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(binary["data"].as_str().unwrap())
        .unwrap();
    assert!(bytes.starts_with(b"%PDF-1.4"));
    assert_eq!(doc["content"][0]["attachment"]["size"], bytes.len());

    assert_eq!(trailer.counts.get("Invoice"), Some(&6));
    let skipped: Vec<(&str, Option<&str>)> = trailer
        .skipped
        .iter()
        .map(|s| (s.code.as_str(), s.resource_type.as_deref()))
        .collect();
    // One skip per account for the payments that did not settle.
    assert_eq!(
        skipped,
        [
            ("not_settled", Some("PaymentReconciliation")),
            ("not_settled", Some("PaymentReconciliation")),
            ("verify_failed", Some("DocumentReference")),
        ]
    );
    for (i, says) in [
        (
            0,
            "0 payment(s) turned down (declined, failed, voided), 1 scheduled or pending and 0 not shown to have \
             settled were left out",
        ),
        (
            1,
            "3 payment(s) turned down (declined, failed, voided), 0 scheduled or pending and 0 not shown to have \
             settled were left out",
        ),
    ] {
        assert_eq!(trailer.skipped[i].reason, says);
    }
}

#[test]
fn the_export_since_a_day_keeps_the_accounts() {
    for (since, account, kept, left_out, invoices, payments) in [
        ("2026-01-15", "ACCT-0000001", "BILL-0000003", "BILL-0000001", 1, 0),
        ("2022-06-15", "ACCT-0000002", "BILL-0000006", "BILL-0000005", 5, 3),
    ] {
        let server = serve(fixture_routes(), None);
        let env = env(&server, "export-since");
        let site = Site::open(&env, &adult1()).unwrap();
        let mut out = Vec::new();
        readers::export(&site, &server.base, &mut out, None, None, Some(since)).unwrap();
        let (header, resources, _) = read_stream(&out);
        assert_eq!(header.since.as_deref(), Some(since));
        let ids: Vec<&str> = resources.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"account-ACCT-0000001") && ids.contains(&"account-ACCT-0000002"));
        assert!(ids.contains(&format!("bill-{account}-{kept}").as_str()));
        assert!(!ids.contains(&format!("bill-{account}-{left_out}").as_str()));
        let count = |t: &str| resources.iter().filter(|r| r["resourceType"] == t).count();
        assert_eq!(
            (count("Invoice"), count("PaymentReconciliation")),
            (invoices, payments),
            "{since}"
        );
        // Bills before the day are not read at all.
        assert!(!operations(&server).contains(&("getSingleBill".into(), json!({"id": left_out}))));
    }
}

#[test]
fn the_export_refuses_another_patient_or_provider_before_the_stream() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "export-ids");
    let site = Site::open(&env, &adult1()).unwrap();
    for (patient, org) in [(Some("patient-other"), None), (None, Some("org-other"))] {
        let mut out = Vec::new();
        let e = readers::export(&site, &server.base, &mut out, patient, org, None)
            .err()
            .unwrap();
        assert_eq!(e.exit_code(), 4, "{e}");
        assert!(out.is_empty());
    }
}

#[test]
fn a_statement_with_no_file_is_skipped_in_the_export() {
    let mut routes = fixture_routes();
    routes.insert(
        "POST graphql pastStatementsContent STMT-0000001".into(),
        (
            200,
            JSON,
            r#"{"data":{"pastStatementsContent":{"imageFormat":null,"preview":null}}}"#.into(),
        ),
    );
    let server = serve(routes, None);
    let env = env(&server, "export-no-file");
    let site = Site::open(&env, &adult1()).unwrap();
    let mut out = Vec::new();
    let finished = readers::export(&site, &server.base, &mut out, None, None, None).unwrap();
    assert!(finished.trailer.complete);
    let (_, resources, trailer) = read_stream(&out);
    assert!(!resources.iter().any(|r| r["resourceType"] == "DocumentReference"));
    let codes: Vec<&str> = trailer.skipped.iter().map(|s| s.code.as_str()).collect();
    assert_eq!(codes, ["not_settled", "not_settled", "no_file", "verify_failed"]);
}

// ------------------------------------------- bills and payments, by account

/// One bill: what is due on it, `null` for a bill that does not say.
fn bill_of(id: &str, date: &str, due: Value, status: &str) -> Value {
    json!({ "id": id, "admissionDate": date, "amountDue": due, "displayName": "Fee A", "status": status })
}

/// One payment, with a status or without (`null`).
fn payment_of(date: &str, amount: i64, status: Value, method: &str) -> Value {
    json!({ "amount": amount, "date": date, "status": status, "paymentMethod": { "paymentType": method } })
}

/// A sign-in of the accounts `accounts`, each a letter (`PAY-A`, `ACCT-A`),
/// its standing amount, its bills and its payments, with no statements. Every
/// bill's page is the same.
fn accounts_of(accounts: &[(&str, Value, Vec<Value>, Vec<Value>)]) -> Routes {
    let mut r = fixture_routes();
    let mut put = |key: String, data: Value| r.insert(key, (200, JSON, json!({ "data": data }).to_string()));
    let mut payables = Vec::new();
    for (letter, standing, bills, payments) in accounts {
        payables.push(json!({
            "id": format!("PAY-{letter}"),
            "accountNumber": format!("ACCT-{letter}"),
            "standingAmount": standing,
            "bills": bills,
            "payee": { "companyDisplayName": "Example Clinic" },
        }));
        put(
            format!("POST graphql previousPayments PAY-{letter}"),
            json!({ "previousPayments": payments }),
        );
    }
    put(
        "POST graphql payables".into(),
        json!({ "payables": { "payables": payables } }),
    );
    put(
        "POST graphql pastStatementsInfo".into(),
        json!({ "pastStatementsInfo": [] }),
    );
    let charges = json!([{ "amount": 44490, "date": null, "description": "Note 1" }]);
    put(
        "POST graphql getSingleBill".into(),
        json!({ "bill": { "aTotalBilled": 44490, "charges": charges } }),
    );
    r
}

fn payments_a() -> Vec<Value> {
    vec![
        payment_of("2021-07-04T06:55:08Z", 27756, json!("Paid"), "cash"),
        payment_of("2024-03-26T01:29:37Z", 56917, json!("Posted"), "check"),
        payment_of("2019-03-31T09:37:36Z", 72129, json!("Voided"), "check"),
        payment_of("2020-08-25T13:00:36Z", 18640, json!("Example word"), "cash"),
        payment_of("2019-12-11T21:58:02Z", 53312, json!("Pending"), "money_order"),
    ]
}

/// Two bills that do not say what is due on them, one for want of the field;
/// three that do, 242.60 in all; and two voided, one of which says 639.86 and
/// one nothing.
fn bills_a() -> Vec<Value> {
    let mut missing = bill_of("BILL-D", "2022-02-11", json!(null), "open");
    missing.as_object_mut().unwrap().remove("amountDue");
    vec![
        bill_of("BILL-A", "2024-11-02", json!(18869), "open"),
        bill_of("BILL-B", "2020-03-28", json!(0), "closed"),
        bill_of("BILL-C", "2021-03-12", json!(63986), "Voided"),
        missing,
        bill_of("BILL-E", "2021-09-18", json!(null), "example"),
        bill_of("BILL-F", "2024-05-30", json!(5391), "open"),
        bill_of("BILL-G", "2021-02-13", json!(null), "cancelled"),
    ]
}

fn payments_b() -> Vec<Value> {
    vec![
        payment_of("2021-04-30T20:36:58Z", 88979, json!("Paid"), "check"),
        payment_of("2999-08-18", 77334, json!("Scheduled"), "cash"),
        payment_of("2021-05-21T05:32:34Z", 30673, json!("Settled"), "cash"),
        payment_of("2022-03-09T10:32:26Z", 63146, json!("Voided"), "cash"),
        payment_of("2019-07-29T00:56:34Z", 54397, json!("Refunded"), "cash"),
        payment_of("2024-10-02T22:49:26Z", 3623, json!("Example word"), "check"),
        payment_of("2999-11-24", 8769, json!("Paid"), "money_order"),
        payment_of("2023-02-12T03:35:49Z", 22780, json!("Posted"), "cash"),
        payment_of("2024-04-13T12:54:44Z", 15064, json!("Authorized"), "cash"),
        payment_of("2019-02-12T17:25:07Z", 86936, json!("Processing"), "money_order"),
        payment_of("2019-01-17T08:16:41Z", 8823, json!("Declined"), "check"),
        payment_of("2022-02-11T03:29:08Z", 67043, json!(null), "money_order"),
        payment_of("2999-12-24", 2419, json!("Cancelled"), "money_order"),
        payment_of("2019-02-07T14:01:56Z", 47677, json!("Charged Back"), "cash"),
        payment_of("2019-04-13T09:09:56Z", 83179, json!(""), "money_order"),
    ]
}

/// Four bills, none of which says what is due on it.
fn bills_b() -> Vec<Value> {
    [
        ("BILL-H", "2022-08-20"),
        ("BILL-I", "2020-08-26"),
        ("BILL-J", "2023-06-14"),
        ("BILL-K", "2019-12-18"),
    ]
    .map(|(id, date)| bill_of(id, date, json!(null), "open"))
    .to_vec()
}

/// What a list says is owed on each, by id.
fn owed(v: &Value) -> Vec<(String, Value)> {
    let row = |i: &Value| (i["id"].as_str().unwrap().to_string(), i["owes"].clone());
    v["items"].as_array().unwrap().iter().map(row).collect()
}

/// Each payment's state is its status's, or its day's when it is after
/// today; the export writes the settled ones and counts the rest by state.
#[test]
fn a_payment_is_settled_only_by_a_status_that_says_so() {
    let bills = [
        bill_of("BILL-H", "2022-08-20", json!(45908), "open"),
        bill_of("BILL-I", "2020-08-26", json!(0), "closed"),
        bill_of("BILL-J", "2023-06-14", json!(10636), "open"),
        bill_of("BILL-K", "2019-12-18", json!(0), "voided"),
    ];
    let server = serve(accounts_of(&[("B", json!(56544), bills.to_vec(), payments_b())]), None);
    let env = env(&server, "states");
    let site = Site::open(&env, &adult1()).unwrap();
    let v = readers::payments_list(&site, None, &RangeArgs::default()).unwrap();
    assert_eq!(v["schema"], "payment-list/v2");
    let rows: Vec<Value> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            assert_eq!(p["settled"], p["state"] == "settled", "{p}");
            json!([p["date"], p["status"], p["state"]])
        })
        .collect();
    #[rustfmt::skip]
    let want = json!([
        ["2999-12-24", "Cancelled", "failed"],
        ["2999-11-24", "Paid", "scheduled"],
        ["2999-08-18", "Scheduled", "scheduled"],
        ["2024-10-02", "Example word", "unconfirmed"],
        ["2024-04-13", "Authorized", "pending"],
        ["2023-02-12", "Posted", "settled"],
        ["2022-03-09", "Voided", "failed"],
        ["2022-02-11", null, "unconfirmed"],
        ["2021-05-21", "Settled", "settled"],
        ["2021-04-30", "Paid", "settled"],
        ["2019-07-29", "Refunded", "failed"],
        ["2019-04-13", null, "unconfirmed"],
        ["2019-02-12", "Processing", "pending"],
        ["2019-02-07", "Charged Back", "failed"],
        ["2019-01-17", "Declined", "failed"],
    ]);
    assert_eq!(json!(rows), want);

    let mut out = Vec::new();
    readers::export(&site, &server.base, &mut out, None, None, None).unwrap();
    let (_, resources, trailer) = read_stream(&out);
    let paid: Vec<&Value> = resources
        .iter()
        .filter(|r| r["resourceType"] == "PaymentReconciliation")
        .map(|r| &r["paymentDate"])
        .collect();
    assert_eq!(paid, ["2023-02-12", "2021-05-21", "2021-04-30"]);
    let skipped: Vec<(&str, &str)> = trailer
        .skipped
        .iter()
        .map(|s| (s.code.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(
        skipped,
        [(
            "not_settled",
            "5 payment(s) turned down (declined, failed, voided), 4 scheduled or pending and 3 not shown to have \
             settled were left out"
        )]
    );
    assert!(env.warnings.lines().is_empty(), "{:?}", env.warnings.lines());
}

/// The bills that say what is due on them account for the account's standing
/// amount, so the others are owed 0. A voided bill is neither open nor owed,
/// whatever it says.
#[test]
fn bills_that_do_not_say_are_owed_0_when_the_rest_account_for_the_account() {
    let server = serve(accounts_of(&[("A", json!(24260), bills_a(), payments_a())]), None);
    let env = env(&server, "settled-by-standing");
    let site = Site::open(&env, &adult1()).unwrap();
    let a = &readers::accounts_list(&site).unwrap()["items"][0];
    assert_eq!((a["owes"].as_str(), a["bills"].as_u64()), (Some("242.60"), Some(7)));
    assert_eq!(a["open_bills"], 2);
    let bills = readers::bills_list(&site, None, false, &RangeArgs::default()).unwrap();
    assert_eq!(
        json!(owed(&bills)),
        json!([
            ["BILL-A", "188.69"],
            ["BILL-F", "53.91"],
            ["BILL-D", "0.00"],
            ["BILL-E", "0.00"],
            ["BILL-C", "639.86"],
            ["BILL-G", null],
            ["BILL-B", "0.00"],
        ])
    );
    for b in bills["items"].as_array().unwrap() {
        assert_eq!(b["open"], b["id"] == "BILL-A" || b["id"] == "BILL-F", "{b}");
    }
    let open = readers::bills_list(&site, None, true, &RangeArgs::default()).unwrap();
    assert_eq!(ids(&open, "id"), ["BILL-A", "BILL-F"]);
    let shown = readers::bill_show(&site, "BILL-E", None).unwrap();
    assert_eq!(
        (&shown["owes"], &shown["totals"]["owes"]),
        (&json!("0.00"), &json!("0.00"))
    );
    assert_eq!(readers::bill_show(&site, "BILL-C", None).unwrap()["open"], false);

    let mut out = Vec::new();
    readers::export(&site, &server.base, &mut out, None, None, None).unwrap();
    let (_, resources, trailer) = read_stream(&out);
    let invoices: Vec<Value> = resources
        .iter()
        .filter(|r| r["resourceType"] == "Invoice")
        .map(|i| json!([i["identifier"][0]["value"], i["status"]]))
        .collect();
    assert_eq!(
        json!(invoices),
        json!([
            ["BILL-A", "issued"],
            ["BILL-F", "issued"],
            ["BILL-D", "balanced"],
            ["BILL-E", "balanced"],
            ["BILL-C", "cancelled"],
            ["BILL-G", "cancelled"],
            ["BILL-B", "balanced"],
        ])
    );
    assert!(!trailer.skipped.iter().any(|s| s.code == "unreadable"));
    assert!(env.warnings.lines().is_empty(), "{:?}", env.warnings.lines());
}

/// Otherwise they stay unsaid, a cent off included: `null` and one warning,
/// without a figure, and a skip in place of each one's Invoice.
#[test]
fn bills_that_do_not_say_stay_unsaid_unless_the_rest_account_for_the_account() {
    for standing in [
        json!(24261),
        json!(24259),
        // With what the voided bill says.
        json!(88246),
        json!(47870),
        json!(0),
        json!(-4335),
        json!(null),
    ] {
        let server = serve(accounts_of(&[("A", standing.clone(), bills_a(), payments_a())]), None);
        let env = env(&server, "unsaid");
        let site = Site::open(&env, &adult1()).unwrap();
        let warned = || env.warnings.lines().len();
        let a = &readers::accounts_list(&site).unwrap()["items"][0];
        // The account's total is the portal's own, whatever its bills say.
        let total = standing
            .as_i64()
            .map(|c| format!("{}{}.{:02}", if c < 0 { "-" } else { "" }, c.abs() / 100, c.abs() % 100));
        assert_eq!((&a["owes"], &a["open_bills"]), (&json!(total), &json!(2)), "{standing}");
        assert_eq!(warned(), 1);
        let bills = readers::bills_list(&site, None, false, &RangeArgs::default()).unwrap();
        assert_eq!(
            json!(owed(&bills)),
            json!([
                ["BILL-A", "188.69"],
                ["BILL-F", "53.91"],
                ["BILL-D", null],
                ["BILL-E", null],
                ["BILL-C", "639.86"],
                ["BILL-G", null],
                ["BILL-B", "0.00"],
            ]),
            "{standing}"
        );
        assert_eq!(warned(), 2);
        let open = readers::bills_list(&site, None, true, &RangeArgs::default()).unwrap();
        assert_eq!(ids(&open, "id"), ["BILL-A", "BILL-F"], "{standing}");
        assert_eq!(warned(), 3, "the list that leaves them out says so too");
        for id in ["BILL-D", "BILL-E"] {
            let shown = readers::bill_show(&site, id, None).unwrap();
            assert_eq!((&shown["owes"], &shown["open"]), (&json!(null), &json!(false)), "{id}");
            assert!(shown["totals"]["owes"].is_null(), "{id}");
        }
        assert_eq!(warned(), 5);
        // A bill that says, and a voided one, are known.
        for id in ["BILL-A", "BILL-C", "BILL-G"] {
            readers::bill_show(&site, id, None).unwrap();
        }
        assert_eq!(warned(), 5);
        for w in env.warnings.lines() {
            assert!(w.starts_with("2 bill(s) do not say what is owed on them"), "{w}");
            assert_eq!(w.chars().filter(char::is_ascii_digit).count(), 1, "no figures: {w}");
        }

        let mut out = Vec::new();
        let finished = readers::export(&site, &server.base, &mut out, None, None, None).unwrap();
        assert!(finished.trailer.complete);
        let (_, resources, trailer) = read_stream(&out);
        let invoices: Vec<Value> = resources
            .iter()
            .filter(|r| r["resourceType"] == "Invoice")
            .map(|i| json!([i["identifier"][0]["value"], i["status"], i.get("totalNet").is_some()]))
            .collect();
        assert_eq!(
            json!(invoices),
            json!([
                ["BILL-A", "issued", true],
                ["BILL-F", "issued", true],
                ["BILL-C", "cancelled", true],
                ["BILL-G", "cancelled", false],
                ["BILL-B", "balanced", true],
            ])
        );
        let skipped: Vec<(Option<&str>, Option<&str>, &str)> = trailer
            .skipped
            .iter()
            .filter(|s| s.code == "unreadable")
            .map(|s| (s.resource_type.as_deref(), s.id.as_deref(), s.reason.as_str()))
            .collect();
        let why = "the bill does not say what is owed on it";
        assert_eq!(
            skipped,
            [
                (Some("Invoice"), Some("bill-ACCT-A-BILL-D"), why),
                (Some("Invoice"), Some("bill-ACCT-A-BILL-E"), why),
            ]
        );
        assert_eq!(trailer.skipped.len(), 3, "and the payments that did not settle");
    }
}

/// `--since` leaves out a bill before its day, said or not.
#[test]
fn a_bill_that_does_not_say_before_since_is_no_skip() {
    let server = serve(accounts_of(&[("A", json!(null), bills_a(), payments_a())]), None);
    let env = env(&server, "unsaid-since");
    let site = Site::open(&env, &adult1()).unwrap();
    let mut out = Vec::new();
    readers::export(&site, &server.base, &mut out, None, None, Some("2021-12-30")).unwrap();
    let (_, resources, trailer) = read_stream(&out);
    let count = resources.iter().filter(|r| r["resourceType"] == "Invoice").count();
    assert_eq!(count, 2);
    let skipped: Vec<Option<&str>> = trailer
        .skipped
        .iter()
        .filter(|s| s.resource_type.as_deref() == Some("Invoice"))
        .map(|s| s.id.as_deref())
        .collect();
    assert_eq!(skipped, [Some("bill-ACCT-A-BILL-D")]);
}

/// Each account is settled, or warned about, by its own standing amount. An
/// account none of whose bills says owes nothing on any when it owes nothing.
#[test]
fn each_account_s_bills_are_settled_by_its_own_standing_amount() {
    let nothing: [(&str, Value); 4] = ["BILL-J", "BILL-H", "BILL-I", "BILL-K"].map(|id| (id, json!("0.00")));
    let unsaid: [(&str, Value); 4] = ["BILL-J", "BILL-H", "BILL-I", "BILL-K"].map(|id| (id, json!(null)));
    for (standing, owes, listed, warnings) in [
        (json!(0), json!("0.00"), &nothing, &["2 bill(s)"][..]),
        (json!(39401), json!("394.01"), &unsaid, &["2 bill(s)", "4 bill(s)"]),
        (json!(null), json!(null), &unsaid, &["2 bill(s)", "4 bill(s)"]),
    ] {
        let server = serve(
            accounts_of(&[
                ("A", json!(59216), bills_a(), payments_a()),
                ("B", standing.clone(), bills_b(), payments_b()),
            ]),
            None,
        );
        let env = env(&server, "unsaid-accounts");
        let site = Site::open(&env, &adult1()).unwrap();
        let accounts = readers::accounts_list(&site).unwrap();
        assert_eq!(
            json!(owed(&accounts)),
            json!([["PAY-A", "592.16"], ["PAY-B", owes]]),
            "{standing}"
        );
        let said: Vec<String> = env
            .warnings
            .lines()
            .iter()
            .map(|w| w.split(" do not say what is owed on them").next().unwrap().to_string())
            .collect();
        assert_eq!(said, warnings, "one for each account: {standing}");
        // `--account` reads, and warns of, the one it names.
        let one = readers::bills_list(&site, Some("ACCT-B"), false, &RangeArgs::default()).unwrap();
        assert_eq!(json!(owed(&one)), json!(listed), "{standing}");
        assert_eq!(env.warnings.lines().len(), 2 * warnings.len() - 1);
        for b in one["items"].as_array().unwrap() {
            assert_eq!(b["open"], false, "{b}");
        }
    }
}

#[test]
fn a_signed_out_portal_is_exit_3_before_anything_is_written() {
    let mut routes = fixture_routes();
    routes.insert("POST graphql payables".into(), (200, JSON, fixture("signedOut.json")));
    let server = serve(routes, None);
    let env = env(&server, "reader-signed-out");
    let site = Site::open(&env, &adult1()).unwrap();
    let e = readers::accounts_list(&site).err().unwrap();
    assert_eq!(e.exit_code(), 3, "{e}");
    assert!(
        e.to_string().contains("--provider exampleclinic --profile adult1"),
        "{e}"
    );
    let mut out = Vec::new();
    let e = readers::export(&site, &server.base, &mut out, None, None, None)
        .err()
        .unwrap();
    assert_eq!(e.exit_code(), 3);
    assert!(out.is_empty(), "no header before the session is known good");
}

#[test]
fn a_graphql_error_on_one_bill_is_exit_5_and_a_skip_in_the_export() {
    let mut routes = fixture_routes();
    routes.insert(
        "POST graphql getSingleBill BILL-0000001".into(),
        (200, JSON, fixture("error.json")),
    );
    let server = serve(routes, None);
    let env = env(&server, "reader-error");
    let site = Site::open(&env, &adult1()).unwrap();
    let e = readers::bill_show(&site, "BILL-0000001", None).err().unwrap();
    assert_eq!(e.exit_code(), 5, "{e}");
    assert!(
        !e.to_string().contains("could not be loaded"),
        "the error's text is not repeated: {e}"
    );
    let mut out = Vec::new();
    let finished = readers::export(&site, &server.base, &mut out, None, None, None).unwrap();
    assert!(finished.trailer.complete, "one bill does not stop the rest");
    let (_, resources, trailer) = read_stream(&out);
    assert_eq!(trailer.counts.get("Invoice"), Some(&5));
    assert_eq!(trailer.counts.get("PaymentReconciliation"), Some(&4));
    assert_eq!(trailer.counts.get("DocumentReference"), Some(&1));
    assert!(!resources.iter().any(|r| r["id"] == "bill-ACCT-0000001-BILL-0000001"));
    let skip = &trailer.skipped[0];
    assert_eq!(
        (skip.code.as_str(), skip.id.as_deref()),
        ("upstream", Some("bill-ACCT-0000001-BILL-0000001"))
    );
    assert!(skip.reason.contains("1 GraphQL error"), "{}", skip.reason);
    assert!(!skip.reason.contains("could not be loaded"), "{}", skip.reason);
}

#[test]
fn a_bill_answered_without_the_bill_is_exit_5_and_a_skip_in_the_export() {
    let mut routes = fixture_routes();
    routes.insert(
        "POST graphql getSingleBill BILL-0000001".into(),
        (200, JSON, r#"{"data":{"bill":null}}"#.into()),
    );
    let server = serve(routes, None);
    let env = env(&server, "bill-null");
    let site = Site::open(&env, &adult1()).unwrap();
    let e = readers::bill_show(&site, "BILL-0000001", None).err().unwrap();
    assert_eq!(e.exit_code(), 5, "{e}");
    let mut out = Vec::new();
    let finished = readers::export(&site, &server.base, &mut out, None, None, None).unwrap();
    assert!(finished.trailer.complete);
    let (_, resources, trailer) = read_stream(&out);
    assert_eq!(trailer.counts.get("Invoice"), Some(&5));
    assert!(!resources.iter().any(|r| r["id"] == "bill-ACCT-0000001-BILL-0000001"));
    let skip = &trailer.skipped[0];
    assert_eq!(
        (skip.code.as_str(), skip.id.as_deref()),
        ("upstream", Some("bill-ACCT-0000001-BILL-0000001"))
    );
}

#[test]
fn accounts_answered_without_the_list_end_the_export_before_its_header() {
    let mut routes = fixture_routes();
    routes.insert(
        "POST graphql payables".into(),
        (200, JSON, r#"{"data":{"payables":null}}"#.into()),
    );
    let server = serve(routes, None);
    let env = env(&server, "payables-null");
    let site = Site::open(&env, &adult1()).unwrap();
    assert_eq!(
        readers::bills_list(&site, None, false, &RangeArgs::default())
            .err()
            .unwrap()
            .exit_code(),
        5
    );
    let mut out = Vec::new();
    let e = readers::export(&site, &server.base, &mut out, None, None, None)
        .err()
        .unwrap();
    assert_eq!(e.exit_code(), 5, "{e}");
    assert!(out.is_empty(), "no stream claims a sign-in with no accounts");
}

#[test]
fn a_session_that_ends_mid_export_ends_it_incomplete() {
    let mut routes = fixture_routes();
    routes.insert(
        "POST graphql previousPayments PAY-0000001".into(),
        (200, JSON, fixture("signedOut.json")),
    );
    let server = serve(routes, None);
    let env = env(&server, "export-signed-out");
    let site = Site::open(&env, &adult1()).unwrap();
    let mut out = Vec::new();
    let finished = readers::export(&site, &server.base, &mut out, None, None, None).unwrap();
    let error = finished.trailer.error.as_ref().unwrap();
    assert!(!finished.trailer.complete);
    assert_eq!(error.exit_code, Some(3));
    assert_eq!(flywire::app::incomplete_exit(&finished.trailer), 3);
    let text = String::from_utf8(out).unwrap();
    assert!(text.lines().last().unwrap().contains("\"complete\":false"));
    assert!(
        text.contains("\"resourceType\":\"Invoice\""),
        "what was read before it is kept"
    );
}

#[test]
fn an_amount_that_is_not_whole_cents_is_exit_5_naming_the_field() {
    let mut routes = fixture_routes();
    let payables = fixture("payables.json").replacen("\"amountDue\": 4500", "\"amountDue\": 45.5", 1);
    assert!(payables.contains("45.5"));
    routes.insert("POST graphql payables".into(), (200, JSON, payables));
    let server = serve(routes, None);
    let env = env(&server, "reader-amount");
    let site = Site::open(&env, &adult1()).unwrap();
    let e = readers::bills_list(&site, None, false, &RangeArgs::default())
        .err()
        .unwrap();
    assert_eq!(e.exit_code(), 5, "{e}");
    assert!(e.to_string().contains("payables.bills.amountDue"), "{e}");
    assert!(!e.to_string().contains("45.5"), "{e}");
}

/// The binary, against the loopback server: a reader with no profile, or
/// one not configured, is exit 2 before any request. No session is stored
/// and the keychain service holds nothing, so no key is read.
#[test]
fn a_reader_without_its_profile_is_exit_2() {
    let server = serve(fixture_routes(), None);
    let config = temp_dir("config");
    std::fs::create_dir_all(config.join("flywire")).unwrap();
    std::fs::write(
        config.join("flywire/config.json"),
        json!({"profiles": {"adult1": {}}}).to_string(),
    )
    .unwrap();
    let out = temp_dir("records");
    let out = out.to_str().unwrap();
    for (args, profile) in [
        (vec!["patients", "list"], None),
        (vec!["accounts", "list"], None),
        (vec!["bills", "list", "--open"], None),
        (vec!["bills", "show", "BILL-0000001"], None),
        (vec!["payments", "list"], None),
        (vec!["documents", "list"], Some("nobody")),
        (vec!["documents", "download", "--all", "-o", out], None),
        (vec!["records", "export", "-o", out], Some("nobody")),
    ] {
        let mut cmd = assert_cmd::Command::cargo_bin("flywire").unwrap();
        cmd.env("FLYWIRE_KEYCHAIN_SERVICE", "piekstra.flywire.hermetic-tests")
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", temp_dir("data"))
            .env("FLYWIRE_BASE_URL", &server.base)
            .env_remove("FLYWIRE_PROFILE")
            .args(&args)
            .arg("--json");
        if let Some(p) = profile {
            cmd.args(["--profile", p]);
        }
        let assert = cmd.assert().code(2);
        let v: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
        assert_eq!(v["error"]["code"], "usage", "{args:?}");
        assert!(
            v["error"]["message"].as_str().unwrap().contains("adult1"),
            "{args:?}: {v}"
        );
    }
    assert!(server.seen.lock().unwrap().is_empty(), "no request without a profile");
}

#[test]
fn a_request_without_the_source_header_is_refused_as_the_portal_does() {
    let server = serve(routes(), None);
    let answer = reqwest::blocking::Client::new()
        .post(format!("{}/graphql", server.base))
        .header("content-type", "application/json")
        .body(r#"{"operationName":"payableStandingAmount","query":"query payableStandingAmount { payableStandingAmount }","variables":{}}"#)
        .send()
        .unwrap();
    let status = answer.status().as_u16();
    let content_type = answer
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = answer.bytes().unwrap();
    assert_eq!(status, 400);
    match flywire::site::judge(status, content_type.as_deref(), &body) {
        pk_cli_browser::Confirm::Fail(e) => {
            assert_eq!(e.exit_code(), 5);
            assert!(e.to_string().contains("\"source: patient-client\""), "{e}");
        }
        other => panic!("a sourceless answer must fail the check, not {other:?}"),
    }
    assert_eq!(server.seen.lock().unwrap()[0].source, None);
}
