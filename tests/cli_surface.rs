//! Black-box tests of the binary's surface. None reaches a portal:
//! each renders help or info, is refused by argument checks that run before
//! a session is read, or finds no session stored. The config directory is a
//! temp dir, never the developer's, and `FLYWIRE_PROFILE` is cleared.
//!
//! None reaches the real keychain or browser profile:
//! `FLYWIRE_KEYCHAIN_SERVICE` names a service that holds nothing (a missing
//! item needs no approval), and under it the binary never touches the kept
//! profile. `XDG_DATA_HOME`, where Linux keeps that profile, is a temp dir
//! besides. No test stores a session, so no test writes the vault's key or
//! reads one: the vault reads its key only when an entry exists (cli-common
//! DESIGN.md §2.5).
//!
//! The temp config of [`flywire`] configures one profile, `adult1`.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn tmp() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "flywire-cli-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The empty keychain service every test reads instead of `piekstra.flywire`.
const HERMETIC_SERVICE: &str = "piekstra.flywire.hermetic-tests";

/// A config directory whose config configures `profiles`.
fn config_with(profiles: &[&str]) -> std::path::PathBuf {
    let base = tmp();
    std::fs::create_dir_all(base.join("flywire")).unwrap();
    let names: serde_json::Map<String, Value> = profiles
        .iter()
        .map(|p| (p.to_string(), serde_json::json!({})))
        .collect();
    std::fs::write(
        base.join("flywire").join("config.json"),
        serde_json::json!({ "profiles": names }).to_string(),
    )
    .unwrap();
    base
}

fn bare() -> Command {
    let mut cmd = Command::cargo_bin("flywire").unwrap();
    cmd.env("FLYWIRE_KEYCHAIN_SERVICE", HERMETIC_SERVICE)
        .env("XDG_DATA_HOME", tmp())
        .env_remove("FLYWIRE_CHROME")
        .env_remove("FLYWIRE_PROFILE")
        .env_remove("FLYWIRE_BASE_URL")
        .env_remove("NO_COLOR");
    cmd
}

/// A loopback origin no test listens on. Under the test keychain service a
/// command that would send a request needs `FLYWIRE_BASE_URL`; with this
/// one, a test that finds no session still reaches its own refusal, and a
/// request that went out anyway would be refused by this machine.
const NO_SERVER: &str = "http://127.0.0.1:9";

fn flywire() -> Command {
    let mut cmd = bare();
    cmd.env("XDG_CONFIG_HOME", config_with(&["adult1"]))
        .env("FLYWIRE_BASE_URL", NO_SERVER);
    cmd
}

fn json_of(out: &assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&out.get_output().stdout).expect("one JSON document on stdout")
}

#[test]
fn every_command_renders_help() {
    for args in [
        vec!["--help"],
        vec!["auth", "--help"],
        vec!["auth", "login", "--help"],
        vec!["auth", "status", "--help"],
        vec!["auth", "logout", "--help"],
        vec!["auth", "reset", "--help"],
        vec!["profiles", "--help"],
        vec!["profiles", "list", "--help"],
        vec!["profiles", "add", "--help"],
        vec!["profiles", "remove", "--help"],
        vec!["config", "--help"],
        vec!["config", "path", "--help"],
        vec!["config", "show", "--help"],
        vec!["patients", "--help"],
        vec!["patients", "list", "--help"],
        vec!["accounts", "--help"],
        vec!["accounts", "list", "--help"],
        vec!["bills", "--help"],
        vec!["bills", "list", "--help"],
        vec!["bills", "show", "--help"],
        vec!["payments", "--help"],
        vec!["payments", "list", "--help"],
        vec!["records", "--help"],
        vec!["records", "export", "--help"],
        vec!["documents", "--help"],
        vec!["documents", "list", "--help"],
        vec!["documents", "download", "--help"],
        vec!["api", "--help"],
        vec!["self-update", "--help"],
        vec!["completions", "--help"],
        vec!["info", "--help"],
    ] {
        flywire().args(&args).assert().success();
    }
}

#[test]
fn help_says_read_only_and_unofficial() {
    flywire()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Unofficial read-only"));
}

#[test]
fn info_declares_the_profile_and_login_profiles() {
    let out = bare().env("XDG_CONFIG_HOME", tmp()).arg("info").assert().success();
    let v = json_of(&out);
    assert_eq!(v["schema"], "cli-info/v1");
    assert_eq!(v["name"], "flywire");
    let caps = v["capabilities"].as_array().unwrap();
    assert!(caps.iter().any(|c| c == "login-profiles"), "{caps:?}");
    assert!(!caps.iter().any(|c| c == "records-files"), "no files source: {caps:?}");
    for c in ["patients", "accounts", "bills", "payments", "records", "documents"] {
        assert!(caps.iter().any(|x| x == c), "{c}: {caps:?}");
    }
    assert_eq!(v["profiles"], serde_json::json!(["health-records/v1", "documents/v1"]));
    assert_eq!(v["auth"]["required"], true);
}

#[test]
fn completions_and_version_run() {
    flywire().args(["completions", "zsh"]).assert().success();
    flywire()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn no_color_is_accepted() {
    flywire().env("NO_COLOR", "1").arg("info").assert().success();
}

/// A command that signs in needs a configured profile: none given is exit 2
/// listing the configured ones, an unknown one too, and a lone profile is
/// never picked.
#[test]
fn a_login_command_needs_its_profile() {
    for args in [
        vec!["auth", "status"],
        vec!["auth", "logout"],
        vec!["api", "GET", "/app/bills"],
        vec!["patients", "list"],
        vec!["accounts", "list"],
        vec!["bills", "list"],
        vec!["bills", "show", "BILL-0000001"],
        vec!["payments", "list"],
        vec!["documents", "list"],
        vec!["documents", "download", "STMT-0000001"],
        vec!["records", "export", "-o", "unused"],
    ] {
        let out = flywire().args(&args).arg("--json").assert().code(2);
        let v = json_of(&out);
        assert_eq!(v["error"]["code"], "usage", "{args:?}");
        let message = v["error"]["message"].as_str().unwrap();
        assert!(message.contains("adult1"), "{args:?}: {message}");
    }
    let out = flywire()
        .args(["api", "GET", "/app/bills", "--profile", "nobody", "--json"])
        .assert()
        .code(2);
    assert!(json_of(&out)["error"]["message"].as_str().unwrap().contains("adult1"));
    // With none configured, it names `profiles add`.
    let out = bare()
        .env("XDG_CONFIG_HOME", tmp())
        .args(["api", "GET", "/app/bills", "--json"])
        .assert()
        .code(2);
    assert!(json_of(&out)["error"].to_string().contains("profiles add"));
}

/// The variable stands in for the flag.
#[test]
fn the_profile_variable_is_read() {
    flywire()
        .env("FLYWIRE_PROFILE", "adult1")
        .args(["auth", "status", "--json"])
        .assert()
        .success();
}

#[test]
fn a_command_without_a_login_takes_no_profile() {
    for args in [
        vec!["info"],
        vec!["profiles", "list"],
        vec!["config", "path"],
        vec!["auth", "reset", "--force"],
        vec!["completions", "bash"],
    ] {
        flywire().args(&args).args(["--profile", "adult1"]).assert().code(2);
    }
    // The variable is not read by them.
    flywire()
        .env("FLYWIRE_PROFILE", "adult1")
        .arg("info")
        .assert()
        .success();
}

/// A signed-out profile: status says so, and a read is exit 3 naming the
/// sign-in. Nothing is stored, so nothing reads a key.
#[test]
fn a_profile_without_a_session() {
    let out = flywire()
        .args(["auth", "status", "--profile", "adult1", "--json"])
        .assert()
        .success();
    let v = json_of(&out);
    assert_eq!(v["schema"], "auth-status/v1");
    assert_eq!(v["authenticated"], false);
    assert_eq!(v["method"], "browser-session");
    let out = flywire()
        .args(["api", "GET", "/app/bills", "--profile", "adult1", "--json"])
        .assert()
        .code(3);
    let v = json_of(&out);
    assert_eq!(v["error"]["code"], "auth");
    assert!(
        v["error"]
            .to_string()
            .contains("auth login --browser --provider <PROVIDER> --profile adult1"),
        "{v}"
    );
    for args in [
        vec!["accounts", "list"],
        vec!["bills", "list", "--open"],
        vec!["documents", "list"],
    ] {
        let out = flywire()
            .args(&args)
            .args(["--profile", "adult1"])
            .arg("--json")
            .assert()
            .code(3);
        assert_eq!(json_of(&out)["error"]["code"], "auth", "{args:?}");
    }
    // The stream's own output: nothing on stdout, the error on stderr.
    flywire()
        .args(["records", "export", "--ndjson", "--profile", "adult1"])
        .assert()
        .code(3)
        .stdout("");
    let out = flywire()
        .args(["auth", "logout", "--profile", "adult1", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&out)["session_removed"], false);
}

/// The readers' own argument checks run before the vault or the site.
#[test]
fn the_readers_check_their_arguments_first() {
    for args in [
        vec!["records", "export"],
        vec!["records", "export", "--ndjson", "--json"],
        vec!["records", "export", "--ndjson", "-o", "x"],
        vec!["records", "export", "--ndjson", "--since", "2026-13-01"],
        vec!["bills", "list", "--since", "January"],
        vec!["bills", "list", "--since", "2026-01-20", "--until", "2026-01-10"],
        vec!["bills", "show", " "],
        vec!["payments", "list", "--until", "2026-02-32"],
        vec!["documents", "download"],
        vec!["documents", "download", "--all"],
        vec!["documents", "download", "STMT-0000001", "--all", "-o", "x"],
        vec!["documents", "download", "--all", "-o", "-"],
    ] {
        flywire().args(&args).args(["--profile", "adult1"]).assert().code(2);
    }
    // A plain file of records is for a test's server only: under the test
    // keychain service a base is always set, so that refusal is a unit test
    // in src/app.rs (`plain_file_refused`).
}

/// `api` reads only, refused before the session is read.
#[test]
fn the_api_is_read_only() {
    for args in [
        vec!["api", "POST", "/app/bills"],
        vec!["api", "POST", "/graphql"],
        vec![
            "api",
            "POST",
            "/graphql",
            "--data",
            r#"{"query":"mutation logout { logout }"}"#,
        ],
        vec![
            "api",
            "POST",
            "/graphql",
            "--data",
            r#"{"query":"query verifyCode { verifyCode(code: \"1\") }"}"#,
        ],
        vec!["api", "DELETE", "/app/bills"],
        vec!["api", "GET", "/graphql"],
        vec!["api", "GET", "/app/login"],
        vec!["api", "GET", "/app/bills/logout"],
        vec!["api", "GET", "/app/pay"],
        vec!["api", "GET", "https://example.com/app/bills"],
    ] {
        flywire().args(&args).args(["--profile", "adult1"]).assert().code(2);
    }
}

#[test]
fn profiles_are_added_listed_and_removed() {
    let config = tmp();
    let run = |args: &[&str]| {
        let mut cmd = bare();
        cmd.env("XDG_CONFIG_HOME", &config).args(args);
        cmd
    };
    let out = run(&["profiles", "list", "--json"]).assert().success();
    assert_eq!(
        json_of(&out),
        serde_json::json!({ "schema": "profile-list/v1", "profiles": [] })
    );
    let out = run(&["profiles", "add", "adult2", "--json"]).assert().success();
    assert_eq!(json_of(&out)["added"], true);
    let out = run(&["profiles", "add", "adult2", "--json"]).assert().success();
    assert_eq!(json_of(&out)["added"], false);
    run(&["profiles", "add", "Bad Name"]).assert().code(2);
    let out = run(&["profiles", "list", "--json"]).assert().success();
    assert_eq!(json_of(&out)["profiles"][0]["name"], "adult2");
    // Removing asks; without a terminal it needs --force (exit 6).
    run(&["profiles", "remove", "adult2"]).assert().code(6);
    run(&["profiles", "remove", "nobody", "--force"]).assert().code(4);
    let out = run(&["profiles", "remove", "adult2", "--force", "--json"])
        .assert()
        .success();
    let v = json_of(&out);
    assert_eq!(v["schema"], "profile-remove/v1");
    assert_eq!(v["session_removed"], false);
    assert!(
        v.get("browser_profile_removed").is_none(),
        "left alone under the test service"
    );
    let out = run(&["profiles", "list", "--json"]).assert().success();
    assert!(json_of(&out)["profiles"].as_array().unwrap().is_empty());
}

#[test]
fn auth_login_checks_its_flags_before_anything() {
    for args in [
        vec!["auth", "login"],
        vec!["auth", "login", "--browser"],
        vec!["auth", "login", "--provider", "exampleclinic"],
        vec!["auth", "login", "--provider", "exampleclinic", "--browser", "--stdin"],
        vec!["auth", "login", "--provider", "exampleclinic", "--stdin"],
        vec![
            "auth",
            "login",
            "--provider",
            "exampleclinic",
            "--browser",
            "--user-agent",
            "x",
        ],
        vec!["auth", "login", "--provider", "not a portal", "--browser"],
        vec!["auth", "login", "--provider", "exampleclinic.example.com", "--browser"],
    ] {
        flywire().args(&args).args(["--profile", "adult1"]).assert().code(2);
    }
}

#[test]
fn auth_reset_asks_first() {
    flywire().args(["auth", "reset"]).assert().code(6);
}

#[test]
fn config_shows_the_profiles() {
    let out = flywire().args(["config", "show", "--json"]).assert().success();
    let v = json_of(&out);
    assert_eq!(v["schema"], "config/v1");
    assert!(v["profiles"].get("adult1").is_some(), "{v}");
    flywire()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::ends_with("config.json\n"));
}

/// `FLYWIRE_BASE_URL` points reads at a test's loopback server only, and
/// only with the test keychain service, refused before any vault is opened.
#[test]
fn the_base_url_override_is_for_loopback_tests_only() {
    for url in [
        "https://exampleclinic.simpleepay.com.example.com",
        "http://example.com",
        "http://127.0.0.1:1@example.com",
    ] {
        flywire()
            .env("FLYWIRE_BASE_URL", url)
            .args(["api", "GET", "/app/bills", "--profile", "adult1"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("FLYWIRE_BASE_URL"));
    }
    flywire()
        .env_remove("FLYWIRE_KEYCHAIN_SERVICE")
        .env("FLYWIRE_BASE_URL", "http://127.0.0.1:9")
        .args(["api", "GET", "/app/bills", "--profile", "adult1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("FLYWIRE_KEYCHAIN_SERVICE"));
    // The real service name is not a test's, wherever the vault is.
    flywire()
        .env("FLYWIRE_KEYCHAIN_SERVICE", "piekstra.flywire")
        .env("FLYWIRE_BASE_URL", "http://127.0.0.1:9")
        .args(["api", "GET", "/app/bills", "--profile", "adult1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("piekstra.flywire.<suffix>"));
    flywire()
        .env("FLYWIRE_BASE_URL", "http://127.0.0.1:9")
        .args([
            "auth",
            "login",
            "--browser",
            "--provider",
            "exampleclinic",
            "--profile",
            "adult1",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("real portal"));
    // A loopback origin under the test service passes; the signed-out
    // profile then stops it at the session, before a request.
    flywire()
        .env("FLYWIRE_BASE_URL", "http://127.0.0.1:9")
        .args(["api", "GET", "/app/bills", "--profile", "adult1"])
        .assert()
        .code(3);
}

/// Under the test keychain service a command that would send a request
/// needs `FLYWIRE_BASE_URL`: without it, it is refused before any
/// request, so a test that forgets its server never reaches the real site.
/// `FLYWIRE_CHROME` names no browser, so `auth login --browser` could not
/// open one even if the refusal came too late.
#[test]
fn a_test_run_without_a_base_url_sends_nothing() {
    let cases: &[&[&str]] = &[
        &["api", "GET", "/app/bills", "--profile", "adult1"],
        &["accounts", "list", "--profile", "adult1"],
        &[
            "auth",
            "login",
            "--stdin",
            "--user-agent",
            "Mozilla/5.0",
            "--profile",
            "adult1",
            "--provider",
            "exampleclinic",
        ],
        &[
            "auth",
            "login",
            "--browser",
            "--profile",
            "adult1",
            "--provider",
            "exampleclinic",
        ],
    ];
    for &args in cases {
        flywire()
            .env_remove("FLYWIRE_BASE_URL")
            .env("FLYWIRE_CHROME", "/nonexistent/chrome")
            .args(args)
            .write_stdin("a=b")
            .assert()
            .code(2)
            .stderr(predicate::str::contains("FLYWIRE_BASE_URL"))
            .stderr(predicate::str::contains("FLYWIRE_KEYCHAIN_SERVICE is set"));
    }
}
