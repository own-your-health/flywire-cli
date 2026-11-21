//! The command tree (piekstra-cli/1, health-records/v1, documents/v1).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use pk_cli_auth::LoginArgs;
use pk_cli_browser::BrowserArgs;
use pk_cli_core::{CommonArgs, RangeArgs};
use pk_cli_selfupdate::SelfUpdateArgs;

pub const BIN: &str = "flywire";

/// A patient's medical bills from a Flywire patient portal, the billing site
/// many hospitals and health systems send their statements from
/// (`<provider>.simpleepay.com`): each account's balance, the visits billed
/// with their charges, what insurance paid and adjusted and what the patient
/// owes, payments made, and past statements. Read-only: it never pays or
/// changes anything. Unofficial.
#[derive(Parser, Debug)]
#[command(name = BIN, version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Override the config file location.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Read the portal as this profile: one configured with `flywire
    /// profiles add <NAME>` and signed in with `flywire auth login
    /// --browser --provider <PROVIDER> --profile <NAME>`. There is no
    /// default profile.
    #[arg(long, global = true, env = "FLYWIRE_PROFILE", value_name = "NAME")]
    pub profile: Option<String>,

    /// Whether --profile was given as a flag, not through FLYWIRE_PROFILE.
    #[arg(skip)]
    pub profile_flag: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// The portal sign-in, one per --profile.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// The logins: one profile per portal sign-in (profile-list/v1).
    #[command(subcommand)]
    Profiles(ProfilesCmd),
    /// The patient the sign-in is (patient-list/v1).
    #[command(subcommand)]
    Patients(PatientsCmd),
    /// The accounts the sign-in shows, with what is owed on each.
    #[command(subcommand)]
    Accounts(AccountsCmd),
    /// Bills: the visits billed on an account, with their charges, what
    /// insurance paid and adjusted, and what the patient owes.
    #[command(subcommand)]
    Bills(BillsCmd),
    /// Payments made on an account.
    #[command(subcommand)]
    Payments(PaymentsCmd),
    /// Bills, payments and statements as FHIR in records-stream/v1, for
    /// `medrec import --from flywire`.
    #[command(subcommand)]
    Records(RecordsCmd),
    /// Past statements (documents/v1).
    #[command(subcommand)]
    Documents(DocumentsCmd),
    /// One read of the profile's portal as --profile <NAME>, printed as it
    /// answers: `POST /graphql --data '{"query": "query ..."}'`, or `GET
    /// <PATH>`. Read-only: a mutation, a query that does more than read, and
    /// the sign-in, sign-out and payment paths are refused before a request
    /// is sent.
    Api(ApiArgs),
    /// Non-secret settings: the config file and the profiles in it.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Update to the latest release from GitHub.
    SelfUpdate(SelfUpdateArgs),
    /// Print a shell completion script.
    Completions { shell: Shell },
    /// Machine-readable capability discovery (cli-info/v1).
    Info,
}

/// `flywire api <METHOD> <PATH> [--data JSON]`: cli-common's `ApiArgs`
/// offers writes, and flywire only reads.
#[derive(Args, Debug, Clone)]
pub struct ApiArgs {
    /// POST for a GraphQL query, or GET (any case).
    #[arg(value_enum, ignore_case = true)]
    pub method: ApiMethod,
    /// A path on the profile's portal (leading /): /graphql for POST.
    pub path: String,
    /// For POST: the GraphQL request as JSON, {"query": "query ...",
    /// "variables": {...}, "operationName": "..."}. Only query operations
    /// are sent.
    #[arg(long)]
    pub data: Option<String>,
}

/// The methods `flywire api` sends: it reads only.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiMethod {
    #[value(name = "GET")]
    Get,
    #[value(name = "POST")]
    Post,
}

#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Sign in to --provider's portal as --profile <NAME> and keep the
    /// session in flywire's vault: encrypted files under
    /// ~/.config/flywire/vault/, sealed with a key held in the OS keychain
    /// (auth-login/v1).
    ///
    /// --browser opens a Chrome window on the portal's sign-in page in the
    /// profile's own browser profile. There the patient enters what the
    /// statement asks for: the account number, the patient's date of birth
    /// and, for some providers, a ZIP code or the last four of an SSN.
    /// --stdin reads the Cookie header of a request to the portal, copied
    /// from a signed-in browser's DevTools, with --user-agent from the same
    /// request. One read checks the session before it is stored, unless
    /// --no-verify.
    Login(LoginCmd),
    /// Whether the profile's session is stored and when it was captured
    /// (auth-status/v1). Sends no request.
    Status,
    /// Remove the profile's stored session, then the browser profile its
    /// --browser sign-in keeps (auth-logout/v1).
    Logout,
    /// Remove every profile's stored session from the vault and the vault's
    /// key from the keychain (auth-reset/v1): the way out when the key is
    /// lost or damaged. Asks first; without a terminal it needs --force
    /// (exit 6).
    Reset {
        /// Do not ask.
        #[arg(long)]
        force: bool,
    },
}

// The family's login flags with `--browser` beside them (DESIGN.md §2.2).
// A plain comment: a doc comment here would replace `auth login`'s about.
#[derive(Args, Debug)]
pub struct LoginCmd {
    /// The provider's portal: the part before .simpleepay.com in the
    /// address on the statement or email (`exampleclinic` for
    /// https://exampleclinic.simpleepay.com), or that address.
    #[arg(long, value_name = "PROVIDER")]
    pub provider: String,
    #[command(flatten)]
    pub login: LoginArgs,
    #[command(flatten)]
    pub browser: BrowserArgs,
    /// With --stdin or --from-env: the User-Agent of the browser the Cookie
    /// header was copied from, sent with the check and every later read.
    #[arg(long, value_name = "UA")]
    pub user_agent: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ProfilesCmd {
    /// The configured profiles, by name (profile-list/v1).
    #[command(visible_alias = "ls")]
    List,
    /// Add a profile: 1 to 32 lowercase letters, digits, `-` or `_`,
    /// starting with a letter or a digit (profile-add/v1).
    Add { name: String },
    /// Remove a profile with its stored session and the browser profile its
    /// --browser sign-in keeps (profile-remove/v1). Asks first; without a
    /// terminal it needs --force (exit 6).
    Remove {
        name: String,
        /// Do not ask.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print the config file path.
    Path,
    /// Show the effective configuration.
    Show,
}

#[derive(Subcommand, Debug)]
pub enum PatientsCmd {
    /// The patient the sign-in is, with its id for `records export
    /// --patient` (patient-list/v1).
    #[command(visible_alias = "ls")]
    List,
}

#[derive(Subcommand, Debug)]
pub enum AccountsCmd {
    /// Each account: its number, the provider, what is owed and how many
    /// bills (account-list/v1).
    #[command(visible_alias = "ls")]
    List,
}

#[derive(Args, Debug)]
pub struct AccountArg {
    /// Only this account: its id or number from `flywire accounts list`.
    #[arg(long, value_name = "ID")]
    pub account: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum BillsCmd {
    /// One row per bill, newest service first, open and closed (bill-list/v1).
    #[command(visible_alias = "ls")]
    List(BillsListArgs),
    /// One bill with its totals, the plans that covered it and its charges
    /// (bill/v1).
    Show(BillShowArgs),
}

#[derive(Args, Debug)]
pub struct BillsListArgs {
    #[command(flatten)]
    pub account: AccountArg,
    /// Only bills the patient still owes on.
    #[arg(long)]
    pub open: bool,
    #[command(flatten)]
    pub range: RangeArgs,
}

#[derive(Args, Debug)]
pub struct BillShowArgs {
    /// The bill's id from `flywire bills list`.
    pub id: String,
    #[command(flatten)]
    pub account: AccountArg,
}

#[derive(Subcommand, Debug)]
pub enum PaymentsCmd {
    /// Payments, newest first, with whether each settled (payment-list/v1).
    #[command(visible_alias = "ls")]
    List(PaymentsListArgs),
}

#[derive(Args, Debug)]
pub struct PaymentsListArgs {
    #[command(flatten)]
    pub account: AccountArg,
    #[command(flatten)]
    pub range: RangeArgs,
}

#[derive(Subcommand, Debug)]
pub enum RecordsCmd {
    /// The sign-in's bills, payments and statements as records-stream/v1.
    Export(ExportArgs),
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// The patient: the id from `flywire patients list`. A sign-in is one
    /// patient, so it may be omitted.
    #[arg(long, value_name = "ID")]
    pub patient: Option<String>,
    /// The provider: the organization id from `flywire patients list`. A
    /// sign-in reads one provider, so it may be omitted.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
    /// Only bills with a service on or after this day, and payments and
    /// statements dated on or after it (YYYY-MM-DD). The organization, the
    /// patient and the accounts are always included.
    #[arg(long, value_name = "YYYY-MM-DD")]
    pub since: Option<String>,
    /// Write the stream to stdout, which `medrec import --from flywire` reads. The
    /// stream is the output, so --json with it is a usage error.
    #[arg(long, conflicts_with = "output")]
    pub ndjson: bool,
    /// Write the stream to DIR/records.ndjson instead (tests and synthetic
    /// data only: real records go straight into medrec).
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum DocumentsCmd {
    /// Past statements, newest first (document-list/v1).
    #[command(visible_alias = "ls")]
    List(DocumentsListArgs),
    /// Save one statement, or every one with --all (document-download/v1,
    /// document-download-batch/v1).
    Download(DownloadArgs),
}

#[derive(Args, Debug)]
pub struct DocumentsListArgs {
    #[command(flatten)]
    pub account: AccountArg,
    #[command(flatten)]
    pub range: RangeArgs,
}

#[derive(Args, Debug)]
pub struct DownloadArgs {
    /// The document's id from `flywire documents list`.
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub id: Option<String>,
    /// Save every listed statement into the directory -o names.
    #[arg(long, requires = "output")]
    pub all: bool,
    /// Where to write: a file or a directory for one statement (default:
    /// its own name in the current directory), a directory for --all.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Replace a file that is already there.
    #[arg(long)]
    pub overwrite: bool,
    #[command(flatten)]
    pub account: AccountArg,
    #[command(flatten)]
    pub range: RangeArgs,
}
