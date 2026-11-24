# Changelog

## Unreleased

- While `FLYWIRE_KEYCHAIN_SERVICE` names a test's keychain service, a command
  that would send a request without a loopback `FLYWIRE_BASE_URL` is exit 2
  before sending it, so a test run never reaches the real portal.
- An answer without the accounts' list (`payables` null, and no GraphQL
  error) is exit 5 instead of a sign-in with no accounts, so `records
  export` never writes a complete stream of nothing. A bill answered
  without the bill is exit 5 for `bills show` and a skip in the export,
  instead of a bill with every total missing.
- `scripts/fixture-server.py` answers a request without the `source:
  patient-client` header with HTTP 400 "no source found", as the portal
  does, and `make dogfood` covers the two empty answers.
- medical-common v0.6.11 and cli-common v0.33.5: the bill's code systems,
  price components, document types, `Money` and totals come from
  `pk_med_records::bill` (DESIGN §1.10) instead of the CLI's own copy. Two
  displays change to the profile's: `insurance-adjustment` is "Insurance
  adjustment" (was "Adjustment") and `patient-responsibility` is "Patient
  responsibility" (was "Patient owes"). Codes, types, amounts and ids
  are unchanged.
- Every request sends the portal app's `source: patient-client` header; without it `/graphql` answers HTTP 400 "no source found", which `auth login` read as not signed in and so never confirmed a real sign-in.
- `flywire`: a patient's bills from a Flywire patient portal
  (`<provider>.simpleepay.com`), read-only.
- `profiles list|add|remove`; every command that reads the portal takes
  `--profile`.
- `auth login --browser --provider <PROVIDER>` signs in through a Chrome
  window in the profile's own browser profile (the portal's guest form);
  `auth status|logout|reset`.
- `api POST /graphql --data '{"query": ...}'` sends one GraphQL query;
  mutations and queries that do more than read are refused before a
  request. `api GET <PATH>` reads a path on the portal; payment, sign-in,
  subscription and settings paths are refused.
- `config path|show`, `info`, `completions`, `self-update`.
- The bill readers, each taking `--profile`: `patients list`, `accounts
  list`, `bills list [--open] [--account] [--since] [--until] [--limit]`,
  `bills show <ID>`, `payments list`, `documents list` and `documents
  download <ID>|--all -o <DIR>` (documents/v1). They send the portal app's
  own queries (`payables` with closed bills, `getSingleBill`,
  `previousPayments`, `pastStatementsInfo`, `pastStatementsContent`),
  trimmed to the fields read. `patients list` and `records export` also
  read the accounts' guarantor id, by its own query, for a patient id that
  holds while accounts come and go. An amount that is not whole cents is exit 5
  naming the field.
- `records export --ndjson` (records-stream/v1, `method: portal`): the
  Organization, the Patient, the Accounts, an Invoice per bill with its
  charges as contained ChargeItems and its amounts in
  `urn:pk-med:bill-component`, settled payments as PaymentReconciliation,
  and each statement as a DocumentReference followed by its Binary.
  `-o <DIR>` writes a file, against a test's server only.
- `info` lists the new commands and the `health-records/v1` and
  `documents/v1` profiles.
- `make dogfood` drives the installed binary through every command and
  error path against a loopback server serving the synthetic fixtures.
- The repository moved to own-your-health
  (github.com/own-your-health/flywire-cli); `self-update`, `cargo install
  --git` and the medical-common dependency follow it.
- `url` 2.5.9, for RUSTSEC-2026-0592 (a stack overflow in `Url::origin()`).
- CI's `security` job runs the gitleaks CLI 8.30.1, pinned by checksum, over
  the whole history: gitleaks-action needs a paid license on an
  organization's repository.
- The export follows medrec's bill mapping (docs/fhir.md): every `Money`
  value is a JSON number in the bill's decimal spelling (`125.00`, kept
  by serde_json's `arbitrary_precision`); a bill without lines holds its
  date of service in a ChargeItem of its own; `totalPriceComponent` is the
  bill's breakdown, its billed amount the `base` `billed` component, and
  `totalGross` equals `totalNet`, what the patient owes; a payment's
  detail names the provider paid (`payee`) and drops `paymentIssuer`,
  which R4 allows for an Organization only; a statement is typed in
  `urn:pk-med:doc-type` and its `context.related` names its Account.
