# Flywire patient portals

Flywire (Flywire Healthcare Corporation) runs the patient billing portals
that many US hospitals and health systems send their statements and emails
from. It bought Simplee in 2020, and the portals are still Simplee's
platform: one per provider at `https://<provider>.simpleepay.com`. Emails
come from `email@hc.flywire.com` under the provider's name, with links to
the provider's portal.

Flywire publishes no patient API and no developer program for the portals.
The portal is a single-page app that reads and writes through one GraphQL
endpoint on its own host, so flywire sends the `query` documents the app
sends, with a signed-in patient's session.

Status: the signed-out pages and the app's JavaScript were read on
2026-11-20 without signing in. Everything marked *bundle* below comes from
the JavaScript only and has not yet been seen answered for a signed-in
patient; this file gets the observed answers when a sign-in is first made.

## Where the portals are

- `https://<provider>.simpleepay.com/app/...`: the app (React, Apollo
  GraphQL client, `basename` `/app`). Its routes are `/app/login`,
  `/app/login/payer`, `/app/account_lookup`, `/app/bills`,
  `/app/bills/:id`, `/app/payments/new` and `/app/bills/logout` (*bundle*).
- `POST https://<provider>.simpleepay.com/graphql`: every read and write.
  The app derives it from the page's address (everything before `/app/`,
  then `/graphql`) unless its settings name another.
- `<provider>` is one DNS label, the part before `.simpleepay.com` in the
  address a statement or email links to. flywire takes it as `auth login
  --provider <PROVIDER>` and keeps it with the session.
- `hc.flywire.com` only redirects; it holds no patient pages.
- Some Flywire healthcare clients are on a second platform, OnPlan
  (`<provider>.myonplanhealth.com`, Spring, hCaptcha on sign-in, statement
  PDFs at `/activity/statement/pdf/<id>`). flywire does not read OnPlan.

## Sign-in

A patient signs in as a guest: no user name or password.

1. `/app/login` asks for what the provider requires, from the statement:
   the account (or patient reference) number and the patient's date of
   birth and, for some providers, a ZIP code, the last four digits of an
   SSN or a last name. `query loginFields(provider, ...)` says which
   (*bundle*).
2. The form sends `mutation login(loginInput: { provider, acceptedTerms,
   dateOfBirth, payableIds, accountNumber, captchaKey, zip, ssn, lastName
   })` (*bundle*). `acceptedTerms` is the terms checkbox on the form;
   `captchaKey` is filled by the page.
3. Some providers offer an account lookup instead (`/app/account_lookup`:
   email or phone, date of birth, then a code), and some sign in from the
   hospital's own patient portal (MyChart, Cerner, a JWT link). flywire
   does none of these itself.

`auth login --browser --provider <PROVIDER>` opens Chrome on
`/app/login` in the profile's kept browser profile; the patient signs in
there. flywire never fills or submits the form.

## The session

- The session is the `HttpOnly` cookie `_simplee_store2` on the provider's
  host. The site sets cookies before anyone signs in, so a cookie alone
  does not show a session.
- `auth login` keeps the host's whole cookie jar and confirms it with one
  read: `query payableStandingAmount { payableStandingAmount }`, the
  account's amount due, which needs a signed-in patient (*bundle*: which
  query a visitor is refused has not been observed).
- The app treats two GraphQL errors as a session that has ended,
  `Unauthorized Request` and `Session Expired/Account Not Logged In` (the
  latter with ` SSO Redirect` for a portal signed into from a hospital's
  portal). flywire does too: exit 3, naming the sign-in.
- Its lifetime and whether the app refreshes it are not yet observed.
  `mutation verifyOrClearSession` is the app's own check, but it is a
  mutation and clears the session when it fails, so flywire never sends it.

## The reads (*bundle*)

| Query | What it returns |
|---|---|
| `payables(filterClosedBills)` | The accounts ("payables"): account number, standing amount, each bill's id, display name, admission date, amount due, status, claim number, pending insurance and discounts; any payment plan |
| `bill(id)` (operation `getSingleBill`) | One bill: visit number, account number, total billed, insurance amount and adjustments, patient payments and adjustments, the plans that covered it (payer, plan, member id), and each charge's date, description, amount, CPT code and revenue code |
| `previousPayments(payableIds)` | Payments made: amount, date, method (type and last four), status |
| `pastStatementsInfo(payableIds, accountNumber)` | Past statements: id, date, balance at the time |
| `pastStatementsContent(id)` | A statement's image or PDF (`imageFormat`, `preview`) |
| `insuranceOfAccount(accountNumber)` | The insurance on file for the account |
| `payableStandingAmount` | The amount due across the accounts |
| `providerDetails(provider)`, `publicSettings(provider)` | The provider's name, phone and portal settings, before sign-in |

## What each command reads (*bundle*)

The documents are the app's own, trimmed to these fields
(`src/queries.rs`). One request at a time, 750 ms apart.

| Command | Queries | Fields used |
|---|---|---|
| `patients list` | `payables(filterClosedBills: false)` | `guarantor.externalId` (hashed with the provider and the profile into the patient id, never printed), `payee.companyDisplayName` or `companyName` (the organization's name) |
| `accounts list` | `payables(filterClosedBills: false)` | `id`, `accountNumber` (else `accountId`), `standingAmount`, `payee`, and each bill's `amountDue` and `aPendingInsurance` |
| `bills list [--open] [--since] [--until] [--account]` | `payables(filterClosedBills: false)` | each bill's `id`, `displayName`, `admissionDate`, `amountDue`, `aPendingInsurance`, `status`, `claimNo`, `rawVisitType` |
| `bills show <ID>` | `payables`, then `bill(id)` (`getSingleBill`) | the bill's row, then `visitNumber`, `aTotalBilled`, `insuranceAmount`, `aTotalInsuranceAdjustments`, `aPatientPayments`, `aTotalPatientAdjustments`, `aPendingInsurance`, `aInsuranceResp`, `planCoverages { payer priority insuranceName insurancePlan }`, `charges { amount date description cptCode { code translation } revenueCode { code translation } }` |
| `payments list` | `payables`, then `previousPayments(payableIds: [<id>])` per account | `amount`, `date`, `status`, `paymentMethod { paymentType paymentSubType description }` |
| `documents list` | `payables`, then `pastStatementsInfo(payableIds: [<id>], accountNumber)` per account | `id`, `createdAt`, `balanceSnapshot`, `statementUniqueId`, `paperUploadedAt` |
| `documents download <ID>\|--all -o <DIR>` | as `documents list`, then `pastStatementsContent(id)` per statement | `imageFormat`, `preview` |
| `records export --ndjson` | all of the above: `getSingleBill` per bill, then payments, statements and each statement's file | as above |

How the answers are read (`src/reads.rs`):

- Amounts are whole cents: the app's money formatter divides by 100 by
  default (`convertToCents`). A JSON integer, or a number with no
  fraction, is cents. A fractional number or a string is exit 5 naming the
  field (`payables.bills.amountDue`), never with its value.
- Payments and adjustments are read as amounts taken off, whatever their
  sign.
- A date is its first ten characters, when they are a day.
- A missing or `null` field is absent from the output. An account, bill or
  statement without its `id` is left out.
- A numeric CPT or revenue code is padded to 5 or 4 digits.
- `bills list --open` is a bill with `amountDue` above 0. A bill is voided
  when its `status` contains `void` or `cancel`.
- A payment is settled unless its `status` says scheduled, cancelled,
  voided, failed, bounced, returned, charged back, declined, pending or
  refunded. `payments list` shows every payment with `settled`; `records
  export` writes only the settled ones and counts the rest in one skip.
- `documents list` lists only the statements the app shows: those with
  both `statementUniqueId` and `paperUploadedAt`.

### The statement file

`pastStatementsContent` answers `imageFormat` and `preview`. The app shows
the preview as an image or a PDF, but the values of `imageFormat` and the
encoding of `preview` are not in the JavaScript. flywire reads `preview` as
a data URL or as base64, and keeps the decoded bytes when they pass the
check of the format `imageFormat` (or the data URL's media type) names, or
else their magic bytes show a PDF, PNG, JPEG, TIFF or HEIC. Anything else
is written as the portal sent it, with a `.bin` name and a warning on
`documents download`; `records export` skips that statement
(`verify_failed`) rather than write a Binary it cannot type.

### Not yet observed

Each of these is the reading of the JavaScript alone, to check against the
first signed-in answers:

- the values of a bill's `status`, so how a voided bill shows;
- the sign of `insuranceAmount`, the adjustments and `aPatientPayments`;
- whether `insuranceAmount` is what the insurance paid;
- whether `claimNo` is the provider's claim number or the insurer's (it is
  written as the provider's: identifier type `claim`, not `insurer-claim`);
- the formats of `admissionDate`, a charge's `date`, a payment's `date` and
  `createdAt`;
- the raw values of a payment's `status` (the app's labels are Paid,
  Scheduled and Cancelled);
- whether amounts ever arrive as fractional numbers or strings;
- the values of `imageFormat` and the encoding of `preview`;
- whether a visitor is refused `payableStandingAmount` (the session check);
- whether a guest sign-in is answered `guarantor.externalId`, whether it
  stays the same across sign-ins, and whether `payables` lists one
  patient's accounts only (a guarantor responsible for two patients at a
  provider would put both under one patient id).

## Read-only

The GraphQL endpoint takes the portal's writes too: payments, payment
plans, stored cards and bank accounts, paperless and text-message
subscriptions, email and phone changes, insurance changes, and financing
offers through partners (Curae, CareCredit, Payzen). flywire sends none of
them:

- Every document is parsed before it is sent and must hold `query` and
  `fragment` definitions only. A `mutation` or `subscription` is exit 2.
- A query whose field or variable starts, at a camel-case boundary, with a
  word that does more than read (`validateCreditCard`, `verifyCode`,
  `setLanguage`, `checkCuraeTransactionStatus`) or names a financing
  partner is exit 2 too. GraphQL calls these queries; they validate a card
  with the processor, spend a code or call a partner.
- `flywire api GET` refuses `/graphql` and every path that names paying, a
  sign-in or sign-out, a subscription or a setting.

## Terms

Flywire's Payer Terms of Use (flywire.com/legal/terms-of-use) apply to the
portals ("the Platform"):

- Section 6.2.2: "Flywire grants you a limited, revocable,
  non-sublicensable license to access and use the Platform ... solely for
  your own personal use".
- Section 7: by using the Platform you agree not to "use any robot, spider,
  other automatic device, or manual process to monitor or copy our Platform
  without our prior written permission", nor to "use the Platform other
  than for the processing of payments to a Designated Entity".
- Section 10.2: what a patient reads there "may be privileged,
  confidential, and/or protected health information" and "is intended for
  the sole use of the patient or authorized party".

flywire reads only the signed-in patient's own account, one request at a
time, 750 ms apart, and never pays, changes a setting or sends a message.
Whoever runs it decides whether that is a use the terms allow.

## Traps

- The "Verify Email Address" email links to
  `/providers/<provider id>/subscriptions/go_paperless?activation_token=...`:
  following it enrolls the address in paperless billing. The unsubscribe
  link changes email settings too. flywire's `api` refuses both paths.
- The provider id in those links (`cn_<something>`) is not the portal's
  subdomain; `--provider` takes the subdomain.
- `/app/bills/logout` is a route of the app that signs out; a `GET` of it
  ends the session. `api` refuses it.
- Payment receipt emails show a transaction id, the amount, the card's
  last four and the remaining balance; they are not statements.
