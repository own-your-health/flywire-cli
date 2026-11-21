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
