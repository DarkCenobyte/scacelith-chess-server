//! Administration commands (`scacelith-server admin <command> ...`): accounts, sanctions,
//! integrity reviews, reports, rating refunds, engine analysis requests, backups and the bench
//! accounts of test servers. They run on the server host, directly against the database (no
//! network; WAL mode lets them run while the server is up), with the same commands, options,
//! outputs and exit codes as the former `scacelith-admin` (`bin/admin.js`); only the first line of
//! the help names the new command.
//!
//! Every moderator action is audited: a security event `moderator_action` {action, moderator,
//! ...}, a security log line `moderator.action`, and `reviewed_by` / `created_by` where the data
//! model has one. A command runs its reads and writes in one store job, so that a record it reads
//! and writes back (an integrity record) cannot be overwritten in between by the server, and a
//! refused or failed command writes nothing. The one exception is `integrity confirm`, whose ban
//! stands when its rating refunds fail (they run in a savepoint of their own).
//!
//! Exit codes: 0 done, 1 refused (`error: ...`) or failed (`admin: ...`), 2 usage.

mod accounts;
mod backup;
mod review;
mod rows;
mod text;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::clock::SharedClock;
use crate::config::{self, Config, ConfigError};
use crate::ids::UserId;
use crate::log::{self, Level, Logger};
use crate::store::{self, Db, NewSecurityEvent, ReportCounts, Signup, Store, StoreError, StoreOptions, User};
use crate::util::js;

use super::reports::{Received, recent_report_weight};

/// The help of the commands, printed by `admin --help` and for an unknown command.
pub const USAGE: &str = r#"Usage: scacelith-server admin <command> [options]

Accounts
  user show <name>                            account, ratings, sanctions, integrity summary (no account:
                                              the pending signup that holds the name)
  user ban <name> --hours N --reason TEXT     ban for anything but cheating, no refunds (applies at the
                                              next connection or game) [--revoke-sessions]
  user unban <name>                           lift the active bans
  user reset-mfa <name>                       disable TOTP, delete recovery codes, log out everywhere
  user verify-email <name>                    mark the e-mail address verified (no account: create the
                                              account of the pending signup that holds the name, as
                                              its link would)
  user revoke-sessions <name>                 log the account out everywhere

Integrity
  integrity list [--level suspected|high_confidence|confirmed] [--limit N]
  integrity show <name>                       evidence, per-game features, anomalies, reports
  integrity confirm <name> --reason TEXT [--hours N] [--keep-reports] [--refund-since DATE | --no-refund]
                                              level confirmed + ban (open cheating reports -> actioned) +
                                              rating refunds of the games since DATE (default:
                                              RATING_REFUND_DAYS before now) and of those recorded
                                              during the ban; --no-refund: none of them
  integrity clear <name> [--reason TEXT] [--dismiss-reports]

Rating refunds (the points the victims of a confirmed cheater lost to them, given back)
  refunds apply <name> [--since DATE]         refunds of a confirmed cheater's games since DATE (default:
                                              RATING_REFUND_DAYS before their latest ban for
                                              cheating); those already given are skipped
  refunds list [<name>] [--victim NAME] [--limit N]
                                              refunds of a cheater's games, received by a victim, or all

Engine analysis
  analysis queue <gameId>                     analyse the game before every other one, even one the
                                              queue left out (casual, short): a waiting job moves
                                              up, a failed one is tried again; a game being
                                              analysed or already analysed is left as it is

Reports and anomalies
  reports list [--limit N]                    open reports grouped by reported player, by priority
  reports resolve <id> actioned|dismissed
  anomalies <name> [--limit N]
  stats

Data
  backup <file> [--verify]                    consistent copy of the database (VACUUM INTO), safe while
                                              the server runs; the file must not exist; mode 600

Test servers only
  bench-accounts --count N [--prefix bench] --out FILE [--format tokens|tsv] --i-know-this-is-a-test-server

Common options: --json (machine-readable output), --by NAME (moderator name; default: OS user)
"#;

/// Options that never take a value.
const BOOLEAN_FLAGS: [&str; 8] = [
    "json",
    "help",
    "revoke-sessions",
    "keep-reports",
    "dismiss-reports",
    "i-know-this-is-a-test-server",
    "verify",
    "no-refund",
];

/// Milliseconds in an hour.
const HOUR_MS: i64 = 3_600_000;
/// Milliseconds in a day.
const DAY_MS: i64 = 86_400_000;

/// The value of an option: present without a value, or with one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Flag {
    On,
    Value(String),
}

impl Flag {
    /// JavaScript truthiness of the value (`--json=` is off).
    fn truthy(&self) -> bool {
        match self {
            Flag::On => true,
            Flag::Value(v) => !v.is_empty(),
        }
    }

    /// `String(value)`.
    fn text(&self) -> &str {
        match self {
            Flag::On => "true",
            Flag::Value(v) => v,
        }
    }
}

/// Command-line arguments: positionals and options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub positional: Vec<String>,
    pub flags: HashMap<String, Flag>,
}

/// Parses arguments: positionals, `--flag value`, `--flag=value`, boolean options (also an
/// option followed by another option or by nothing). A later option replaces an earlier one.
pub fn parse_args(argv: &[String]) -> Args {
    let mut args = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if let Some(rest) = a.strip_prefix("--") {
            if let Some(eq) = rest.find('=') {
                args.flags.insert(rest[..eq].to_string(), Flag::Value(rest[eq + 1..].to_string()));
            } else if BOOLEAN_FLAGS.contains(&rest) || i + 1 >= argv.len() || argv[i + 1].starts_with("--") {
                args.flags.insert(rest.to_string(), Flag::On);
            } else {
                args.flags.insert(rest.to_string(), Flag::Value(argv[i + 1].clone()));
                i += 1;
            }
        } else {
            args.positional.push(a.clone());
        }
        i += 1;
    }
    args
}

/// Why a command failed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Refused, with a message for the moderator (`error: ...`).
    Refused(String),
    /// Failed (`admin: ...`): a store or file error.
    Failed(String),
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Failure {
        Failure::Failed(e.to_string())
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Failure {
        Failure::Failed(e.to_string())
    }
}

/// A refusal.
fn refuse(msg: impl Into<String>) -> Failure {
    Failure::Refused(msg.into())
}

/// What a command answers: its `--json` data, its text, and the security log lines to write
/// (the audit trail, after the commit).
pub(crate) struct Output {
    data: Value,
    text: String,
    security: Vec<(&'static str, Value)>,
}

impl Output {
    fn new(data: Value, text: String) -> Output {
        Output { data, text, security: Vec::new() }
    }

    fn logged(mut self, security: Vec<(&'static str, Value)>) -> Output {
        self.security = security;
        self
    }
}

/// What the commands run with.
#[derive(Clone)]
pub struct AdminEnv {
    pub store: Store,
    pub config: Arc<Config>,
    /// The wall clock of the commands.
    pub clock: SharedClock,
    /// The moderator of the audit trail when `--by` is not given.
    pub moderator: String,
    /// Logger of the audit lines (`admin`).
    pub logger: Logger,
    /// Hash of a session token as the sessions table stores it.
    pub hash_token: fn(&str) -> String,
    /// Effective user id of the process (`None`: the system's).
    pub euid: Option<u32>,
}

impl AdminEnv {
    /// The environment of a store: the system clock, the session-token hash of the server, the
    /// moderator `moderator`.
    pub fn new(store: Store, config: Arc<Config>, moderator: String) -> AdminEnv {
        AdminEnv {
            store,
            config,
            clock: crate::clock::system(),
            moderator,
            logger: Logger::root().child("admin"),
            hash_token: crate::security::keys::sha256_hex,
            euid: None,
        }
    }
}

/// A command's context, cheap to clone into its store job.
#[derive(Clone)]
pub(crate) struct Ctx {
    store: Store,
    config: Arc<Config>,
    args: Arc<Args>,
    now: i64,
    moderator: String,
    /// The logger of the audit trail and of the refunds' own lines.
    logger: Logger,
    hash_token: fn(&str) -> String,
    euid: Option<u32>,
}

impl Ctx {
    /// A positional argument.
    fn positional(&self, i: usize) -> Option<&str> {
        self.args.positional.get(i).map(String::as_str)
    }

    fn flag(&self, name: &str) -> Option<&Flag> {
        self.args.flags.get(name)
    }

    /// Whether an option is given (and not empty).
    fn on(&self, name: &str) -> bool {
        self.flag(name).is_some_and(Flag::truthy)
    }

    /// An integer option, `default` when absent; at least `min`, at most `max`.
    fn int_flag(&self, name: &str, min: i64, max: i64, default: Option<i64>) -> Result<i64, Failure> {
        let v = match self.flag(name) {
            None | Some(Flag::On) => {
                return default.ok_or_else(|| refuse(format!("--{name} N is required")));
            }
            Some(Flag::Value(v)) => v,
        };
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(refuse(format!("--{name} expects an integer")));
        }
        let n: f64 = v.parse().unwrap_or(f64::INFINITY);
        if n < min as f64 || n > max as f64 {
            return Err(refuse(format!("--{name} must be between {min} and {max}")));
        }
        Ok(n as i64)
    }

    /// A text option, trimmed (empty when absent and not required); at most `max` characters.
    fn text_flag(&self, name: &str, required: bool, max: usize) -> Result<String, Failure> {
        let v = match self.flag(name) {
            Some(Flag::Value(v)) if !js::trim(v).is_empty() => js::trim(v),
            _ if required => return Err(refuse(format!("--{name} TEXT is required"))),
            _ => return Ok(String::new()),
        };
        if js::utf16_len(v) > max {
            return Err(refuse(format!("--{name} is limited to {max} characters")));
        }
        Ok(v.to_string())
    }

    /// A date option (`YYYY-MM-DD`, or an ISO 8601 time with its offset), not in the future.
    fn date_flag(&self, name: &str) -> Result<Option<i64>, Failure> {
        let Some(v) = self.flag(name) else { return Ok(None) };
        let Some(t) = text::parse_date(js::trim(v.text())) else {
            return Err(refuse(format!(
                "--{name} expects a date: YYYY-MM-DD (UTC) or an ISO 8601 time with its offset"
            )));
        };
        if t > self.now {
            return Err(refuse(format!("--{name} is in the future")));
        }
        Ok(Some(t))
    }

    /// Writes the audit trail of a moderator action (inside the command's job): a security
    /// event `moderator_action`. Returns the security log line to write after the commit.
    fn audit(
        &self,
        db: &Db<'_>,
        action: &str,
        user: Option<UserId>,
        detail: Vec<(&'static str, Value)>,
    ) -> Result<(&'static str, Value), StoreError> {
        let mut d = Map::new();
        d.insert("action".into(), Value::from(action));
        d.insert("moderator".into(), Value::from(self.moderator.as_str()));
        for (k, v) in detail {
            d.insert(k.into(), v);
        }
        db.security().insert_batch(&[NewSecurityEvent {
            kind: "moderator_action".into(),
            user_id: user,
            ip: None,
            at: Some(self.now),
            detail: Some(Value::Object(d.clone())),
        }])?;
        let mut fields = Map::new();
        fields.insert("userId".into(), user.map_or(Value::Null, Value::from));
        fields.extend(d);
        Ok(("moderator.action", Value::Object(fields)))
    }
}

/// The account named `name`.
fn require_user(db: &Db<'_>, name: Option<&str>) -> Result<User, Failure> {
    let Some(name) = name.filter(|n| !n.is_empty()) else { return Err(refuse("a user name is required")) };
    db.users().by_username(name)?.ok_or_else(|| refuse(format!("no user named \"{name}\"")))
}

/// The live pending signup that holds `name` when no account has it: with
/// REQUIRE_EMAIL_VERIFICATION a registration has no account until its link is used, and
/// `user show` / `user verify-email` reach the signup instead.
fn signup_instead(db: &Db<'_>, name: Option<&str>, now: i64) -> Result<Option<Signup>, StoreError> {
    let Some(name) = name.filter(|n| !n.is_empty()) else { return Ok(None) };
    if db.users().by_username(name)?.is_some() {
        return Ok(None);
    }
    Ok(db.signups().by_username(name)?.filter(|p| p.expires_at > now))
}

/// A player's integrity record as the commands read it (defaults for a player never scored).
#[derive(Clone, Debug)]
struct Record {
    level: store::IntegrityLevel,
    score: f64,
    evidence: Map<String, Value>,
    updated_at: i64,
    reviewed_by: Option<String>,
}

impl Default for Record {
    fn default() -> Record {
        Record {
            level: store::IntegrityLevel::None,
            score: 0.0,
            evidence: Map::new(),
            updated_at: 0,
            reviewed_by: None,
        }
    }
}

impl Record {
    /// The record, a store error propagated.
    fn read(db: &Db<'_>, user: UserId) -> Result<Record, StoreError> {
        let r = db.integrity().get(user)?;
        let evidence = match r.evidence.as_ref().map(rows::structured) {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let score = if r.score.is_nan() { 0.0 } else { r.score };
        Ok(Record { level: r.level, score, evidence, updated_at: r.updated_at, reviewed_by: r.reviewed_by })
    }

    /// The record, a store error read as no record.
    fn read_or_default(db: &Db<'_>, user: UserId) -> Record {
        Record::read(db, user).unwrap_or_default()
    }

    fn priority(&self, report_weight: f64) -> i64 {
        super::reports::review_priority(
            super::players::from_store_level(self.level),
            self.score,
            report_weight,
        )
    }
}

/// Summed weight of the reports a player received in the last 30 days, every report counted
/// (the newest 200 when the sum cannot be read).
fn report_weight_30d(db: &Db<'_>, user: UserId, now: i64) -> f64 {
    match db.reports().weight_since(user, now - 30 * DAY_MS, 0.0) {
        Ok(w) => js::round(w.total * 1000.0) / 1000.0,
        Err(_) => {
            let received: Vec<Received> = db
                .reports()
                .for_reported(user, 200)
                .unwrap_or_default()
                .iter()
                .map(|r| Received { weight: r.weight, at: r.created_at })
                .collect();
            recent_report_weight(&received, now, 30)
        }
    }
}

/// Reports a player received and those still open.
fn report_counts(db: &Db<'_>, user: UserId) -> ReportCounts {
    db.reports().count_for(user).unwrap_or_else(|_| {
        let received = db.reports().for_reported(user, 200).unwrap_or_default();
        let open = received.iter().filter(|r| r.outcome().is_none()).count();
        ReportCounts { total: received.len() as i64, open: open as i64 }
    })
}

/// Runs one command (`argv` follows `admin`); writes its answer to `out`, a refusal or an error to
/// `err`. Returns the exit code: 0 done, 1 refused or failed, 2 usage.
pub async fn run_admin(argv: &[String], env: &AdminEnv, out: &mut String, err: &mut String) -> i32 {
    let args = parse_args(argv);
    let p = |i: usize| args.positional.get(i).map(String::as_str);
    let command = Command::of(p(0), p(1));
    let Some(command) = command.filter(|_| !args.flags.get("help").is_some_and(Flag::truthy)) else {
        if command.is_some() {
            out.push_str(USAGE);
            return 0;
        }
        err.push_str(USAGE);
        return 2;
    };
    let moderator = match args.flags.get("by") {
        Some(Flag::Value(by)) if !js::trim(by).is_empty() => text::slice(js::trim(by), 64).to_string(),
        _ => env.moderator.clone(),
    };
    let json = args.flags.get("json").is_some_and(Flag::truthy);
    let ctx = Ctx {
        store: env.store.clone(),
        config: env.config.clone(),
        args: Arc::new(args),
        now: env.clock.wall_ms(),
        moderator,
        logger: env.logger.clone(),
        hash_token: env.hash_token,
        euid: env.euid,
    };
    match command.run(&ctx).await {
        Ok(output) => {
            for (msg, fields) in output.security {
                env.logger.emit(Level::Security, msg, Some(fields));
            }
            if json {
                out.push_str(&crate::util::json::to_string_pretty(&output.data));
                out.push('\n');
            } else {
                out.push_str(&output.text);
            }
            0
        }
        Err(Failure::Refused(msg)) => {
            err.push_str(&format!("error: {msg}\n"));
            1
        }
        Err(Failure::Failed(msg)) => {
            err.push_str(&format!("admin: {msg}\n"));
            1
        }
    }
}

/// The commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    UserShow,
    UserBan,
    UserUnban,
    UserResetMfa,
    UserVerifyEmail,
    UserRevokeSessions,
    IntegrityList,
    IntegrityShow,
    IntegrityConfirm,
    IntegrityClear,
    ReportsList,
    ReportsResolve,
    RefundsApply,
    RefundsList,
    AnalysisQueue,
    Anomalies,
    Stats,
    BenchAccounts,
    Backup,
}

impl Command {
    /// The command of the first two positionals (`<group> <name>`, else `<name>`).
    fn of(a: Option<&str>, b: Option<&str>) -> Option<Command> {
        let pair = match (a?, b.unwrap_or("")) {
            ("user", "show") => Some(Command::UserShow),
            ("user", "ban") => Some(Command::UserBan),
            ("user", "unban") => Some(Command::UserUnban),
            ("user", "reset-mfa") => Some(Command::UserResetMfa),
            ("user", "verify-email") => Some(Command::UserVerifyEmail),
            ("user", "revoke-sessions") => Some(Command::UserRevokeSessions),
            ("integrity", "list") => Some(Command::IntegrityList),
            ("integrity", "show") => Some(Command::IntegrityShow),
            ("integrity", "confirm") => Some(Command::IntegrityConfirm),
            ("integrity", "clear") => Some(Command::IntegrityClear),
            ("reports", "list") => Some(Command::ReportsList),
            ("reports", "resolve") => Some(Command::ReportsResolve),
            ("refunds", "apply") => Some(Command::RefundsApply),
            ("refunds", "list") => Some(Command::RefundsList),
            ("analysis", "queue") => Some(Command::AnalysisQueue),
            _ => None,
        };
        pair.or(match a? {
            "anomalies" => Some(Command::Anomalies),
            "stats" => Some(Command::Stats),
            "bench-accounts" => Some(Command::BenchAccounts),
            "backup" => Some(Command::Backup),
            _ => None,
        })
    }

    async fn run(self, ctx: &Ctx) -> Result<Output, Failure> {
        match self {
            Command::UserShow => accounts::user_show(ctx).await,
            Command::UserBan => accounts::user_ban(ctx).await,
            Command::UserUnban => accounts::user_unban(ctx).await,
            Command::UserResetMfa => accounts::user_reset_mfa(ctx).await,
            Command::UserVerifyEmail => accounts::user_verify_email(ctx).await,
            Command::UserRevokeSessions => accounts::user_revoke_sessions(ctx).await,
            Command::IntegrityList => review::integrity_list(ctx).await,
            Command::IntegrityShow => review::integrity_show(ctx).await,
            Command::IntegrityConfirm => review::integrity_confirm(ctx).await,
            Command::IntegrityClear => review::integrity_clear(ctx).await,
            Command::ReportsList => review::reports_list(ctx).await,
            Command::ReportsResolve => review::reports_resolve(ctx).await,
            Command::RefundsApply => review::refunds_apply(ctx).await,
            Command::RefundsList => review::refunds_list(ctx).await,
            Command::AnalysisQueue => review::analysis_queue(ctx).await,
            Command::Anomalies => review::anomalies(ctx).await,
            Command::Stats => review::stats(ctx).await,
            Command::BenchAccounts => accounts::bench_accounts(ctx).await,
            Command::Backup => backup::backup(ctx).await,
        }
    }
}

/// The moderator of the audit trail: `SCACELITH_MODERATOR`, else `SUDO_USER`, else the user
/// running the command, else `admin`.
fn moderator_name() -> String {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    env("SCACELITH_MODERATOR")
        .or_else(|| env("SUDO_USER"))
        .or_else(|| current_euid().and_then(user_name))
        .unwrap_or_else(|| "admin".into())
}

/// The effective user id of the process: the owner of its `/proc/self`.
fn current_euid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// The name of a user id in `/etc/passwd`.
fn user_name(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut f = line.split(':');
        let name = f.next()?;
        let id: u32 = f.nth(1)?.parse().ok()?;
        (id == uid && !name.is_empty()).then(|| name.to_string())
    })
}

/// `scacelith-server admin ...`: loads the configuration (after the help), logs warnings and
/// errors readably on stderr, opens the server's existing database and runs one command on a
/// runtime of its own. Returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    run_process(args, config::load_process, true, &mut out, &mut err)
}

/// [`main`] with its configuration loader and outputs (tests); `init_logging` sets up the
/// process's logging.
pub(crate) fn run_process(
    args: &[String],
    load: impl FnOnce() -> Result<Config, ConfigError>,
    init_logging: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if args.is_empty() || args[0] == "--help" || args[0] == "help" {
        let _ = out.write_all(USAGE.as_bytes());
        return if args.is_empty() { 2 } else { 0 };
    }
    let config = match load() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return 1;
        }
    };
    if init_logging {
        let ip_mode = log::Options::from_config(&config);
        log::init(log::Options {
            ip_mode: ip_mode.ip_mode,
            ip_secret: ip_mode.ip_secret,
            ..log::Options::admin()
        });
    }
    // Every command works on the server's existing database: opening a missing file would create
    // an empty one (a wrong DB_PATH, or a relative DATA_DIR run from another directory).
    let memory = std::path::Path::new(&config.db_path).file_name().is_some_and(|n| n == ":memory:");
    if !memory && !std::path::Path::new(&config.db_path).exists() {
        let _ = writeln!(
            err,
            "error: no Scacelith database at {}; check DB_PATH and DATA_DIR (a relative DATA_DIR is resolved from the \
             current directory)",
            config.db_path
        );
        return 1;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(err, "admin: {e}");
            return 1;
        }
    };
    let (code, stdout, stderr) = runtime.block_on(async {
        let (mut stdout, mut stderr) = (String::new(), String::new());
        let store = match Store::open(&config, StoreOptions::default()).await {
            Ok(s) => s,
            Err(e) => return (1, stdout, format!("admin: {e}\n")),
        };
        let env = AdminEnv::new(store.clone(), Arc::new(config), moderator_name());
        let code = run_admin(args, &env, &mut stdout, &mut stderr).await;
        store.close().await;
        (code, stdout, stderr)
    });
    let _ = out.write_all(stdout.as_bytes());
    let _ = err.write_all(stderr.as_bytes());
    code
}
