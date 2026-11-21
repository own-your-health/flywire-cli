//! The GraphQL documents flywire's readers send: the portal app's own
//! queries (docs/api.md, *bundle*), trimmed to the fields the readers use.
//! Each is checked by `client::check_document` before it is sent, like any
//! document.

/// The accounts ("payables") and their bills. `filterClosedBills: false`
/// asks for closed bills too; the app sends `true`.
pub const PAYABLES: &str = "query payables($filterClosedBills: Boolean) { \
payables(filterClosedBills: $filterClosedBills) { payables { id accountNumber accountId standingAmount \
bills { id admissionDate amountDue displayName status claimNo aPendingInsurance rawVisitType } \
payee { companyName companyDisplayName } } } }";

/// One bill's totals, the plans that covered it and its charges.
pub const SINGLE_BILL: &str = "query getSingleBill($id: ID!) { bill(id: $id) { visitNumber accountNumber \
aTotalBilled insuranceAmount aTotalInsuranceAdjustments aPatientPayments aTotalPatientAdjustments \
aPendingInsurance aInsuranceResp planCoverages { payer priority insuranceName insurancePlan } \
charges { amount date description cptCode { code translation } revenueCode { code translation } } } }";

/// The payments made on the accounts named.
pub const PREVIOUS_PAYMENTS: &str = "query previousPayments($payableIds: [ID!]!) { \
previousPayments(payableIds: $payableIds) { amount date status paymentMethod { paymentType paymentSubType \
description } } }";

/// An account's past statements.
pub const PAST_STATEMENTS: &str = "query pastStatementsInfo($payableIds: [ID!]!, $accountNumber: String!) { \
pastStatementsInfo(payableIds: $payableIds, accountNumber: $accountNumber) { id createdAt balanceSnapshot \
statementUniqueId paperUploadedAt } }";

/// One statement's file.
pub const STATEMENT_CONTENT: &str = "query pastStatementsContent($statementId: ID!) { \
pastStatementsContent(id: $statementId) { imageFormat preview } }";

/// Every document with its operation name, for the tests and `docs/api.md`.
pub const ALL: &[(&str, &str)] = &[
    ("payables", PAYABLES),
    ("getSingleBill", SINGLE_BILL),
    ("previousPayments", PREVIOUS_PAYMENTS),
    ("pastStatementsInfo", PAST_STATEMENTS),
    ("pastStatementsContent", STATEMENT_CONTENT),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reader_query_is_a_read_flywire_sends() {
        for (operation, document) in ALL {
            crate::client::check_document(document).unwrap_or_else(|e| panic!("{operation}: {e}"));
            assert!(
                document.starts_with(&format!("query {operation}(")),
                "{operation} names its operation"
            );
        }
    }
}
