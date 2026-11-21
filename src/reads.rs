//! What the portal's answers say, parsed into the bill model (`bill.rs`).
//!
//! The shapes come from the portal app's JavaScript (docs/api.md, *bundle*):
//!
//! - Amounts are whole cents: the app's money formatter divides by 100 by
//!   default. A JSON integer, or a number with no fraction, is read as
//!   cents; a fractional number or a string is an error naming the field
//!   (exit 5), never a guess, and never with the value in the message.
//! - Dates are ISO strings; their first ten characters are the day.
//! - A missing or `null` field is `None`. An entry the readers cannot place
//!   (an account, bill or statement without its id) is left out.
//! - Payments and adjustments are read as positive amounts taken off the
//!   charge, whatever sign the portal gives them.

use base64::Engine as _;
use pk_cli_core::CliError;
use serde_json::Value;

use crate::bill::{self, Amounts, Cents, Charge, Code};

/// One account ("payable") the sign-in shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalAccount {
    /// The portal's id for it, which the per-account queries take.
    pub id: String,
    /// The account number the statement prints.
    pub number: String,
    /// What the patient owes on it now (`standingAmount`).
    pub standing: Option<Cents>,
    /// The provider's name as the portal shows it (`payee`).
    pub payee: Option<String>,
    /// The portal's id for the account's guarantor (`guarantor.externalId`):
    /// personal data, used only hashed into the patient id.
    pub guarantor: Option<String>,
    pub bills: Vec<PortalBill>,
}

/// One bill as the account's list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalBill {
    /// The portal's id for it, which `getSingleBill` takes.
    pub id: String,
    /// The bill's title on its page (`displayName`).
    pub name: Option<String>,
    /// The admission or service date, `YYYY-MM-DD`.
    pub date: Option<String>,
    /// What the patient owes on it.
    pub amount_due: Option<Cents>,
    pub pending_insurance: Option<Cents>,
    /// As the portal gives it; its values are not yet observed.
    pub status: Option<String>,
    /// The portal's claim number for it.
    pub claim: Option<String>,
    /// `HB` (hospital) or `PB` (professional), where given.
    pub visit_type: Option<String>,
}

impl PortalBill {
    pub fn is_open(&self) -> bool {
        self.amount_due.is_some_and(|c| c.0 > 0)
    }

    pub fn is_voided(&self) -> bool {
        self.status.as_deref().is_some_and(|s| {
            let s = s.to_ascii_lowercase();
            s.contains("void") || s.contains("cancel")
        })
    }
}

/// One bill's page: its totals, coverage and charges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BillDetail {
    pub visit_number: Option<String>,
    pub billed: Option<Cents>,
    pub insurance_paid: Option<Cents>,
    pub insurance_adjustments: Option<Cents>,
    pub patient_payments: Option<Cents>,
    pub patient_adjustments: Option<Cents>,
    pub pending_insurance: Option<Cents>,
    /// What the plans are responsible for (`aInsuranceResp`).
    pub insurance_responsibility: Option<Cents>,
    pub coverages: Vec<Coverage>,
    pub charges: Vec<PortalCharge>,
}

/// A plan that covered a bill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub payer: Option<String>,
    pub priority: Option<i64>,
    pub insurance_name: Option<String>,
    pub plan: Option<String>,
}

/// One charge on a bill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalCharge {
    pub date: Option<String>,
    pub description: Option<String>,
    pub amount: Option<Cents>,
    /// The CPT code and its description.
    pub cpt: Option<(String, Option<String>)>,
    /// The revenue code and its description.
    pub revenue: Option<(String, Option<String>)>,
}

/// One payment on an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalPayment {
    /// The account's number.
    pub account: String,
    pub date: Option<String>,
    pub amount: Option<Cents>,
    /// As the portal gives it (`Paid`, `Scheduled`, `Cancelled` are the
    /// app's labels; the raw values are not yet observed).
    pub status: Option<String>,
    /// How it was paid, as the portal words it (`Visa`, `cash`).
    pub method: Option<String>,
}

impl PortalPayment {
    /// Whether the money moved: not scheduled, cancelled, voided, failed,
    /// bounced, returned, charged back or pending.
    pub fn is_settled(&self) -> bool {
        const NOT: &[&str] = &[
            "schedul",
            "cancel",
            "void",
            "fail",
            "bounce",
            "return",
            "chargeback",
            "declin",
            "pending",
            "refund",
        ];
        let s = self.status.as_deref().unwrap_or("").to_ascii_lowercase();
        !NOT.iter().any(|n| s.contains(n))
    }
}

/// One past statement of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalStatement {
    /// The portal's id for it, which `pastStatementsContent` takes.
    pub id: String,
    /// The account's number.
    pub account: String,
    pub date: Option<String>,
    /// The account's balance on the statement.
    pub balance: Option<Cents>,
    pub unique_id: Option<String>,
    pub uploaded_at: Option<String>,
}

impl PortalStatement {
    /// The app lists only statements with a statement id and an upload
    /// time ("uploadedStatements"); the rest have no file to view.
    pub fn is_viewable(&self) -> bool {
        self.unique_id.is_some() && self.uploaded_at.is_some()
    }
}

/// A statement's file, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementFile {
    pub bytes: Vec<u8>,
    /// The format the bytes were recognized as (`pdf`, `png`, `jpeg`,
    /// `tiff`, `heic`), or `None` when they were written as the portal sent
    /// them.
    pub format: Option<&'static str>,
}

impl StatementFile {
    pub fn content_type(&self) -> &'static str {
        match self.format {
            Some("pdf") => "application/pdf",
            Some("png") => "image/png",
            Some("jpeg") => "image/jpeg",
            Some("tiff") => "image/tiff",
            Some("heic") => "image/heic",
            _ => "application/octet-stream",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self.format {
            Some("jpeg") => "jpg",
            Some(f) => f,
            None => "bin",
        }
    }
}

// ------------------------------------------------------------- fields

fn upstream(what: &str) -> CliError {
    CliError::Upstream(format!(
        "the portal's {what} is not a whole number of cents; the portal may have changed (`{} api POST /graphql` \
         shows the answer)",
        crate::cli::BIN
    ))
}

/// Ten trillion dollars, in cents: far beyond any bill, and small enough
/// that any sum of the amounts flywire reads stays in an `i64`.
const MAX_CENTS: i64 = 1_000_000_000_000_000;

/// An amount in cents at `v[key]`, `what` naming it in an error.
fn cents(v: &Value, key: &str, what: &str) -> Result<Option<Cents>, CliError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            // Bounded like the float path: no sum or `abs` of them overflows.
            if let Some(i) = n.as_i64() {
                return if i.unsigned_abs() < MAX_CENTS as u64 {
                    Ok(Some(Cents(i)))
                } else {
                    Err(upstream(&format!("{what}.{key}")))
                };
            }
            match n.as_f64() {
                Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < MAX_CENTS as f64 => Ok(Some(Cents(f as i64))),
                _ => Err(upstream(&format!("{what}.{key}"))),
            }
        }
        Some(_) => Err(upstream(&format!("{what}.{key}"))),
    }
}

/// A positive amount: payments and adjustments taken off a charge.
fn taken_off(v: &Value, key: &str, what: &str) -> Result<Option<Cents>, CliError> {
    Ok(cents(v, key, what)?.map(|c| Cents(c.0.abs())))
}

/// Text at `v[key]`: a string, trimmed, or a number written out.
fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The day of an ISO date or date-time at `v[key]`.
fn day(v: &Value, key: &str) -> Option<String> {
    let s = text(v, key)?;
    let d = s.get(..10)?;
    pk_cli_core::dates::parse_iso(d).ok()?;
    Some(d.to_string())
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// A code object `{code, translation}` at `v[key]`; a numeric code is
/// padded to `width` digits (CPT 5, revenue codes 4), since a number loses
/// its leading zeros.
fn code(v: &Value, key: &str, width: usize) -> Option<(String, Option<String>)> {
    let c = v.get(key)?;
    let raw = match c.get("code")? {
        Value::Number(n) => format!("{:0>width$}", n.as_u64()?),
        Value::String(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return None,
    };
    Some((raw, text(c, "translation")))
}

// ------------------------------------------------------------ answers

/// `payables`' `data`: the accounts, with their bills, newest first.
pub fn payables(data: &Value) -> Result<Vec<PortalAccount>, CliError> {
    let list = data.pointer("/payables/payables").unwrap_or(&Value::Null);
    let mut accounts = Vec::new();
    for p in list.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let Some(id) = text(p, "id") else { continue };
        let number = text(p, "accountNumber")
            .or_else(|| text(p, "accountId"))
            .unwrap_or_else(|| id.clone());
        let payee = p
            .get("payee")
            .and_then(|y| text(y, "companyDisplayName").or_else(|| text(y, "companyName")));
        let mut bills = Vec::new();
        for b in array(p, "bills") {
            let Some(bill_id) = text(b, "id") else { continue };
            bills.push(PortalBill {
                id: bill_id,
                name: text(b, "displayName"),
                date: day(b, "admissionDate"),
                amount_due: cents(b, "amountDue", "payables.bills")?,
                pending_insurance: cents(b, "aPendingInsurance", "payables.bills")?,
                status: text(b, "status"),
                claim: text(b, "claimNo"),
                visit_type: text(b, "rawVisitType"),
            });
        }
        bills.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.id.cmp(&b.id)));
        accounts.push(PortalAccount {
            id,
            number,
            standing: cents(p, "standingAmount", "payables")?,
            payee,
            guarantor: p.get("guarantor").and_then(|g| text(g, "externalId")),
            bills,
        });
    }
    Ok(accounts)
}

/// `getSingleBill`'s `data`.
pub fn bill_detail(data: &Value) -> Result<BillDetail, CliError> {
    let b = data.get("bill").unwrap_or(&Value::Null);
    let at = "bill";
    let mut coverages: Vec<Coverage> = array(b, "planCoverages")
        .iter()
        .map(|c| Coverage {
            payer: text(c, "payer"),
            priority: c.get("priority").and_then(Value::as_i64),
            insurance_name: text(c, "insuranceName"),
            plan: text(c, "insurancePlan"),
        })
        .collect();
    coverages.sort_by_key(|c| c.priority.unwrap_or(i64::MAX));
    let mut charges = Vec::new();
    for c in array(b, "charges") {
        charges.push(PortalCharge {
            date: day(c, "date"),
            description: text(c, "description"),
            amount: cents(c, "amount", "bill.charges")?,
            cpt: code(c, "cptCode", 5),
            revenue: code(c, "revenueCode", 4),
        });
    }
    Ok(BillDetail {
        visit_number: text(b, "visitNumber"),
        billed: cents(b, "aTotalBilled", at)?,
        insurance_paid: taken_off(b, "insuranceAmount", at)?,
        insurance_adjustments: taken_off(b, "aTotalInsuranceAdjustments", at)?,
        patient_payments: taken_off(b, "aPatientPayments", at)?,
        patient_adjustments: taken_off(b, "aTotalPatientAdjustments", at)?,
        pending_insurance: cents(b, "aPendingInsurance", at)?,
        insurance_responsibility: cents(b, "aInsuranceResp", at)?,
        coverages,
        charges,
    })
}

/// `previousPayments`' `data` for one account, newest first.
pub fn payments(data: &Value, account: &str) -> Result<Vec<PortalPayment>, CliError> {
    let mut out = Vec::new();
    for p in array(data, "previousPayments") {
        let method = p.get("paymentMethod").and_then(|m| {
            text(m, "description")
                .or_else(|| text(m, "paymentSubType"))
                .or_else(|| text(m, "paymentType"))
        });
        out.push(PortalPayment {
            account: account.to_string(),
            date: day(p, "date"),
            amount: cents(p, "amount", "previousPayments")?,
            status: text(p, "status"),
            method,
        });
    }
    out.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(out)
}

/// `pastStatementsInfo`'s `data` for one account, newest first.
pub fn statements(data: &Value, account: &str) -> Result<Vec<PortalStatement>, CliError> {
    let mut out = Vec::new();
    for s in array(data, "pastStatementsInfo") {
        let Some(id) = text(s, "id") else { continue };
        out.push(PortalStatement {
            id,
            account: account.to_string(),
            date: day(s, "createdAt"),
            balance: cents(s, "balanceSnapshot", "pastStatementsInfo")?,
            unique_id: text(s, "statementUniqueId"),
            uploaded_at: text(s, "paperUploadedAt"),
        });
    }
    out.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// `pastStatementsContent`'s `data`: the file.
///
/// `preview` is a data URL or base64 (*bundle*: the app shows it as an
/// image or a PDF by `imageFormat`). The decoded bytes are the format
/// `imageFormat` (or the data URL's media type) declares when they pass
/// that format's check, else the format their magic bytes show. Bytes in
/// no format flywire knows are the text as the portal sent it, with no
/// format, so `documents download` saves them as given (docs/api.md).
pub fn statement_file(data: &Value) -> Result<StatementFile, CliError> {
    let c = data.get("pastStatementsContent").unwrap_or(&Value::Null);
    let Some(preview) = c
        .get("preview")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
    else {
        return Err(CliError::NotFound(
            "the portal answered with no file for that statement".into(),
        ));
    };
    let mut declared = c.get("imageFormat").and_then(Value::as_str).and_then(format_name);
    let payload = match preview
        .trim_start()
        .strip_prefix("data:")
        .and_then(|r| r.split_once(','))
    {
        Some((meta, body)) => {
            declared = declared.or_else(|| format_name(meta.split(';').next().unwrap_or("")));
            if !meta.ends_with(";base64") {
                return Ok(recognize(body.as_bytes().to_vec(), declared));
            }
            body
        }
        None => preview,
    };
    let compact: String = payload.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(compact.as_bytes()) {
        let file = recognize(bytes, declared);
        if file.format.is_some() {
            return Ok(file);
        }
    }
    Ok(StatementFile {
        bytes: preview.as_bytes().to_vec(),
        format: None,
    })
}

/// A format name or media type as one of the formats flywire saves.
fn format_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pdf" | "application/pdf" => Some("pdf"),
        "png" | "image/png" => Some("png"),
        "jpg" | "jpeg" | "image/jpeg" | "image/jpg" => Some("jpeg"),
        "tif" | "tiff" | "image/tiff" => Some("tiff"),
        "heic" | "image/heic" => Some("heic"),
        _ => None,
    }
}

/// `bytes` as `declared` when they pass its check, else as their magic
/// bytes show, else in no known format.
fn recognize(bytes: Vec<u8>, declared: Option<&'static str>) -> StatementFile {
    let format = declared
        .filter(|f| pk_cli_documents::verify_download(&bytes, Some(f)).is_ok())
        .or_else(|| match pk_cli_documents::sniff(&bytes) {
            Some(f @ ("pdf" | "png" | "jpeg" | "tiff" | "heic")) => Some(f),
            _ => None,
        });
    StatementFile { bytes, format }
}

// ----------------------------------------------------------- the model

/// The bill model's account, at `provider`.
pub fn account(a: &PortalAccount, provider: &bill::Provider) -> bill::Account {
    bill::Account {
        number: a.number.clone(),
        provider: provider.clone(),
        patient_name: None,
        guarantor_name: None,
        balance: None,
        patient_balance: a.standing,
        insurance_pending: sum(a.bills.iter().map(|b| b.pending_insurance)),
        active: true,
    }
}

fn sum(it: impl Iterator<Item = Option<Cents>>) -> Option<Cents> {
    let vals: Vec<i64> = it.flatten().map(|c| c.0).collect();
    (!vals.is_empty()).then(|| Cents(vals.iter().sum()))
}

fn add(a: Option<Cents>, b: Option<Cents>) -> Option<Cents> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(Cents(a.map_or(0, |c| c.0) + b.map_or(0, |c| c.0))),
    }
}

const CPT: &str = "http://www.ama-assn.org/go/cpt";
const REVENUE: &str = "https://www.nubc.org/CodeSystem/RevenueCodes";

/// The bill model's bill: the list's row with its page where read. `None`
/// for a bill with no date of service anywhere.
pub fn bill(a: &PortalAccount, b: &PortalBill, d: Option<&BillDetail>) -> Option<bill::Bill> {
    let lines: Vec<Charge> = d
        .map(|d| {
            d.charges
                .iter()
                .filter_map(|c| {
                    let date = c.date.clone().or_else(|| b.date.clone())?;
                    let code = c
                        .cpt
                        .as_ref()
                        .map(|(code, display)| Code {
                            system: Some(CPT.into()),
                            code: code.clone(),
                            display: display.clone(),
                        })
                        .or_else(|| {
                            c.revenue.as_ref().map(|(code, display)| Code {
                                system: Some(REVENUE.into()),
                                code: code.clone(),
                                display: display.clone(),
                            })
                        });
                    Some(Charge {
                        date,
                        code,
                        description: c.description.clone(),
                        quantity: None,
                        amounts: Amounts {
                            billed: c.amount,
                            ..Amounts::default()
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let first_line = lines.iter().map(|l| l.date.clone()).min();
    let last_line = lines.iter().map(|l| l.date.clone()).max();
    let start = b.date.clone().or_else(|| first_line.clone())?;
    let end = last_line.filter(|l| l > &start);
    let insurer = d.and_then(|d| {
        d.coverages
            .first()
            .and_then(|c| c.insurance_name.clone().or_else(|| c.payer.clone()))
    });
    let totals = Amounts {
        billed: d.and_then(|d| d.billed),
        allowed: None,
        insurance_paid: d.and_then(|d| d.insurance_paid),
        adjustment: d.and_then(|d| add(d.insurance_adjustments, d.patient_adjustments)),
        patient_paid: d.and_then(|d| d.patient_payments),
        patient_owes: b.amount_due,
    };
    Some(bill::Bill {
        number: b.id.clone(),
        account: a.number.clone(),
        service_start: start,
        service_end: end,
        posted: None,
        rendering: None,
        description: b.name.clone(),
        insurer,
        insurer_claim: None,
        lines,
        totals,
        voided: b.is_voided(),
    })
}

/// The bill model's payment. `None` for one without a date or an amount.
pub fn payment(p: &PortalPayment) -> Option<bill::Payment> {
    Some(bill::Payment {
        number: None,
        account: p.account.clone(),
        date: p.date.clone()?,
        amount: p.amount?,
        method: p.method.clone(),
        bills: Vec::new(),
    })
}

/// The bill model's statement. `None` for one without a date.
pub fn statement(s: &PortalStatement) -> Option<bill::Statement> {
    Some(bill::Statement {
        number: s.id.clone(),
        account: s.account.clone(),
        date: s.date.clone()?,
        amount_due: s.balance,
        due_date: None,
        bills: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn amounts_are_whole_cents_or_an_error_naming_the_field() {
        let v = json!({"a": 1234, "b": 1234.0, "c": null, "d": 12.5, "e": "12.50", "f": -300});
        assert_eq!(cents(&v, "a", "x").unwrap(), Some(Cents(1234)));
        assert_eq!(cents(&v, "b", "x").unwrap(), Some(Cents(1234)));
        assert_eq!(cents(&v, "c", "x").unwrap(), None);
        assert_eq!(cents(&v, "missing", "x").unwrap(), None);
        assert_eq!(taken_off(&v, "f", "x").unwrap(), Some(Cents(300)));
        let huge = json!({"min": i64::MIN, "max": i64::MAX, "f": 1e300});
        for key in ["min", "max", "f"] {
            assert_eq!(taken_off(&huge, key, "x").err().unwrap().exit_code(), 5, "{key}");
        }
        for bad in ["d", "e"] {
            let e = cents(&v, bad, "payables.bills").err().unwrap();
            assert_eq!(e.exit_code(), 5);
            assert!(e.to_string().contains(&format!("payables.bills.{bad}")), "{e}");
            assert!(!e.to_string().contains("12.5"), "{e}");
        }
    }

    #[test]
    fn dates_and_codes_are_read_tolerantly() {
        let v = json!({"d": "2026-01-05T10:00:00-05:00", "e": "2026-01-05", "f": "Jan 5", "g": 7,
            "cpt": {"code": 99213, "translation": "Office visit"}, "low": {"code": 450}, "none": {"code": null}});
        assert_eq!(day(&v, "d").as_deref(), Some("2026-01-05"));
        assert_eq!(day(&v, "e").as_deref(), Some("2026-01-05"));
        assert_eq!(day(&v, "f"), None);
        assert_eq!(day(&v, "g"), None);
        assert_eq!(code(&v, "cpt", 5), Some(("99213".into(), Some("Office visit".into()))));
        assert_eq!(code(&v, "low", 4), Some(("0450".into(), None)));
        assert_eq!(code(&v, "none", 4), None);
    }

    #[test]
    fn a_statement_file_is_decoded_when_its_format_is_known() {
        let pdf = b"%PDF-1.4\n1 0 obj << >> endobj\n%%EOF\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(pdf);
        for preview in [b64.clone(), format!("data:application/pdf;base64,{b64}")] {
            let f =
                statement_file(&json!({"pastStatementsContent": {"imageFormat": "pdf", "preview": preview}})).unwrap();
            assert_eq!(f.bytes, pdf);
            assert_eq!(
                (f.format, f.content_type(), f.extension()),
                (Some("pdf"), "application/pdf", "pdf")
            );
        }
        // The declared format wins only when the bytes pass its check.
        let f = statement_file(&json!({"pastStatementsContent": {"imageFormat": "PNG", "preview": b64}})).unwrap();
        assert_eq!(f.format, Some("pdf"));
        let f =
            statement_file(&json!({"pastStatementsContent": {"imageFormat": null, "preview": "<p>x</p>"}})).unwrap();
        assert_eq!(
            (f.bytes.as_slice(), f.format, f.extension()),
            (&b"<p>x</p>"[..], None, "bin")
        );
        let f = statement_file(&json!({"pastStatementsContent": {"preview": "data:text/html,<p>x</p>"}})).unwrap();
        assert_eq!((f.bytes.as_slice(), f.format), (&b"<p>x</p>"[..], None));
        let e = statement_file(&json!({"pastStatementsContent": null})).err().unwrap();
        assert_eq!(e.exit_code(), 4);
    }

    #[test]
    fn payments_that_did_not_settle_are_told_apart() {
        let p = |s: Option<&str>| PortalPayment {
            account: "A".into(),
            date: None,
            amount: None,
            status: s.map(str::to_string),
            method: None,
        };
        for settled in [None, Some("Paid"), Some("success"), Some("settled")] {
            assert!(p(settled).is_settled(), "{settled:?}");
        }
        for not in ["Scheduled", "cancelled", "VOID", "bounced", "pending"] {
            assert!(!p(Some(not)).is_settled(), "{not}");
        }
    }
}
