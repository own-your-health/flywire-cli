//! The commands that read a Flywire portal as a profile: what they read, and
//! the DTOs and stream they make of it. `app` prints what these return; the
//! tests run them in-process against a loopback server with the session in
//! an in-memory vault ([`Env`]).

use std::time::Duration;

use pk_cli_config::profile::ProfileName;
use pk_cli_core::CliError;

use crate::client::{self, Answer, ApiRequest, Client};
use crate::session::{Session, SessionStore};

/// Where sessions are kept and which site is read.
pub struct Env {
    pub sessions: SessionStore,
    /// A test's loopback server; `None` reads each session's own portal.
    pub base: Option<String>,
    /// The least time between one answer and the next request.
    pub pace: Duration,
}

impl Env {
    /// The binary's: the vault, each session's portal, [`client::PACE`] apart.
    pub fn for_binary() -> Result<Env, CliError> {
        // The base first: a refused override reaches no vault.
        let base = client::base_override()?;
        Ok(Env {
            sessions: SessionStore::open()?,
            base,
            pace: client::PACE,
        })
    }

    /// Whether this reads the real site, as opposed to a test's server.
    pub fn is_real_site(&self) -> bool {
        self.base.is_none()
    }
}

/// A profile's signed-in reads.
pub struct Site<'a> {
    client: Client,
    profile: ProfileName,
    session: Session,
    store: &'a SessionStore,
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
        })
    }

    /// `flywire api`, the request already checked by
    /// [`client::api_request`].
    pub fn api(&self, request: &ApiRequest) -> Result<Answer, CliError> {
        self.client.api(request)
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
