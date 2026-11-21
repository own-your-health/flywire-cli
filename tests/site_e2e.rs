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
use flywire::portal::{Env, Site};
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
    body: String,
}

/// A loopback server. A `GET` is answered by its path from `routes`
/// (`GET /path`); a `POST /graphql` by its `operationName`, first with the
/// id its variables name (`POST graphql <operationName> <id>`: `id`,
/// `statementId` or the first of `payableIds`), then without
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
            let (status, mime, body) = keys.iter().find_map(|k| routes.get(k).cloned()).unwrap_or((
                404,
                "text/html",
                "<p>not found</p>".to_string(),
            ));
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
    let mut payables: Value = serde_json::from_str(&fixture("payables.json")).unwrap();
    let list = payables["data"]["payables"]["payables"].as_array_mut().unwrap();
    let mut added = list[1].clone();
    added["id"] = "PAY-0000000".into();
    list.remove(0);
    list.push(added);
    let mut routes = fixture_routes();
    routes.insert("POST graphql payables".into(), (200, JSON, payables.to_string()));
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
    let no_guarantor = fixture("payables.json").replace("\"externalId\": \"GUAR-0000001\"", "\"externalId\": null");
    let none = r#"{"data":{"payables":{"payables":[]}}}"#.to_string();
    for (payables, code) in [(no_guarantor, 5), (none, 4)] {
        let mut routes = fixture_routes();
        routes.insert("POST graphql payables".into(), (200, JSON, payables));
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
        // The other readers need no patient id.
        readers::accounts_list(&site).unwrap();
    }
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
            {"id": "PAY-0000002", "number": "ACCT-0000002", "provider": "Example Clinic", "owes": "0.00",
             "pending_insurance": "15.00", "bills": 1, "open_bills": 0},
        ])
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
        ["BILL-0000004", "BILL-0000003", "BILL-0000002", "BILL-0000001"]
    );
    assert_eq!(
        v["items"][3],
        json!({"id": "BILL-0000001", "account": "ACCT-0000001", "date": "2026-01-05", "description": "Office visit",
               "owes": "45.00", "pending_insurance": "0.00", "status": "open", "claim": "CLM-0000001",
               "visit_type": "PB", "open": true})
    );
    // A date-time is its day; a null amount is null.
    assert_eq!(v["items"][2]["date"], "2026-01-10");
    assert_eq!(v["items"][1]["pending_insurance"], Value::Null);

    let open = readers::bills_list(&site, None, true, &RangeArgs::default()).unwrap();
    assert_eq!(ids(&open, "id"), ["BILL-0000001"]);
    let ranged = readers::bills_list(&site, None, false, &range(Some("2026-1-10"), Some("2026-01-15"))).unwrap();
    assert_eq!(ids(&ranged, "id"), ["BILL-0000003", "BILL-0000002"]);
    let one = readers::bills_list(&site, Some("ACCT-0000002"), false, &RangeArgs::default()).unwrap();
    assert_eq!(ids(&one, "id"), ["BILL-0000004"]);
    let limited = readers::bills_list(
        &site,
        None,
        false,
        &RangeArgs {
            limit: Some(1),
            ..RangeArgs::default()
        },
    );
    assert_eq!(ids(&limited.unwrap(), "id"), ["BILL-0000004"]);
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
    assert_eq!(v["schema"], "payment-list/v1");
    assert_eq!(
        v["items"],
        json!([
            {"account": "ACCT-0000001", "date": "2026-01-30", "amount": "45.00", "method": "Visa",
             "status": "Scheduled", "settled": false},
            {"account": "ACCT-0000002", "date": "2026-01-20", "amount": "20.00", "method": "cash",
             "status": "Paid", "settled": true},
            {"account": "ACCT-0000001", "date": "2026-01-05", "amount": "5.00", "method": "Visa",
             "status": "Paid", "settled": true},
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
    // STMT-0000002 has no statement id or upload: the app lists it nowhere.
    assert_eq!(
        listed,
        [
            ("statement-ACCT-0000002-STMT-0000003", Some("2026-01-30")),
            ("statement-ACCT-0000001-STMT-0000001", Some("2026-01-25")),
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
    let raw = "2026-01-30_exampleclinic-statement_STMT-0000003.bin";
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
    assert!(batch.skipped[0].reason.contains("--overwrite"), "{:?}", batch.skipped);
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
        Some("200.0")
    );
    let totals = &office["totalPriceComponent"];
    assert_eq!(component(totals, "billed").as_deref(), Some("250.0"));
    assert_eq!(component(totals, "insurance-payment").as_deref(), Some("150.0"));
    assert_eq!(component(totals, "insurance-adjustment").as_deref(), Some("50.0"));
    assert_eq!(component(totals, "patient-payment").as_deref(), Some("5.0"));
    assert_eq!(component(totals, "patient-responsibility").as_deref(), Some("45.0"));
    assert_eq!(office["totalGross"]["value"].to_string(), "250.0");
    assert_eq!(office["totalNet"]["value"].to_string(), "45.0");
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
    assert_eq!(voided["lineItem"][0]["chargeItemCodeableConcept"]["text"], "Services");
    assert_eq!(
        component(&voided["lineItem"][0]["priceComponent"], "billed").as_deref(),
        Some("120.0")
    );
    assert_eq!(voided["note"][0]["text"], "Date of service: 2026-01-15");

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
            ("2026-01-05".into(), "5.0".into()),
            ("2026-01-20".into(), "20.0".into())
        ]
    );
    assert_eq!(payments[0]["disposition"], "Paid by Visa");

    let doc = by_id("statement-ACCT-0000001-STMT-0000001");
    assert_eq!(doc["date"], "2026-01-25T00:00:00Z");
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

    assert_eq!(trailer.counts.get("Invoice"), Some(&4));
    let skipped: Vec<(&str, Option<&str>)> = trailer
        .skipped
        .iter()
        .map(|s| (s.code.as_str(), s.resource_type.as_deref()))
        .collect();
    assert_eq!(
        skipped,
        [
            ("not_settled", Some("PaymentReconciliation")),
            ("verify_failed", Some("DocumentReference")),
        ]
    );
}

#[test]
fn the_export_since_a_day_keeps_the_accounts() {
    let server = serve(fixture_routes(), None);
    let env = env(&server, "export-since");
    let site = Site::open(&env, &adult1()).unwrap();
    let mut out = Vec::new();
    readers::export(&site, &server.base, &mut out, None, None, Some("2026-01-15")).unwrap();
    let (header, resources, _) = read_stream(&out);
    assert_eq!(header.since.as_deref(), Some("2026-01-15"));
    let ids: Vec<&str> = resources.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"account-ACCT-0000001"));
    assert!(ids.contains(&"bill-ACCT-0000001-BILL-0000003"));
    assert!(!ids.contains(&"bill-ACCT-0000001-BILL-0000001"));
    assert_eq!(
        resources
            .iter()
            .filter(|r| r["resourceType"] == "PaymentReconciliation")
            .count(),
        1
    );
    // Bills before the day are not read at all.
    assert!(!operations(&server).contains(&("getSingleBill".into(), json!({"id": "BILL-0000001"}))));
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
    assert_eq!(codes, ["not_settled", "no_file", "verify_failed"]);
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
    assert_eq!(trailer.counts.get("Invoice"), Some(&3));
    assert_eq!(trailer.counts.get("PaymentReconciliation"), Some(&2));
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
