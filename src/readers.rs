//! The reader commands: what `patients`, `accounts`, `bills`, `payments`
//! and `documents` print, and the `records export` stream. Each reads
//! through a [`Site`] and returns its document, so `app` prints it and the
//! tests check it in-process against a loopback server.
//!
//! One sign-in is one patient at one provider: the portal's guest sign-in
//! takes the patient's account number and date of birth. The organization's
//! id is the provider. The patient's is the provider with a hash of the
//! profile and the portal's id for the accounts' guarantor, so it holds
//! while accounts come and go and across sign-ins to the profile, and a
//! profile reused for another person's accounts gets another id rather than
//! merging two people's records. The guarantor's id itself never shows.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use pk_cli_core::private_file::{self, Replace};
use pk_cli_core::{CliError, RangeArgs, Reason};
use pk_cli_documents::{Document, DownloadBatch, SavedDocument, SkippedDocument};
use pk_med_records::dto::{kind, method, relationship};
use pk_med_records::stream::skip;
use pk_med_records::{Finished, Header, Organization, PatientContext, RawResource, RecordsWriter, Skip, Source};
use serde_json::{json, Value};

use crate::bill::{self, id_of, Cents, Mapper};
use crate::cli::{DownloadArgs, BIN};
use crate::portal::{now_rfc3339, Site};
use crate::reads::{self, PortalAccount, PortalBill, PortalStatement};

/// `meta.source` on every resource flywire maps.
pub const META_SOURCE: &str = "flywire:site";
pub const TAG_SYSTEM: &str = "https://github.com/piekstra/flywire-cli/fhir/tag";
pub const TAG_MAPPED: &str = "mapped-from-patient-portal";
const TAG_DISPLAY: &str = "Mapped by flywire from a Flywire patient portal";

/// What one sign-in shows, read once: the accounts and who the resources
/// are about.
pub struct Book {
    pub accounts: Vec<PortalAccount>,
    /// The resources' ids; `patient_id` is set only by [`patient_book`].
    pub mapper: Mapper,
}

/// The accounts, and the ids the resources carry but the patient's.
pub fn book(site: &Site) -> Result<Book, CliError> {
    let accounts = site.accounts()?;
    let provider = site.provider().as_str().to_string();
    let organization_name = accounts
        .iter()
        .find_map(|a| a.payee.clone())
        .unwrap_or_else(|| provider.clone());
    Ok(Book {
        accounts,
        mapper: Mapper {
            cli: BIN.into(),
            meta_source: META_SOURCE.into(),
            tag_system: TAG_SYSTEM.into(),
            tag_code: TAG_MAPPED.into(),
            tag_display: TAG_DISPLAY.into(),
            patient_id: String::new(),
            organization_id: id_of("org", &provider),
            organization_name,
        },
    })
}

/// FNV-1a, 64-bit: a stable hash with no dependency, for ids only.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// [`book`] with the patient's id, for `patients list` and `records
/// export`. A sign-in with no accounts is exit 4. Accounts naming no
/// guarantor, or a portal that refuses the guarantor query, are exit 5,
/// since no id would stay the same from one run to the next; any other
/// failure of that query is the client's own error.
pub fn patient_book(site: &Site) -> Result<Book, CliError> {
    let mut book = book(site)?;
    if book.accounts.is_empty() {
        return Err(CliError::NotFound("the sign-in shows no accounts".into())
            .with_remedy(format!("`{BIN} accounts list --profile <NAME>` shows what it reads")));
    }
    let no_guarantor = || {
        CliError::Upstream(
            "the portal named no guarantor on the sign-in's accounts, so the patient has no stable id".into(),
        )
        .with_remedy(format!(
            "the portal may have changed; `{BIN} api POST /graphql` shows its answer"
        ))
    };
    // A refusal of the query (GraphQL errors, an HTTP 400) is the field
    // refused; a 429 or 5xx keeps its own message, which says to retry.
    let guarantors = match site.guarantors() {
        Ok(g) => g,
        Err(e) if e.reason() == Some(Reason::BadRequest) => return Err(no_guarantor()),
        Err(e) => return Err(e),
    };
    // The smallest, should the accounts name more than one guarantor.
    let Some(guarantor) = guarantors.iter().min() else {
        return Err(no_guarantor());
    };
    let provider = site.provider().as_str();
    let key = format!("{provider}\n{}\n{guarantor}", site.profile());
    book.mapper.patient_id = id_of("patient", &format!("{provider}-{:016x}", fnv1a(key.as_bytes())));
    Ok(book)
}

impl Book {
    /// The accounts `--account` names (its portal id or its number), or
    /// every one. An unknown one is exit 4.
    pub fn select(&self, account: Option<&str>) -> Result<Vec<&PortalAccount>, CliError> {
        let Some(want) = account.map(str::trim).filter(|a| !a.is_empty()) else {
            return Ok(self.accounts.iter().collect());
        };
        let found: Vec<&PortalAccount> = self
            .accounts
            .iter()
            .filter(|a| a.id == want || a.number == want)
            .collect();
        if found.is_empty() {
            return Err(CliError::NotFound(format!("no account `{want}` at this sign-in"))
                .with_remedy(format!("`{BIN} accounts list --profile <NAME>` lists them")));
        }
        Ok(found)
    }

    fn provider(&self) -> bill::Provider {
        bill::Provider {
            id: self.mapper.organization_id.clone(),
            name: self.mapper.organization_name.clone(),
        }
    }

    /// The patient a sign-in is. `name` is never in the stream.
    pub fn context(&self) -> PatientContext {
        PatientContext {
            id: self.mapper.patient_id.clone(),
            organization: self.mapper.organization_id.clone(),
            label: None,
            relationship: Some(relationship::SELF.into()),
            kind: Some(kind::PATIENT.into()),
            member_role: None,
            fhir_user: None,
            name: None,
            session: None,
        }
    }
}

fn money(c: Option<Cents>) -> Value {
    c.map_or(Value::Null, |c| c.decimal().into())
}

/// A day given as `YYYY-MM-DD` (or unpadded, `2026-1-5`) as `YYYY-MM-DD`,
/// so it compares with the portal's days as text.
fn iso_day(s: &str) -> Result<String, CliError> {
    let (y, m, d) = pk_cli_core::dates::parse_iso(s.trim())?;
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

/// The range with its days padded (`RangeArgs::validate` has checked them).
fn padded(range: &RangeArgs) -> Result<RangeArgs, CliError> {
    Ok(RangeArgs {
        since: range.since.as_deref().map(iso_day).transpose()?,
        until: range.until.as_deref().map(iso_day).transpose()?,
        ..range.clone()
    })
}

/// Whether `date` is in the range: an undated row is in it only when the
/// range is open.
fn in_range(date: Option<&str>, range: &RangeArgs) -> bool {
    match date {
        Some(d) => range.since.as_deref().is_none_or(|s| d >= s) && range.until.as_deref().is_none_or(|u| d <= u),
        None => range.since.is_none() && range.until.is_none(),
    }
}

fn limited(mut items: Vec<Value>, range: &RangeArgs) -> Vec<Value> {
    if let Some(n) = range.limit {
        items.truncate(n as usize);
    }
    items
}

fn list(record: &str, items: Vec<Value>) -> Value {
    json!({ "schema": format!("{record}-list/v1"), "items": items })
}

// ------------------------------------------------------------- patients

/// `patients list` (patient-list/v1): the one patient a sign-in is.
pub fn patients(site: &Site) -> Result<Value, CliError> {
    let book = patient_book(site)?;
    serde_json::to_value(pk_med_records::patient_list(vec![book.context()]))
        .map_err(|e| CliError::Other(format!("serializing: {e}")))
}

// ------------------------------------------------------------- accounts

/// `accounts list` (account-list/v1).
pub fn accounts_list(site: &Site) -> Result<Value, CliError> {
    let book = book(site)?;
    let items = book
        .accounts
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "number": a.number,
                "provider": book.mapper.organization_name,
                "owes": money(a.standing),
                "pending_insurance": money(reads::account(a, &book.provider()).insurance_pending),
                "bills": a.bills.len(),
                "open_bills": a.bills.iter().filter(|b| b.is_open()).count(),
            })
        })
        .collect();
    Ok(list("account", items))
}

pub const ACCOUNT_COLUMNS: &[&str] = &["id", "number", "provider", "owes", "bills", "open_bills"];

// ---------------------------------------------------------------- bills

fn bill_row(a: &PortalAccount, b: &PortalBill) -> Value {
    json!({
        "id": b.id,
        "account": a.number,
        "date": b.date,
        "description": b.name,
        "owes": money(b.amount_due),
        "pending_insurance": money(b.pending_insurance),
        "status": b.status,
        "claim": b.claim,
        "visit_type": b.visit_type,
        "open": b.is_open(),
    })
}

/// `bills list` (bill-list/v1): newest service first.
pub fn bills_list(site: &Site, account: Option<&str>, open: bool, range: &RangeArgs) -> Result<Value, CliError> {
    let range = &padded(range)?;
    let book = book(site)?;
    let mut rows: Vec<(&PortalAccount, &PortalBill)> = book
        .select(account)?
        .into_iter()
        .flat_map(|a| a.bills.iter().map(move |b| (a, b)))
        .filter(|(_, b)| !open || b.is_open())
        .filter(|(_, b)| in_range(b.date.as_deref(), range))
        .collect();
    rows.sort_by(|x, y| y.1.date.cmp(&x.1.date).then_with(|| x.1.id.cmp(&y.1.id)));
    let items = rows.into_iter().map(|(a, b)| bill_row(a, b)).collect();
    Ok(list("bill", limited(items, range)))
}

pub const BILL_COLUMNS: &[&str] = &["id", "date", "account", "description", "owes", "status"];

/// `bills show <ID>` (bill/v1): the bill's page, its totals and charges.
pub fn bill_show(site: &Site, id: &str, account: Option<&str>) -> Result<Value, CliError> {
    let book = book(site)?;
    let id = id.trim();
    let Some((a, b)) = book
        .select(account)?
        .into_iter()
        .find_map(|a| a.bills.iter().find(|b| b.id == id).map(|b| (a, b)))
    else {
        return Err(CliError::NotFound(format!("no bill `{id}` at this sign-in"))
            .with_remedy(format!("`{BIN} bills list --profile <NAME>` lists them")));
    };
    let d = site.bill_detail(&b.id)?;
    let mut v = json!({ "schema": "bill/v1" });
    if let (Some(doc), Value::Object(row)) = (v.as_object_mut(), bill_row(a, b)) {
        doc.extend(row);
    }
    v["visit_number"] = json!(d.visit_number);
    v["totals"] = json!({
        "billed": money(d.billed),
        "insurance_paid": money(d.insurance_paid),
        "insurance_adjustments": money(d.insurance_adjustments),
        "patient_paid": money(d.patient_payments),
        "patient_adjustments": money(d.patient_adjustments),
        "pending_insurance": money(d.pending_insurance),
        "insurance_responsibility": money(d.insurance_responsibility),
        "owes": money(b.amount_due),
    });
    v["plans"] = d
        .coverages
        .iter()
        .map(|c| json!({ "priority": c.priority, "insurance": c.insurance_name, "payer": c.payer, "plan": c.plan }))
        .collect::<Vec<_>>()
        .into();
    v["charges"] = d
        .charges
        .iter()
        .map(|c| {
            let (system, code, display) = match (&c.cpt, &c.revenue) {
                (Some((code, d)), _) => (Some("cpt"), Some(code.clone()), d.clone()),
                (None, Some((code, d))) => (Some("revenue"), Some(code.clone()), d.clone()),
                (None, None) => (None, None, None),
            };
            json!({
                "date": c.date,
                "description": c.description,
                "code": code,
                "code_system": system,
                "code_display": display,
                "amount": money(c.amount),
            })
        })
        .collect::<Vec<_>>()
        .into();
    Ok(v)
}

pub const CHARGE_COLUMNS: &[&str] = &["date", "code", "description", "amount"];

// ------------------------------------------------------------- payments

/// `payments list` (payment-list/v1): newest first, settled or not.
pub fn payments_list(site: &Site, account: Option<&str>, range: &RangeArgs) -> Result<Value, CliError> {
    let range = &padded(range)?;
    let book = book(site)?;
    let mut rows = Vec::new();
    for a in book.select(account)? {
        for p in site.payments(a)? {
            if in_range(p.date.as_deref(), range) {
                rows.push(p);
            }
        }
    }
    rows.sort_by(|x, y| y.date.cmp(&x.date));
    let items = rows
        .iter()
        .map(|p| {
            json!({
                "account": p.account,
                "date": p.date,
                "amount": money(p.amount),
                "method": p.method,
                "status": p.status,
                "settled": p.is_settled(),
            })
        })
        .collect();
    Ok(list("payment", limited(items, range)))
}

pub const PAYMENT_COLUMNS: &[&str] = &["date", "account", "amount", "method", "status"];

// ------------------------------------------------------------ documents

/// A statement as a listed document: its id is the `DocumentReference.id`
/// `records export` writes for it.
fn document_of(book: &Book, s: &PortalStatement) -> Document {
    let mut d = Document::new(
        book.mapper.statement_id(&s.account, &s.id),
        match &s.date {
            Some(date) => format!("Statement {date}"),
            None => "Statement".to_string(),
        },
    );
    d.date = s.date.clone();
    d.category = Some("statement".into());
    d
}

/// The statements a sign-in can view, newest first, with their documents.
pub fn documents(
    site: &Site,
    account: Option<&str>,
    range: &RangeArgs,
) -> Result<(Book, Vec<(Document, PortalStatement)>), CliError> {
    let range = &padded(range)?;
    let book = book(site)?;
    let mut found = Vec::new();
    for a in book.select(account)? {
        for s in site.statements(a)? {
            if s.is_viewable() && in_range(s.date.as_deref(), range) {
                found.push(s);
            }
        }
    }
    found.sort_by(|x, y| y.date.cmp(&x.date).then_with(|| x.id.cmp(&y.id)));
    if let Some(n) = range.limit {
        found.truncate(n as usize);
    }
    let docs = found.into_iter().map(|s| (document_of(&book, &s), s)).collect();
    Ok((book, docs))
}

/// The statement a `documents download` id names: a `documents list` id,
/// or the portal's own statement id.
pub fn find_document(docs: &[(Document, PortalStatement)], id: &str) -> Result<(Document, PortalStatement), CliError> {
    let id = id.trim();
    docs.iter()
        .find(|(d, s)| d.id == id || s.id == id)
        .cloned()
        .ok_or_else(|| {
            CliError::NotFound(format!("no statement `{id}` at this sign-in"))
                .with_remedy(format!("`{BIN} documents list --profile <NAME>` lists them"))
        })
}

/// The file name a statement is saved under: its date, the provider, and
/// the portal's id, each made path-safe.
pub fn file_name(book: &Book, s: &PortalStatement, ext: &str) -> String {
    use pk_cli_documents::fs_safe;
    format!(
        "{}_{}-statement_{}.{}",
        fs_safe(s.date.as_deref().unwrap_or("undated")),
        fs_safe(book.mapper.organization_id.trim_start_matches("org-")),
        fs_safe(&s.id),
        fs_safe(ext)
    )
}

/// What `documents download` did.
#[derive(Debug)]
pub enum Downloaded {
    /// `-o -`: the file, for stdout.
    Stdout(Vec<u8>),
    /// One statement saved; `as_sent` when its format was not recognized.
    One { saved: SavedDocument, as_sent: bool },
    /// `--all`, with the names saved as sent.
    Batch { batch: DownloadBatch, as_sent: Vec<String> },
}

/// Write `bytes` to `path`, owner-only: an existing file only with
/// `overwrite`, replaced by rename so a failed write leaves the old file.
fn write_private(path: &Path, bytes: &[u8], overwrite: bool) -> Result<(), CliError> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(CliError::Usage(format!("{} is a directory", path.display())));
    }
    let replace = if overwrite { Replace::Always } else { Replace::Never };
    private_file::write(path, bytes, replace).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            CliError::Usage(format!("{} exists; pass --overwrite to replace it", path.display()))
        } else {
            CliError::Other(format!("writing {}: {e}", path.display()))
        }
    })
}

fn same_content(path: &Path, bytes: &[u8]) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64)
        && std::fs::read(path).is_ok_and(|b| b == bytes)
}

/// `documents download`: one statement, or every listed one with --all.
/// Under --all a file already at a statement's name is skipped (`exists`)
/// unless --overwrite; for one statement it is exit 2.
pub fn download(site: &Site, args: &DownloadArgs) -> Result<Downloaded, CliError> {
    let (book, docs) = documents(site, args.account.account.as_deref(), &args.range)?;
    if args.all {
        let dir = args.output.clone().unwrap_or_else(|| PathBuf::from("."));
        private_file::create_dir_all(&dir).map_err(|e| CliError::Other(format!("creating {}: {e}", dir.display())))?;
        let (mut saved, mut skipped, mut as_sent) = (Vec::new(), Vec::new(), Vec::new());
        for (doc, s) in &docs {
            let file = match site.statement_file(&s.id) {
                Ok(f) => f,
                // A session that ended stops the batch: every later read fails too.
                Err(e) if e.exit_code() == 3 => return Err(e),
                Err(e) => {
                    let code = if e.exit_code() == 4 {
                        pk_cli_documents::SKIP_NO_FILE
                    } else {
                        pk_cli_documents::SKIP_UPSTREAM
                    };
                    skipped.push(SkippedDocument::new(doc.id.clone(), e.message()).with_code(code));
                    continue;
                }
            };
            let name = file_name(&book, s, file.extension());
            let path = dir.join(&name);
            if !args.overwrite && std::fs::symlink_metadata(&path).is_ok() {
                let reason = if same_content(&path, &file.bytes) {
                    "already saved"
                } else {
                    "another file is there; --overwrite replaces it"
                };
                skipped.push(SkippedDocument::new(doc.id.clone(), reason).with_code("exists"));
                continue;
            }
            write_private(&path, &file.bytes, args.overwrite)?;
            if file.format.is_none() {
                as_sent.push(name.clone());
            }
            let mut d = doc.clone();
            d.file = Some(name);
            saved.push(SavedDocument::from_document(
                &d,
                path.display().to_string(),
                file.bytes.len() as u64,
            ));
        }
        let batch = DownloadBatch::new(dir.display().to_string(), saved).with_skipped(skipped);
        return Ok(Downloaded::Batch { batch, as_sent });
    }
    let (doc, s) = find_document(&docs, args.id.as_deref().unwrap_or_default())?;
    let file = site.statement_file(&s.id)?;
    if args.output.as_deref().is_some_and(|p| p.as_os_str() == "-") {
        return Ok(Downloaded::Stdout(file.bytes));
    }
    let name = file_name(&book, &s, file.extension());
    let path = match args.output.as_deref() {
        Some(p) if p.is_dir() => p.join(&name),
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(&name),
    };
    write_private(&path, &file.bytes, args.overwrite)?;
    let mut d = doc;
    d.file = Some(name);
    let saved = SavedDocument::from_document(&d, path.display().to_string(), file.bytes.len() as u64);
    Ok(Downloaded::One {
        saved,
        as_sent: file.format.is_none(),
    })
}

// --------------------------------------------------------------- export

fn meta(last_updated: &str) -> Value {
    json!({
        "lastUpdated": last_updated,
        "source": META_SOURCE,
        "tag": [{ "system": TAG_SYSTEM, "code": TAG_MAPPED, "display": TAG_DISPLAY }],
    })
}

/// The stream header for a sign-in.
pub fn header(book: &Book, origin: &str, since: Option<&str>) -> Header {
    let mut h = Header::new(
        Source::new(BIN, env!("CARGO_PKG_VERSION")),
        Organization {
            id: book.mapper.organization_id.clone(),
            name: book.mapper.organization_name.clone(),
            fhir_base: origin.to_string(),
            vendor: Some("flywire".into()),
            location: None,
            aliases: Vec::new(),
        },
        book.context(),
        now_rfc3339(),
    );
    h.method = Some(method::PORTAL.into());
    if let Some(s) = since {
        h.since = Some(s.into());
        h.since_unbounded = vec!["Organization".into(), "Patient".into(), "Account".into()];
    }
    h
}

/// `--patient` and `--org` must name this sign-in's: one sign-in is one
/// patient at one provider. Errors name the id given, never another.
fn check_ids(book: &Book, patient: Option<&str>, org: Option<&str>) -> Result<(), CliError> {
    if let Some(p) = patient.map(str::trim).filter(|p| !p.is_empty()) {
        if p != book.mapper.patient_id {
            return Err(CliError::NotFound(format!(
                "no patient `{p}` at this sign-in: a portal sign-in is one patient"
            ))
            .with_remedy(format!("`{BIN} patients list --profile <NAME>` shows its id")));
        }
    }
    if let Some(o) = org.map(str::trim).filter(|o| !o.is_empty()) {
        if o != book.mapper.organization_id {
            return Err(
                CliError::NotFound(format!("no organization `{o}` at this sign-in: it reads one provider"))
                    .with_remedy(format!("`{BIN} patients list --profile <NAME>` shows its organization")),
            );
        }
    }
    Ok(())
}

/// A bill's Invoice: the model's, with the portal's visit and claim numbers
/// as further identifiers.
fn invoice(book: &Book, b: &bill::Bill, visit: Option<&str>, claim: Option<&str>, now: &str) -> Value {
    let mut v = book.mapper.bill(b, now);
    if let Some(ids) = v.get_mut("identifier").and_then(Value::as_array_mut) {
        let system = |what: &str| format!("urn:{BIN}:{}:{what}", book.mapper.organization_id);
        if let Some(n) = visit {
            ids.push(json!({ "type": { "text": "visit" }, "system": system("visit"), "value": n }));
        }
        if let Some(n) = claim {
            ids.push(json!({ "type": { "text": "claim" }, "system": system("claim"), "value": n }));
        }
    }
    v
}

/// One read of the export: its value, or `None` once a failure that
/// leaves only this item out (a 4 or a 5: the portal's error, an answer
/// flywire cannot read) is written as a skip naming what it was. Anything
/// else ends the export: a session that ended fails every later read too.
fn or_skip<T, W: Write>(
    w: &mut RecordsWriter<W>,
    read: Result<T, CliError>,
    resource_type: &str,
    id: &str,
    what: &str,
) -> Result<Option<T>, CliError> {
    let e = match read {
        Ok(v) => return Ok(Some(v)),
        Err(e) => e,
    };
    let code = match e.exit_code() {
        4 => skip::NOT_FOUND,
        5 => skip::UPSTREAM,
        _ => return Err(e),
    };
    w.skip(Skip::resource(
        resource_type,
        id,
        code,
        format!("{what} could not be read: {}", e.message()),
    ));
    Ok(None)
}

/// `records export`: the stream on `out`. Nothing reaches `out` before the
/// accounts are read, so a refused session is an ordinary error; once the
/// header is out, one item the portal fails on is a skip ([`or_skip`]) and
/// a session that ends ends the stream with an incomplete trailer.
pub fn export<W: Write>(
    site: &Site,
    origin: &str,
    out: W,
    patient: Option<&str>,
    org: Option<&str>,
    since: Option<&str>,
) -> Result<Finished, CliError> {
    let since = since.map(iso_day).transpose()?;
    let since = since.as_deref();
    let book = patient_book(site)?;
    check_ids(&book, patient, org)?;
    let now = now_rfc3339();
    let mut w = RecordsWriter::start(out, &header(&book, origin, since))?;
    let result = (|| -> Result<(), CliError> {
        let m = &book.mapper;
        w.resource(&RawResource::from_value(&json!({
            "resourceType": "Organization",
            "id": m.organization_id,
            "meta": meta(&now),
            "name": m.organization_name,
        }))?)?;
        w.resource(&RawResource::from_value(&json!({
            "resourceType": "Patient",
            "id": m.patient_id,
            "meta": meta(&now),
            "managingOrganization": { "reference": format!("Organization/{}", m.organization_id) },
        }))?)?;
        let provider = book.provider();
        for a in &book.accounts {
            w.resource(&RawResource::from_value(
                &m.account(&reads::account(a, &provider), &now),
            )?)?;
        }
        for a in &book.accounts {
            for b in &a.bills {
                if since.is_some_and(|s| b.date.as_deref().is_some_and(|d| d < s)) {
                    continue;
                }
                let id = m.bill_id(&a.number, &b.id);
                let Some(d) = or_skip(&mut w, site.bill_detail(&b.id), "Invoice", &id, "the bill's page")? else {
                    continue;
                };
                let Some(model) = reads::bill(a, b, Some(&d)) else {
                    w.skip(Skip::resource(
                        "Invoice",
                        m.bill_id(&a.number, &b.id),
                        "unreadable",
                        "the bill came with no date of service",
                    ));
                    continue;
                };
                if since.is_some_and(|s| model.service_start.as_str() < s) {
                    continue;
                }
                let v = invoice(&book, &model, d.visit_number.as_deref(), b.claim.as_deref(), &now);
                w.resource(&RawResource::from_value(&v)?)?;
            }
        }
        for a in &book.accounts {
            let mut unsettled = 0;
            let mut undated = 0;
            let mut seen: BTreeMap<(String, i64), usize> = BTreeMap::new();
            let Some(payments) = or_skip(
                &mut w,
                site.payments(a),
                "PaymentReconciliation",
                "",
                "an account's payments",
            )?
            else {
                continue;
            };
            for p in payments {
                if !p.is_settled() {
                    unsettled += 1;
                    continue;
                }
                let Some(model) = reads::payment(&p) else {
                    undated += 1;
                    continue;
                };
                if since.is_some_and(|s| model.date.as_str() < s) {
                    continue;
                }
                let n = seen.entry((model.date.clone(), model.amount.0)).or_insert(0);
                let v = m.payment(&model, *n, |_| false);
                *n += 1;
                w.resource(&RawResource::from_value(&v)?)?;
            }
            if unsettled > 0 {
                w.skip(Skip::resource(
                    "PaymentReconciliation",
                    "",
                    "not_settled",
                    format!("{unsettled} payment(s) the portal shows as not settled (scheduled, cancelled, returned) were left out"),
                ));
            }
            if undated > 0 {
                w.skip(Skip::resource(
                    "PaymentReconciliation",
                    "",
                    "unreadable",
                    format!("{undated} payment(s) came without a date or an amount"),
                ));
            }
        }
        for a in &book.accounts {
            let Some(statements) = or_skip(
                &mut w,
                site.statements(a),
                "DocumentReference",
                "",
                "an account's statements",
            )?
            else {
                continue;
            };
            for s in statements {
                if !s.is_viewable() {
                    continue;
                }
                let Some(model) = reads::statement(&s) else {
                    w.skip(Skip::resource(
                        "DocumentReference",
                        m.statement_id(&a.number, &s.id),
                        "unreadable",
                        "the statement came without a date",
                    ));
                    continue;
                };
                if since.is_some_and(|x| model.date.as_str() < x) {
                    continue;
                }
                let doc_id = m.statement_id(&a.number, &s.id);
                let file = match site.statement_file(&s.id) {
                    Ok(f) => f,
                    Err(e) if e.exit_code() == 4 => {
                        w.skip(Skip::resource(
                            "DocumentReference",
                            doc_id,
                            skip::NO_FILE,
                            "the portal answered with no file for the statement",
                        ));
                        continue;
                    }
                    Err(e) => {
                        or_skip::<(), _>(&mut w, Err(e), "DocumentReference", &doc_id, "the statement's file")?;
                        continue;
                    }
                };
                let Some(format) = file.format else {
                    w.skip(Skip::resource(
                        "DocumentReference",
                        doc_id,
                        skip::VERIFY_FAILED,
                        format!("the statement's file is in a format {BIN} does not recognize; `{BIN} documents download` saves it as sent"),
                    ));
                    continue;
                };
                if pk_cli_documents::verify_download(&file.bytes, Some(format)).is_err() {
                    w.skip(Skip::resource(
                        "DocumentReference",
                        doc_id,
                        skip::VERIFY_FAILED,
                        "the statement's file did not pass its check",
                    ));
                    continue;
                }
                let binary_id = id_of("bin", &doc_id);
                let title = format!("Statement {}", model.date);
                let doc = m.statement(&model, &binary_id, file.content_type(), file.bytes.len(), &title);
                w.resource(&RawResource::from_value(&doc)?)?;
                w.resource(&RawResource::from_value(&json!({
                    "resourceType": "Binary",
                    "id": binary_id,
                    "meta": meta(&format!("{}T00:00:00Z", model.date)),
                    "contentType": file.content_type(),
                    "data": base64::engine::general_purpose::STANDARD.encode(&file.bytes),
                }))?)?;
            }
        }
        Ok(())
    })();
    w.finish(now_rfc3339(), result.as_ref().err())
}
