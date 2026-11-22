# The FHIR flywire writes

`records export --ndjson` writes `records-stream/v1` (medical-common's
`health-records/v1` profile) with `method: portal`: what the site shows,
mapped by hand to FHIR R4 in `src/bill.rs` and `src/readers.rs`. Every
resource carries `meta.source` `flywire:site` and a tag saying it was
mapped. Ids are stable across runs, derived from the site's own numbers, so
a records store merges the same bill pulled twice.

One sign-in is one patient at one provider. The stream's header names the
organization `org-<provider>` and the patient `patient-<provider>-<hash>`,
a hash of the provider, the flywire profile and the portal's id for the
accounts' guarantor (`payables.guarantor.externalId`, read by its own
query, the smallest if they name several). No number from the portal shows in it, and the header
carries no name. The id stays the same while accounts are added or drop
out and across sign-ins to the profile. It changes if the portal gives the
guarantor another id, or for another profile: a profile reused for
another person's accounts gets a new id rather than merging two people's
records. Accounts that name no guarantor, or a portal that refuses the
guarantor query, are exit 5, and a sign-in with no accounts exit 4, before
the stream starts. `--patient` and `--org` must
be those ids (`patients list` shows them), or the export is exit 4 before
it starts.

Once the header is out, an item the portal fails on (a bill's page, an
account's payments or statements, a statement's file) is left out with a
skip, `upstream` or `not_found`, naming the resource and the failure but
never the answer's text; the stream goes on. A session that ends mid-way
ends the stream with an incomplete trailer and exit 3.

| What the site shows | FHIR | Notes |
|---|---|---|
| The provider | `Organization` | `name` the payee's display name |
| The patient | `Patient` | `managingOrganization` the provider; no name, birth date or address |
| A billing account | `Account` | `identifier` the account number (system `urn:flywire:<org>:account`); `subject` the Patient; `owner` the provider; `status` `active`. R4's Account has no balance element: a balance is the sum of the open Invoices' `totalNet` |
| A service billed (a visit or claim at the provider) | `Invoice` | `identifier[]` the site's number for it (system `urn:flywire:<org>:bill`), the visit number (type text `visit`, system `urn:flywire:<org>:visit`) and the portal's claim number (type text `claim`, system `urn:flywire:<org>:claim`), where shown. `status` `issued` while the patient owes on it, `balanced` at 0, `cancelled` when voided. `type.text` the bill's name; `subject` the Patient; `issuer` the provider; `account` the Account |
| A charge on it | `Invoice.contained[]` `ChargeItem`, pointed at by `lineItem[].chargeItemReference` (`#charge-<n>`) | `occurrenceDateTime` the charge's date, else the bill's; `code` the CPT code (system `http://www.ama-assn.org/go/cpt`), else the revenue code (`https://www.nubc.org/CodeSystem/RevenueCodes`), else the description as text |
| A charge's amounts | `lineItem[].priceComponent[]`, each `amount` a `Money` in USD, code system `urn:pk-med:bill-component` | `base` `billed`; `informational` `allowed`; `deduction` `insurance-payment`; `discount` `insurance-adjustment`; `deduction` `patient-payment`; `informational` `patient-responsibility` (what the patient still owes). The portal itemizes only each charge's billed amount |
| The bill's totals | `totalGross`, `totalNet`, and `totalPriceComponent[]` for an itemized bill | billed, and what the patient still owes: the site's totals, else the sum of the lines'. `totalPriceComponent` carries the bill's whole split in the codes above: insurance paid, the insurance's and the patient's adjustments together, patient paid. A bill the site shows without lines is one line of its own (`chargeItemCodeableConcept` text `Services`) carrying that split, with its date of service in `note` |
| A payment made | `PaymentReconciliation` | `paymentDate`, `paymentAmount`; `disposition` how it was paid. Only settled payments: scheduled, cancelled and returned ones are left out, counted in one skip (`not_settled`). The portal does not say which bills a payment paid |
| A statement | `DocumentReference` (type text `bill`), then its `Binary` | `date` the statement date; the Binary's `data` the file, base64, `contentType` its format. A statement whose file is in no format flywire recognizes is left out (`verify_failed`) |

A records store keys a bill as `bill|<first identifier>|<earliest date of
service>`, and matches it to the insurer's claim by the insurer's claim
number where present, else by person, date of service, provider and billed
amount.

`--since <DAY>` keeps the bills with a service on or after it and the
payments and statements dated on or after it; the Organization, the
Patient and the Accounts are always written (`since_unbounded`).
