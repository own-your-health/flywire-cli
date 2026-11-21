//! The portal's GraphQL: which documents `flywire` sends, and how an answer
//! says nobody is signed in.
//!
//! - [`read_only_graphql`]: a document of `query` (and `fragment`)
//!   definitions only, so a mutation or subscription is never sent.
//! - [`refused_field`]: a query field that does something besides reading
//!   (validating a card, verifying a code, a financing partner's lookup) is
//!   refused too. GraphQL calls those queries; flywire does not send them.
//! - [`is_signed_out`]: the portal's app treats the errors "Unauthorized
//!   Request" and "Session Expired/Account Not Logged In" as a session that
//!   has ended (docs/api.md), and so does flywire.

use serde_json::Value;

/// Whether a GraphQL document holds queries only: every definition a
/// `query`, a `{ ... }` shorthand query or a `fragment`. Comments and strings
/// are skipped, so no operation hides in one, and a document this cannot
/// read (an unclosed string or brace) is not read-only.
pub fn read_only_graphql(doc: &str) -> bool {
    let b = doc.as_bytes();
    let (mut i, mut depth, mut open, mut definitions) = (0, 0usize, true, 0);
    while i < b.len() {
        let c = b[i];
        match c {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' if b[i..].starts_with(b"\"\"\"") => {
                i += 3;
                loop {
                    if i >= b.len() {
                        return false;
                    }
                    if b[i..].starts_with(b"\\\"\"\"") {
                        i += 4;
                    } else if b[i..].starts_with(b"\"\"\"") {
                        i += 3;
                        break;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b'"' => {
                i += 1;
                loop {
                    match b.get(i) {
                        None | Some(b'\n') => return false,
                        Some(b'\\') => i += 2,
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                continue;
            }
            b'{' | b'(' | b'[' => {
                if depth == 0 && open {
                    if c != b'{' {
                        return false;
                    }
                    open = false;
                    definitions += 1;
                }
                depth += 1;
            }
            b'}' | b')' | b']' => {
                let Some(d) = depth.checked_sub(1) else {
                    return false;
                };
                depth = d;
                if depth == 0 && c == b'}' {
                    open = true;
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                if depth == 0 && open {
                    if !matches!(&doc[start..i], "query" | "fragment") {
                        return false;
                    }
                    open = false;
                    definitions += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    depth == 0 && open && definitions > 0
}

/// Words that start a query field (or a variable) which does more than
/// read, at a camel-case boundary: `validateCreditCard`, `verifyCode`,
/// `setLanguage`. `settings` and `payables` are reads.
const ACTION_PREFIXES: &[&str] = &[
    "validate",
    "verify",
    "login",
    "logout",
    "signin",
    "signout",
    "check",
    "create",
    "update",
    "remove",
    "delete",
    "set",
    "subscribe",
    "unsubscribe",
    "resend",
    "send",
    "change",
    "edit",
    "pay",
    "enroll",
    "dispute",
    "cancel",
    "submit",
    "paypal",
    "digitalwallet",
];
/// Names that, anywhere in a field, reach a financing partner or another
/// system through the portal.
const ACTION_PARTS: &[&str] = &["curae", "carecredit", "cerner", "financed", "getoffer", "fulloffers"];

/// The first name in `doc` (a field, an argument, a variable) that
/// [`ACTION_PREFIXES`] or [`ACTION_PARTS`] refuses, outside strings and
/// comments; `None` when there is none.
pub fn refused_field(doc: &str) -> Option<String> {
    let b = doc.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' if b[i..].starts_with(b"\"\"\"") => {
                i += 3;
                while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                    i += if b[i..].starts_with(b"\\\"\"\"") { 4 } else { 1 };
                }
                i += 3;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let name = &doc[start..i];
                if names_an_action(name) {
                    return Some(name.to_string());
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn names_an_action(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if ACTION_PARTS.iter().any(|p| lower.contains(p)) {
        return true;
    }
    ACTION_PREFIXES.iter().any(|p| {
        lower.starts_with(p)
            && name[p.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
    })
}

/// The errors the portal answers a request with when nobody is signed in,
/// as its app matches them (without Apollo's `GraphQL error: ` prefix).
const SIGNED_OUT: &[&str] = &["unauthorized request", "session expired/account not logged in"];

/// Whether a GraphQL answer's `errors` say the session has ended.
pub fn is_signed_out(doc: &Value) -> bool {
    doc.get("errors").and_then(Value::as_array).is_some_and(|errors| {
        errors.iter().any(|e| {
            let m = e.get("message").and_then(Value::as_str).unwrap_or("");
            let m = m.trim().to_ascii_lowercase();
            let m = m.strip_prefix("graphql error: ").unwrap_or(&m);
            SIGNED_OUT.iter().any(|s| m.starts_with(s)) || m == "unauthorized"
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_queries_are_read_only() {
        for ok in [
            "query { a }",
            "{ a { b } }",
            "query A($d: String = \"mutation { x }\") { a(d: $d) { mutation } }",
            "# mutation { x }\nquery A { a }",
            "query A { a(t: \"\"\"mutation \\\"\"\" { x }\"\"\") }",
            "query A { ...F } fragment F on T { b }",
            "query A @cached(ttl: 1) { a }",
        ] {
            assert!(read_only_graphql(ok), "{ok}");
        }
        for refused in [
            "mutation { a }",
            "subscription { a }",
            "query A { a } mutation B { b }",
            "query A { a }\n# x\nmutation { b }",
            "Mutation { a }",
            "query A { a ",
            "query A { a } }",
            "query A { a(t: \"unclosed) }",
            "query A { a(t: \"\"\"unclosed) }",
            "",
            "# only a comment",
            "(x) { a }",
        ] {
            assert!(!read_only_graphql(refused), "{refused}");
        }
    }

    #[test]
    fn fields_that_do_more_than_read_are_refused() {
        for read in [
            "query payables($filterClosedBills: Boolean = true) { payables(filterClosedBills: $filterClosedBills) { payables { id accountNumber standingAmount } } }",
            "query getSingleBill($id: ID!) { bill(id: $id) { visitNumber charges { amount date description } } }",
            "query previousPayments($payableIds: [ID!]!) { previousPayments(payableIds: $payableIds) { amount date status } }",
            "query settings { settings { id settingName } }",
            "query payableStandingAmount { payableStandingAmount }",
            "query paymentStatusCheck($id: ID!) { paymentStatusCheck(id: $id) }",
            "query A { a(note: \"validateCreditCard\") } # verifyCode",
        ] {
            assert_eq!(refused_field(read), None, "{read}");
        }
        for (refused, name) in [
            (
                "query validateCreditCard($n: String!) { validateCreditCard(n: $n) }",
                "validateCreditCard",
            ),
            ("query verifyCode($c: String!) { verifyCode(code: $c) }", "verifyCode"),
            (
                "query getCuraeAccountStatus { getCuraeAccountStatus { ok } }",
                "getCuraeAccountStatus",
            ),
            (
                "query careCreditAuthenticationDetails { careCreditAuthenticationDetails { x } }",
                "careCreditAuthenticationDetails",
            ),
            ("query financedOffers { financedOffers { x } }", "financedOffers"),
            ("query A { setLanguage }", "setLanguage"),
            ("query A { pay }", "pay"),
        ] {
            assert_eq!(refused_field(refused).as_deref(), Some(name), "{refused}");
        }
    }

    #[test]
    fn the_portal_s_signed_out_errors_are_recognized() {
        for signed_out in [
            json!({"errors": [{"message": "Unauthorized Request"}]}),
            json!({"errors": [{"message": "Session Expired/Account Not Logged In"}], "data": null}),
            json!({"errors": [{"message": "GraphQL error: Session Expired/Account Not Logged In SSO Redirect"}]}),
        ] {
            assert!(is_signed_out(&signed_out), "{signed_out}");
        }
        for other in [
            json!({"data": {"payableStandingAmount": 0}}),
            json!({"errors": [{"message": "Internal server error"}]}),
            json!({"errors": []}),
        ] {
            assert!(!is_signed_out(&other), "{other}");
        }
    }
}
