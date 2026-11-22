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
| A billing account | `Account` | `identifier` the account number (system `urn:flywire:<org>:account`); `subject` the Patient; `owner` the provider; `status` `active`. Written before the Invoices, so `Invoice.account` resolves in the stream. R4's Account has no balance element and flywire writes no balance: a balance is the sum of the open (`issued`) Invoices' `totalNet` |
| A service billed (a visit or claim at the provider) | `Invoice` | `identifier[]` the site's number for it first (system `urn:flywire:<org>:bill`), then the visit number (type text `visit`, system `urn:flywire:<org>:visit`) and the portal's claim number (type text `claim`, system `urn:flywire:<org>:claim`), where shown. `status` `issued` while the patient owes on it, `balanced` at 0, `cancelled` when voided. `type.text` the bill's name; `subject` the Patient; `issuer` the provider (`reference` and `display`); `account` the Account; `date` the posting or statement date where the site shows one, never the date of service |
| A charge on it | `Invoice.contained[]` `ChargeItem`, pointed at by `lineItem[].chargeItemReference` (`#charge-<n>`) | `occurrenceDateTime` the charge's date (`YYYY-MM-DD`), else the bill's; `code` the CPT code (system `http://www.ama-assn.org/go/cpt`), else the revenue code (`https://www.nubc.org/CodeSystem/RevenueCodes`), else the description as text; `performer[].actor.display` the rendering provider where the site names one. A bill the site shows without lines gets one ChargeItem of its own, code text the bill's name (else `Services`), holding its date of service: `occurrenceDateTime`, or `occurrencePeriod` when the service spans days |
| A charge's amounts | `lineItem[].priceComponent[]`, code system `urn:pk-med:bill-component` | `base` `billed`; `informational` `allowed`; `deduction` `insurance-payment`; `discount` `insurance-adjustment`; `deduction` `patient-payment`; `informational` `patient-responsibility` (what the patient still owes). The portal itemizes only each charge's billed amount; a bill without lines has its whole split on its one line |
| The bill's totals | `totalPriceComponent[]`, `totalNet`, `totalGross` | `totalPriceComponent` is the bill's own breakdown in the same codes, from the portal's totals where it shows them, else summed from the lines: the billed amount is its `base` `billed`, and insurance paid, the insurance's and the patient's adjustments together, and patient paid follow. `totalNet` is what the patient still owes. R4 puts both totals after the price components and `totalGross` adds tax, which a medical bill has none of, so `totalGross` equals `totalNet`; it is not the billed amount |
| A payment made | `PaymentReconciliation` | `status` `active`; `paymentDate`, `paymentAmount`; `disposition` how it was paid; `detail[]`, each `type` payment and `payee` the provider (`reference` and `display`). The model (`src/bill.rs`, shared with the other billing CLIs) writes one detail per bill a payment names that is an Invoice in the stream, with `request` that Invoice, and `amount` the whole payment when it names that one bill alone; a payment naming no such bill gets one detail with the payee only. The portal does not say which bills a payment paid, so flywire's export names none: every payment has one detail, the payee only, with no `request` or `amount`. Only settled payments: scheduled, cancelled and returned ones are left out, counted in one skip (`not_settled`) |
| A statement | `DocumentReference`, then its `Binary` | `type` `{"coding": [{"system": "urn:pk-med:doc-type", "code": "bill"}], "text": "bill"}`; `date` the statement date; `context.related` the Account, then the Invoices it lists where known; the Binary's `data` the file, base64, `contentType` its format. Amount due and due date have no R4 element, so none is written: a records store reads them from the file. A statement whose file is in no format flywire recognizes is left out (`verify_failed`) |
| A payment receipt | none | The portal keeps no receipt files, so flywire writes no receipt. The shared model types one as a statement is, code `receipt` (`Mapper::document` with `DocType::Receipt`) |

Every `Money` is `{"value": <number>, "currency": "USD"}`, the value a
JSON number in the decimal spelling a bill prints (`125.00`, `-12.30`):
never a string, which R4 forbids, and never rounded to `125.0`.

A records store keys a bill as `bill|<first identifier>|<earliest date of
service>` and a payment as `payment|<first identifier>|<paymentDate>`
(`id|...` where a payment has no number), and matches a bill to the
insurer's claim by the insurer's claim number where present, else by
person, date of service, provider and billed amount.

`--since <DAY>` keeps the bills with a service on or after it and the
payments and statements dated on or after it; the Organization, the
Patient and the Accounts are always written (`since_unbounded`).
