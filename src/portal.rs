//! The commands that read a Flywire portal as a profile: what they read, and
//! the DTOs and stream they make of it. `app` prints what these return; the
//! tests run them in-process against a loopback server with the session in
//! an in-memory vault ([`Env`]).

use std::cell::RefCell;
use std::time::Duration;

use pk_cli_config::profile::ProfileName;
use pk_cli_core::CliError;

use serde_json::json;

use crate::client::{self, Answer, ApiRequest, Client};
use crate::queries;
use crate::reads::{self, BillDetail, PortalAccount, PortalPayment, PortalStatement, StatementFile};
use crate::session::{Session, SessionStore};
use crate::site::Provider;

/// Where sessions are kept and which site is read.
pub struct Env {
    pub sessions: SessionStore,
    /// A test's loopback server; `None` reads each session's own portal.
    pub base: Option<String>,
    /// The least time between one answer and the next request.
    pub pace: Duration,
    pub warnings: Warnings,
}

impl Env {
    /// The binary's: the vault, each session's portal, [`client::PACE`]
    /// apart, warnings on stderr.
    pub fn for_binary() -> Result<Env, CliError> {
        // The base first: a refused override reaches no vault.
        let base = client::base_override()?;
        Ok(Env {
            sessions: SessionStore::open()?,
            base,
            pace: client::PACE,
            warnings: Warnings::stderr(),
        })
    }

    /// Whether this reads the real site, as opposed to a test's server.
    pub fn is_real_site(&self) -> bool {
        self.base.is_none()
    }
}

/// Where a warning goes: stderr for the binary, a list for a test.
pub struct Warnings(Option<RefCell<Vec<String>>>);

impl Warnings {
    pub fn stderr() -> Warnings {
        Warnings(None)
    }

    /// Kept for [`lines`](Self::lines) instead of printed.
    pub fn kept() -> Warnings {
        Warnings(Some(RefCell::default()))
    }

    pub fn warn(&self, line: String) {
        match &self.0 {
            None => eprintln!("warning: {line}"),
            Some(kept) => kept.borrow_mut().push(line),
        }
    }

    /// The warnings kept so far.
    pub fn lines(&self) -> Vec<String> {
        self.0.as_ref().map(|k| k.borrow().clone()).unwrap_or_default()
    }
}

/// A profile's signed-in reads.
pub struct Site<'a> {
    client: Client,
    profile: ProfileName,
    session: Session,
    store: &'a SessionStore,
    warnings: &'a Warnings,
}

impl<'a> Site<'a> {
    /// The profile's stored session (none is exit 3, naming the sign-in).
    pub fn open(env: &'a Env, profile: &ProfileName) -> Result<Site<'a>, CliError> {
        let session = env.sessions.load(profile)?;
        let origin = client::origin(env.base.as_deref(), &session.provider);
        let client = Client::new(&session, profile, &origin, env.pace)?;
        Ok(Site {
            client,
            profile: profile.clone(),
            session,
            store: &env.sessions,
            warnings: &env.warnings,
        })
    }

    /// `flywire api`, the request already checked by
    /// [`client::api_request`].
    pub fn api(&self, request: &ApiRequest) -> Result<Answer, CliError> {
        self.client.api(request)
    }

    /// The profile whose session this is.
    pub fn profile(&self) -> &ProfileName {
        &self.profile
    }

    /// The provider whose portal this session signs in to.
    pub fn provider(&self) -> &Provider {
        &self.session.provider
    }

    pub fn warn(&self, line: String) {
        self.warnings.warn(line);
    }

    /// The accounts, open and closed bills included (`payables`).
    pub fn accounts(&self) -> Result<Vec<PortalAccount>, CliError> {
        let data = self.query("payables", queries::PAYABLES, json!({ "filterClosedBills": false }))?;
        reads::payables(&data)
    }

    /// The accounts' guarantor ids (`payableGuarantors`).
    pub fn guarantors(&self) -> Result<Vec<String>, CliError> {
        let data = self.query(
            "payableGuarantors",
            queries::GUARANTORS,
            json!({ "filterClosedBills": false }),
        )?;
        Ok(reads::guarantors(&data))
    }

    /// One bill's page (`getSingleBill`).
    pub fn bill_detail(&self, bill_id: &str) -> Result<BillDetail, CliError> {
        let data = self.query("getSingleBill", queries::SINGLE_BILL, json!({ "id": bill_id }))?;
        reads::bill_detail(&data)
    }

    /// The payments on one account (`previousPayments`).
    pub fn payments(&self, account: &PortalAccount) -> Result<Vec<PortalPayment>, CliError> {
        let data = self.query(
            "previousPayments",
            queries::PREVIOUS_PAYMENTS,
            json!({ "payableIds": [account.id] }),
        )?;
        reads::payments(&data, &account.number)
    }

    /// One account's past statements (`pastStatementsInfo`).
    pub fn statements(&self, account: &PortalAccount) -> Result<Vec<PortalStatement>, CliError> {
        let data = self.query(
            "pastStatementsInfo",
            queries::PAST_STATEMENTS,
            json!({ "payableIds": [account.id], "accountNumber": account.number }),
        )?;
        reads::statements(&data, &account.number)
    }

    /// One statement's file (`pastStatementsContent`).
    pub fn statement_file(&self, statement_id: &str) -> Result<StatementFile, CliError> {
        let data = self.query(
            "pastStatementsContent",
            queries::STATEMENT_CONTENT,
            json!({ "statementId": statement_id }),
        )?;
        reads::statement_file(&data)
    }

    /// One query's `data` ([`Client::query`]).
    pub fn query(
        &self,
        operation: &str,
        document: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, CliError> {
        self.client.query(operation, document, variables)
    }

    /// Write the cookies the answers changed back to the profile's session,
    /// only while the stored session is still the one this command read: a
    /// sign-in made meanwhile is never overwritten. Whether it wrote.
    pub fn finish(&self) -> Result<bool, CliError> {
        let Some(header) = self.client.updated_cookie_header() else {
            return Ok(false);
        };
        match self.store.get(&self.profile)? {
            Some(stored) if stored.cookie_header.expose() == self.session.cookie_header.expose() => {
                self.store
                    .put(&self.profile, &self.session.with_cookie_header(header))?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// [`Site::finish`] after a command's output is complete: a failed
    /// write-back is a warning, since the output stands and the stored
    /// session still works.
    pub fn finish_quietly(&self) {
        if let Err(e) = self.finish() {
            eprintln!(
                "warning: the session's refreshed cookies were not stored: {}",
                e.message()
            );
        }
    }
}

pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    pk_cli_core::dates::fmt_rfc3339(secs)
}
