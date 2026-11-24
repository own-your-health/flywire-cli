# Security

Report a vulnerability privately through GitHub's "Report a vulnerability"
(Security tab), not in an issue.

## What flywire touches

- Contacts no server except GitHub, for `self-update`, and the profile's
  portal, `https://<provider>.simpleepay.com`: for `auth login`, a Chrome
  window the patient signs in through and one read that checks the session
  before it is stored; for a command run with `--profile`, the patient's own
  reads. Every request is a GraphQL `query` to `/graphql` or a `GET`, one at
  a time and spaced apart, with a 30-second limit and no redirect followed.
  Every URL is built from that origin; the provider is one DNS label under
  simpleepay.com, checked when it is given. `FLYWIRE_BASE_URL` replaces it for tests only: it is
  refused unless it is a plain `http` origin on 127.0.0.1, localhost or
  `[::1]`, `FLYWIRE_KEYCHAIN_SERVICE` names a test service
  (`piekstra.flywire.<suffix>`) and `XDG_CONFIG_HOME` is set away from
  `~/.config`, so a session the owner stored is never sent anywhere but its
  portal. While `FLYWIRE_KEYCHAIN_SERVICE` is set, `FLYWIRE_BASE_URL` is
  required: without a loopback base, every command that would send a request
  exits 2 before sending anything. `flywire api` is held to the same: a GraphQL document of queries
  only, or a `GET` of a path on the portal, refused before anything is sent
  when it is a mutation, a query that does more than read, or a path that
  names a payment, a sign-in or sign-out, a subscription or a setting.
- A cookie the portal changes is written back to the profile's session once
  the command has read what it needed, unless a newer sign-in replaced the
  stored session meanwhile. A failed write-back is a warning on stderr.
- Stores one credential per profile, its portal session: the provider's
  portal name, the `Cookie` header, the browser's user agent and the
  capture time, as the vault entry
  `<profile>/session`. The vault (`pk_cli_auth::vault`) is encrypted
  owner-only files under `~/.config/flywire/vault/`, with one key in the
  keychain (service `piekstra.flywire`, account `vault-key`). A session
  never goes on a command line or into a log.
- Keeps one Chrome profile per flywire profile for `auth login --browser`:
  `~/Library/Application Support/piekstra.flywire/browser-profile-<profile>`
  (under `$XDG_DATA_HOME` on Linux), owner-only (`0700`; a launch is refused
  otherwise). `--fresh-profile` signs in without it.
- `auth logout --profile <NAME>` and `profiles remove <NAME>` remove that
  profile's session, then its Chrome profile. `auth reset` removes every
  session and the vault key.
- The config file holds the profile names.

## What flywire never does

- Writes to the portal: no mutation of any kind, so no payment, payment
  method, payment plan, paperless, text-message or contact setting,
  insurance change, financing offer or sign-out request; and no query that
  validates a card, verifies a code or calls a financing partner.
- Signs in by itself: the account number, date of birth and any other
  detail are typed (or filled) on the portal's own page, never passed to
  flywire.
- Sends bills anywhere: the output goes to stdout or a file you name.
