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
