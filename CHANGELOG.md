# Changelog

## Unreleased

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
  --git` and the medical-common dependency (still v0.6.9) follow it.
- CI's `security` job runs the gitleaks CLI 8.30.1, pinned by checksum, over
  the whole history: gitleaks-action needs a paid license on an
  organization's repository.
