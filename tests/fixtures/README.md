# Fixtures

Everything here is synthetic. No file is a capture of a real portal's
answer: each is shaped from the Flywire portal app's JavaScript (the
queries' fields and nesting, `docs/api.md`), with dummy values in them
(`exampleclinic`, `ACCT-0000001`, `BILL-0000001`, round amounts, days in
January 2026).

`graphql/` holds the answers to the readers' GraphQL queries, one file per
operation: `<operationName>.json`, or `<operationName>.<id>.json` for the
answer to one id (`id`, `statementId` or the first of `payableIds` in the
variables). `signedOut.json`, `error.json` and `noAnswer.json` (the list or
the bill `null`, with no error) are the error answers the tests and `make
dogfood` put in place of a fixture.

Where the JavaScript does not say what a value looks like (a bill's
`status`, a payment's `status`, `imageFormat`), the fixtures use a
plausible value; `docs/api.md` lists each of them under "Not yet observed".
When a signed-in answer is first seen, its shape (never its values) is
what these files should be checked against.
