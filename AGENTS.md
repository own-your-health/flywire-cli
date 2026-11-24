# AGENTS.md — flywire-cli

`flywire` is a read-only CLI for a patient's medical bills on a Flywire
patient portal (the Simplee platform, `https://<provider>.simpleepay.com`,
one per hospital or health system): each account's balance, the visits
billed with their charges, insurance payments, adjustments and what the
patient owes, payments made, and past statements. It sends the `query`
documents the portal's own app sends to its GraphQL endpoint, signed in per
profile, and emits FHIR for the household's
medical-records system (`records-stream/v1`, `medrec import --from
flywire`). It conforms to piekstra-cli/1 and the `health-records/v1`
profile, on the `pk-cli-*` crates from
[cli-common](https://github.com/piekstra/cli-common) and `pk-med-records`
from [medical-common](https://github.com/own-your-health/medical-common). Every
`pk-cli-*` stays on the cli-common tag medical-common pins.

## Layout

| File | Owns |
|---|---|
| `src/cli.rs` | the clap tree |
| `src/app.rs` | dispatch, argument checks, profiles, config, info, output |
| `src/auth.rs` | `auth login`, `status`, `logout` of one `--profile`, `auth reset`: the browser-profile seams |
| `src/session.rs` | `SessionStore`: get, put and delete a profile's session in the vault; the only code that knows where it lives |
| `src/site.rs` | the provider (`Provider`: one label on simpleepay.com), the sign-in: the browser capture and the session check (`SESSION_CHECK`, `judge`) |
| `src/client.rs` | the reads: `query` (one GraphQL query's `data`), pacing, the cookie jar, `refuse` (answers to exit codes), and `api_request`, which checks an `api` request is a read before it is sent |
| `src/graphql.rs` | `read_only_graphql` (queries and fragments only), `refused_field` (queries that do more than read), `is_signed_out` (the portal's signed-out errors) |
| `src/bill.rs` | the bill model (accounts, bills and their charge lines, payments, statements, receipts) and its FHIR mapping (`docs/fhir.md`). The same file is in flywire-cli, peryourhealth-cli and embryooptions-cli, differing only in the CLI's name in two doc comments: a change to it is made in all three. It needs serde_json's `arbitrary_precision` (Cargo.toml) to keep money's decimal spelling; its unit test fails without it |
| `src/portal.rs` | the signed-in session (`Env`: where sessions are kept and which site is read; `Site`: each reader query, and the write-back of refreshed cookies) |
| `src/queries.rs` | the GraphQL documents the readers send, the app's own trimmed to the fields read |
| `src/reads.rs` | the portal's answers parsed (amounts in cents, days, codes, the statement file) and turned into the bill model |
| `src/readers.rs` | the reader commands' documents (`patients`, `accounts`, `bills`, `payments`, `documents`) and the `records export` stream |
| `docs/api.md` | the site as observed, what each command reads, and the traps |
| `docs/fhir.md` | the FHIR `records export` writes |
| `tests/fixtures/graphql/` | synthetic answers, one file per operation (and id), shaped from the app's JavaScript |
| `scripts/fixture-server.py`, `scripts/dogfood.sh` | `make dogfood`: the installed binary against the fixtures |

## Medical data rules

These come from the household's plan and are not negotiable.

- The repository carries no personal data, in code, fixtures, docs, commits,
  pull requests or issues: no account numbers, statement or transaction
  numbers, names, dates of birth, the providers (portal names) the household
  uses, amounts or dates of service. Examples use `exampleclinic`. Fixtures are synthetic; `tests/no_personal_data.rs`
  scans every tracked file for the shapes personal data takes.
- Records and their contents are never sent to an outside service: no
  health-data aggregator, cloud OCR or other company's API. A Claude session
  is not one of these (the household's decision, 2026-11-17). A session may
  read, print and summarize the household's real bills when the work needs
  it: debugging a real export, mapping the site with real answers, checking
  a pull. Real data stays out of tracked files, commits, pull requests and
  issues. A raw capture is deleted from `/tmp` once it has served.
- No FHIR or SMART libraries, no aggregators. The FHIR here is mapped by
  hand in `src/bill.rs`; general-purpose crates are fine.
- Read-only. Nothing writes to the portal: every request is a GraphQL
  `query` (checked by `graphql::read_only_graphql` and
  `graphql::refused_field` before it is sent) or a `GET`. It never pays, schedules or cancels a
  payment, stores or changes a payment method, enrolls in autopay or a
  payment plan, changes paperless or contact preferences, sends a message
  or disputes a charge, and never sends a query that validates a card,
  verifies a code or calls a financing partner. A write would need SPEC §1.3's confirmation gate and
  a change to this paragraph in the same commit; a step on the site that
  would do any of these is the owner's decision, never the CLI's.

## Profiles and the session

Every command that reads the site takes `--profile <NAME>`
(`FLYWIRE_PROFILE`), which must be configured (`profiles add`); a
missing or unknown one is exit 2 listing the configured ones, and a lone
profile is never picked (`pk_cli_config::profile::resolve`). A session is the provider's
portal name, the cookie jar and the browser's user agent, in the vault entry
`<profile>/session`
(`pk_cli_auth::vault::Vault::for_binary("flywire")`: one keychain item,
`vault-key` in `piekstra.flywire`, and an encrypted file per entry
under `~/.config/flywire/vault/`). A `--browser` sign-in runs in the
profile's own kept Chrome profile (`Launch::profile_name`). `profiles
remove` removes the session and that browser profile before the profile;
`auth reset` is `Vault::clear`.

`auth login --provider <PROVIDER>` names the portal; it is kept with the
session, so later commands need only `--profile`. The sign-in is the
portal's own guest form: the account number from a statement, the
patient's date of birth and, for some providers, a ZIP code, the last four
of an SSN or a last name. The window `auth login --browser` opens is a
plain Chrome with `--remote-debugging-port=0`, so a session filling the
sign-in for the owner drives it through the port in `DevToolsActivePort` in
the profile directory (the `browser-explore` harness, `--port`). The
capture reads the portal host's jar (the session is `_simplee_store2`) and
confirms it with one `query payableStandingAmount` before storing it.

`client::Client` sends one request at a time, at least `client::PACE`
apart, follows no redirect, and maps answers to exit codes: a 401, a
redirect or the portal's signed-out GraphQL errors are 3 (`auth login
--browser --provider <PROVIDER> --profile <NAME> --overwrite`); a 403, 429
and 5xx are 5, and any other GraphQL error is 5 with its count, never its
text. So is an HTTP 400 `no source found`: the portal wants the header
`source: patient-client`, which `Client` and the session check
(`site::confirm_session`) send on every request (`client::SOURCE`;
docs/api.md, The session). A `Set-Cookie` updates the jar, written back
once the command has read what it needed, unless a newer sign-in replaced
the stored session meanwhile (`portal::Site::finish`). A failed write-back
is a warning.

## Tests are offline, always

`cargo test` reads no real keychain item or browser profile, stores no
session in the real vault, and contacts nothing beyond loopback. Every test
that runs a credential-reading command sets
`FLYWIRE_KEYCHAIN_SERVICE=piekstra.flywire.hermetic-tests`, a
service that holds nothing, with a temp `XDG_CONFIG_HOME`.

- `tests/cli_surface.rs`: every `--help`, exit codes, JSON errors, argument
  checks before a session is read, profile selection, `NO_COLOR`.
- `tests/site_e2e.rs`: the reads in-process against a loopback server, the
  session in an in-memory vault: the requests, write-back, the refusals,
  and each reader command's document and the export's FHIR on the
  fixtures in `tests/fixtures/graphql/`, with the signed-out, GraphQL-error
  and bad-amount paths.
- `tests/no_personal_data.rs`.

`make verify` = fmt-check, clippy, test, smoke.

`FLYWIRE_BASE_URL` points every read at a test's server instead of the
session's portal. It is honored only for a plain `http` origin on a
loopback address, and only over a test's own store: a
`FLYWIRE_KEYCHAIN_SERVICE` of `piekstra.flywire.<suffix>` and an
`XDG_CONFIG_HOME` away from `~/.config` (`session::is_test_store`), so no
session the owner stored can be sent to it; `auth login --browser` refuses
it. Under `FLYWIRE_KEYCHAIN_SERVICE` (`session::hermetic`) the override
is required: with no `FLYWIRE_BASE_URL`, every command that would send a
request is exit 2 before sending it (`client::base_from`), so a test run that
forgets the server never reaches the real portal.

## Dogfooding on synthetic data

`scripts/fixture-server.py` serves `tests/fixtures/graphql/` the way the
portal's GraphQL endpoint answers, by `operationName` (and the id the
variables name), with `--answer KEY=FILE` and `--status KEY=CODE` for the
error paths; its docstring has the steps. A dogfood run stores a vault key
under its test keychain service: `auth reset --force` under the same
variables removes it.

`make dogfood` (`scripts/dogfood.sh [BINARY]`, the installed `flywire` by
default) does all of it: it starts seven fixture servers (the fixtures; one
signed out on payments and with a GraphQL error on one bill; one answering
HTTP 500; one refusing the guarantor query; one failing it with HTTP 503;
one answering payables without the list; one answering one bill without
the bill),
drives every command and error path, checks exit codes and
values, prints one row per case and exits 1 if any failed. Its vault key,
under `piekstra.flywire.dogfood`, is created by the binary under test, so
it raises no keychain dialog, and is removed when it ends. It is not part
of `make verify`: it needs a keychain (or Secret Service) and free local
ports (8951 to 8957, `DOGFOOD_PORT` moves them).
