# flywire

Unofficial; not affiliated with or endorsed by Flywire, Simplee or any
provider whose portal it reads.

`flywire` is a read-only command line for a patient's own medical bills on
a Flywire patient portal, the billing site many US hospitals and health
systems send their statements from (`https://<provider>.simpleepay.com`,
emails from `email@hc.flywire.com`). It signs in as you, reads what the
portal shows you, and prints it or writes it as FHIR R4 for a personal
medical-records store (`records-stream/v1`, the `health-records/v1` profile
of [medical-common](https://github.com/own-your-health/medical-common), read by
`medrec import --from flywire`). It never pays, schedules or cancels a
payment, sets up a payment plan, stores a card, changes a setting, or
sends a message.

Status: sign-in, `api` and the bill commands (`patients`, `accounts`,
`bills`, `payments`, `documents`, `records export`) work against synthetic
answers shaped from the portal app's own JavaScript. They have not yet read
a signed-in portal; `docs/api.md` lists what the first sign-in has to
confirm.

## Who it is for

Anyone whose hospital statement or billing email links to an address on
`simpleepay.com`. Its first part is the provider's portal name: for
`https://exampleclinic.simpleepay.com/app/login` it is `exampleclinic`.

No developer registration is needed. Flywire publishes no patient API and
no developer program, so there is no app to register and no key to
request: `flywire` sends the same queries the portal's own page sends once
you are signed in (`docs/api.md`).

Portals on Flywire's other platform, OnPlan (`myonplanhealth.com`), are not
read.

## Install

```sh
cargo install --git https://github.com/own-your-health/flywire-cli --locked
# or download a release, flywire-<target>.tar.gz (macOS Apple Silicon or
# Intel, Linux x86_64), put `flywire` on your PATH; later: flywire self-update
```

It needs Google Chrome for the sign-in (`FLYWIRE_CHROME` names another
Chromium-based browser), and the macOS keychain or, on Linux, a Secret
Service keyring such as GNOME Keyring for the stored session.

## Quick start

```sh
flywire profiles add me                       # one profile per patient account
flywire auth login --browser --provider exampleclinic --profile me
flywire auth status --profile me
flywire accounts list --profile me
flywire bills list --open --profile me
flywire bills show <BILL-ID> --profile me
flywire documents download --all -o statements --profile me
flywire patients list --profile me             # the patient id medrec takes
medrec --profile <FOLDER> import --from flywire --from-profile me --patient <ID>
```

Every command that reads the portal takes `--profile <NAME>`
(`FLYWIRE_PROFILE`); there is no default. `--json` on every command prints
one schema-tagged document.

## What needs a person

`auth login --browser --provider <PROVIDER>` opens Chrome on the portal's
sign-in page, in a browser profile kept for that `flywire` profile, and
waits up to 10 minutes. On the portal's own page you enter what your
statement asks for:

1. the account number (some providers call it the patient reference
   number);
2. the patient's date of birth;
3. for some providers, a ZIP code, the last four digits of an SSN or a
   last name;

then tick the terms box and sign in. `flywire` never sees what you type.
Once the portal shows you signed in, it reads the session's cookies,
checks them with one read, stores them encrypted (`SECURITY.md`) and closes
the window.

How long a sign-in lasts has not been measured yet. When the portal has
ended it, a read exits 3 and prints the command that signs in again:
`flywire auth login --browser --provider <PROVIDER> --profile <NAME> --overwrite`.

## Commands

| Command | Output |
|---|---|
| `profiles list\|add\|remove` | `profile-list/v1` |
| `auth login --browser --provider <P>`, `auth status\|logout\|reset` | `auth-login/v1`, `auth-status/v1`, … |
| `patients list` | `patient-list/v1`: the patient the sign-in is, with the id `records export --patient` takes |
| `accounts list` | `account-list/v1`: each account's number, provider, what is owed, bills and open bills |
| `bills list [--open] [--account <ID>] [--since\|--until <DAY>] [--limit <N>]` | `bill-list/v1`: newest service first, open and closed |
| `bills show <ID>` | `bill/v1`: the bill's totals (billed, insurance paid and adjusted, patient paid, owed), the plans that covered it and its charges with their CPT or revenue codes |
| `payments list [--account <ID>] [--since\|--until <DAY>]` | `payment-list/v1`: amount, date, method, status and whether it settled |
| `documents list` | `document-list/v1`: past statements with a file |
| `documents download <ID>\|--all -o <DIR>` | `document-download/v1`, `document-download-batch/v1`: the statement's file, owner-only; `-o -` writes it to stdout |
| `records export (--ndjson\|-o <DIR>) [--patient <ID>] [--org <ORG>] [--since <DAY>]` | `records-stream/v1`: FHIR R4 Invoices with their charges, payments and statements (`docs/fhir.md`); `-o` writes `DIR/records.ndjson`, against a test's server only |
| `api POST /graphql --data '{"query": "query ..."}'` | the portal's answer, as it came; queries only |
| `api GET <PATH>` | a path on the portal, as it came |
| `info` | `cli-info/v1` |

Exit codes: 0 ok, 2 usage (a mutation or a refused query included), 3
signed out (sign in again), 4 not found, 5 the portal or the network
failed, 6 confirmation needed (`--force`).

## Flywire's terms and automated access

Flywire's Payer Terms of Use cover the portals. They license their use
"solely for your own personal use" (section 6.2.2), and in section 7 you
agree not to "use any robot, spider, other automatic device, or manual
process to monitor or copy our Platform without our prior written
permission", nor to "use the Platform other than for the processing of
payments to a Designated Entity". `flywire` reads only what your own
sign-in shows you, one request at a time and spaced apart, and never
writes anything; you sign in yourself, in Chrome. Whether that suits you
under those terms is your call; Flywire could change the portal at any
time.

## Privacy

Bills stay on your machine: `flywire` prints them or pipes them to your own
records store, and contacts no server but your provider's portal (and
GitHub for `self-update`). The repository holds no personal data; its
fixtures are synthetic.

## License

Copyright (C) 2026 piekstra. Licensed under the GNU Affero General Public License, version 3 only (AGPL-3.0-only); see LICENSE.
