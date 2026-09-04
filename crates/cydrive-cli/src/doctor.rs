//! The `doctor` subcommand (M5-2): one-shot offline diagnosis of the
//! local installation — config presence, metadata DB readability, cache
//! writability and the two listen ports — plus the platform/remote leg
//! (Windows WebClient registry + service state, Telegram connectivity).
//!
//! Design (see `docs/rust-rewrite-design.md`, «平台层与 CLI» —
//! "端口占用/注册表/WebClient/Telegram 连通性一键诊断"):
//!
//! * [`run_doctor`] is a pure function over an injected
//!   [`DoctorContext`] so tests run offline against temp paths and
//!   loopback sockets; the CLI builds the context from config discovery.
//! * Port occupancy is a **Warn**, not a Fail: the port may be held by
//!   the user's own running CyDrive instance, which is not a defect.
//! * The platform/remote checks ([`platform_checks`]) touch the real OS
//!   (registry, `sc query`) and the network; the only always-safe one
//!   here is the fixed Telegram advisory ([`telegram_connectivity_check`])
//!   — a real connectivity probe belongs to a live `cydrive run`, so
//!   doctor reports it as a Warn pointing there instead of dialing out
//!   (manual-run boundary, by design).

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use cydrive_core::credentials::{CredentialError, CredentialStore, BOT_TOKEN};
use cydrive_core::database::MetaDatabase;
use cydrive_platform::{BASIC_AUTH_LEVEL, FILE_SIZE_LIMIT_BYTES};

/// Verdict of one doctor check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// The check passed.
    Ok,
    /// Suspicious but possibly benign (e.g. a port held by the running
    /// instance itself, or a check this platform cannot perform).
    Warn,
    /// Broken and user-actionable.
    Fail,
}

/// One doctor check's outcome: a name to identify it, the verdict and a
/// human-readable detail (guidance on non-Ok verdicts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Short stable identifier (e.g. "config", "webdav port").
    pub name: String,
    /// The verdict.
    pub status: CheckStatus,
    /// What was observed and, when not [`CheckStatus::Ok`], what to do.
    pub detail: String,
}

/// Everything [`run_doctor`] needs, injected so tests stay offline: what
/// config discovery found and where the data paths / ports live, plus
/// the credential leg (resolved bot token + the store probed for
/// headless usability — see [`headless_credential_check`]).
pub struct DoctorContext {
    /// Whether a usable config file was discovered in the cwd.
    pub config_present: bool,
    /// Metadata DB path from the config (`None` = no config to read it
    /// from).
    pub db_path: Option<PathBuf>,
    /// Cache directory from the config (`None` = no config).
    pub cache_path: Option<PathBuf>,
    /// WebDAV listen port to probe.
    pub webdav_port: u16,
    /// Web dashboard listen port to probe.
    pub web_ui_port: u16,
    /// Bot token as config discovery resolved it (env > file > keyring);
    /// empty = no source carries one.
    pub bot_token: String,
    /// Credential store to probe (production: the OS keyring; tests
    /// inject fakes). Only ever read, never written.
    pub credential_store: Arc<dyn CredentialStore>,
}

/// Runs the offline checks: config, database, cache directory, both
/// ports. Platform/remote checks ([`platform_checks`]) are deliberately
/// not part of this list; the CLI merges them for display.
pub fn run_doctor(ctx: &DoctorContext) -> Vec<CheckResult> {
    vec![
        check_config(ctx.config_present),
        headless_credential_check(&ctx.bot_token, ctx.credential_store.as_ref()),
        check_database(ctx.db_path.as_ref()),
        check_cache(ctx.cache_path.as_ref()),
        check_port("webdav port", ctx.webdav_port),
        check_port("web ui port", ctx.web_ui_port),
    ]
}

/// Renders the results: one `[OK]/[WARN]/[FAIL] name — detail` line per
/// check, then a blank line and the `X ok, Y warn, Z fail` summary.
pub fn render_report(results: &[CheckResult]) -> String {
    use std::fmt::Write as _;
    let mut report = String::new();
    for result in results {
        let tag = match result.status {
            CheckStatus::Ok => "[OK]",
            CheckStatus::Warn => "[WARN]",
            CheckStatus::Fail => "[FAIL]",
        };
        let _ = writeln!(report, "{tag} {} — {}", result.name, result.detail);
    }
    let (ok, warn, fail) = summary_counts(results);
    let _ = writeln!(report, "\nSummary: {ok} ok, {warn} warn, {fail} fail");
    report
}

/// Counts the results per verdict, in `(ok, warn, fail)` order.
fn summary_counts(results: &[CheckResult]) -> (usize, usize, usize) {
    let mut ok = 0;
    let mut warn = 0;
    let mut fail = 0;
    for result in results {
        match result.status {
            CheckStatus::Ok => ok += 1,
            CheckStatus::Warn => warn += 1,
            CheckStatus::Fail => fail += 1,
        }
    }
    (ok, warn, fail)
}

/// Config check: a usable config in the cwd is Ok; none is a Fail with
/// the setup guidance.
fn check_config(present: bool) -> CheckResult {
    if present {
        CheckResult {
            name: "config".to_string(),
            status: CheckStatus::Ok,
            detail: "config file found in the current directory".to_string(),
        }
    } else {
        CheckResult {
            name: "config".to_string(),
            status: CheckStatus::Fail,
            detail: "no usable config in the current directory; run `cydrive setup` to \
                     create one (or `cydrive migrate` to import a Python config.json)"
                .to_string(),
        }
    }
}

/// Headless-credential check (service-lifecycle C6): in a terminal-less
/// deployment the OS keyring is typically unreachable (no user session
/// for the Secret Service / Credential Manager), so the bot token must
/// come from the config file or the environment. Verdicts:
///
/// * token resolved (file/env/keyring) → Ok — headless boot has its
///   credential whatever the keyring's state;
/// * token empty, store reachable (a `get` answers, key present or
///   absent) → Ok — `cydrive setup` can persist the token there;
/// * token empty, store unusable → Warn with the headless guidance:
///   write the token into `config.toml` or inject `CYDRIVE_BOT_TOKEN`
///   (the shipped systemd unit has an `EnvironmentFile` for exactly
///   this; see `deploy/cydrive.service`).
fn headless_credential_check(bot_token: &str, store: &dyn CredentialStore) -> CheckResult {
    let name = "credentials".to_string();
    if !bot_token.is_empty() {
        return CheckResult {
            name,
            status: CheckStatus::Ok,
            detail: "bot token resolved (file, env or credential store)".to_string(),
        };
    }
    // Probe with the token's own key: a reachable store answers Ok —
    // `None` when nothing is stored yet — while an unusable one errs
    // (keyring 3.x maps every non-NoEntry failure to `Unavailable`).
    match store.get(BOT_TOKEN) {
        Ok(_) => CheckResult {
            name,
            status: CheckStatus::Ok,
            detail: "no bot token yet; the OS credential store is reachable, so \
                     `cydrive setup` can persist one"
                .to_string(),
        },
        Err(error) => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!(
                "no bot token and the OS credential store is unavailable ({error}); in a \
                 headless environment (service/scheduled task) write the token into \
                 config.toml or inject CYDRIVE_BOT_TOKEN via the service's EnvironmentFile \
                 (see deploy/cydrive.service)"
            ),
        },
    }
}

/// A [`CredentialStore`] that answers every operation with
/// [`CredentialError::Unavailable`]: `doctor_cmd` wires it in when the
/// keyring probe (`KeyringStore::new`) itself fails, so
/// [`headless_credential_check`] observes the machine's real condition
/// instead of a healthy-looking in-memory fallback.
pub struct UnavailableKeyring {
    reason: String,
}

impl UnavailableKeyring {
    /// Captures the diagnosis from the failed keyring probe.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl CredentialStore for UnavailableKeyring {
    fn get(&self, _key: &str) -> Result<Option<String>, CredentialError> {
        Err(CredentialError::Unavailable(self.reason.clone()))
    }

    fn set(&self, _key: &str, _value: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(self.reason.clone()))
    }

    fn delete(&self, _key: &str) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable(self.reason.clone()))
    }
}

/// DB check: `None` (no config to read a path from) and a not-yet-created
/// file are both Warns (a fresh install simply has not run yet); an
/// existing path that opens and answers `get_stats` is Ok with the row
/// count; an existing path that fails to open is a Fail.
fn check_database(db_path: Option<&PathBuf>) -> CheckResult {
    let name = "database".to_string();
    let Some(path) = db_path else {
        return CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: "no db path to check (no usable config)".to_string(),
        };
    };
    if !path.exists() {
        return CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!(
                "{} not created yet; it appears on the first `cydrive run`",
                path.display()
            ),
        };
    }
    match MetaDatabase::open(path).and_then(|db| db.get_stats()) {
        Ok(stats) => CheckResult {
            name,
            status: CheckStatus::Ok,
            detail: format!(
                "{} readable: {} files, {} bytes total",
                path.display(),
                stats.total_files,
                stats.total_bytes
            ),
        },
        Err(error) => CheckResult {
            name,
            status: CheckStatus::Fail,
            detail: format!("{} exists but cannot be opened: {error}", path.display()),
        },
    }
}

/// Cache check: the directory must be creatable and writable — verified
/// by `create_dir_all` plus a write-and-delete probe file (a cache on a
/// read-only or full disk fails here, not mid-hydrate). `None` (no
/// config) is a Warn, mirroring the DB check.
fn check_cache(cache_path: Option<&PathBuf>) -> CheckResult {
    let name = "cache directory".to_string();
    let Some(path) = cache_path else {
        return CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: "no cache path to check (no usable config)".to_string(),
        };
    };
    let probe = path.join(".cydrive-doctor-probe");
    let probe_result = std::fs::create_dir_all(path)
        .and_then(|()| std::fs::write(&probe, b"probe"))
        .and_then(|()| std::fs::remove_file(&probe));
    match probe_result {
        Ok(()) => CheckResult {
            name,
            status: CheckStatus::Ok,
            detail: format!("{} exists and is writable", path.display()),
        },
        Err(error) => CheckResult {
            name,
            status: CheckStatus::Fail,
            detail: format!("{} is not writable: {error}", path.display()),
        },
    }
}

/// Port check: binding `127.0.0.1:port` succeeds → Ok "available". A
/// bind failure is a **Warn** — the most common cause is the user's own
/// running CyDrive instance holding the port, which is healthy.
fn check_port(name: &str, port: u16) -> CheckResult {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => CheckResult {
            name: name.to_string(),
            status: CheckStatus::Ok,
            detail: format!("127.0.0.1:{port} available"),
        },
        Err(error) => CheckResult {
            name: name.to_string(),
            status: CheckStatus::Warn,
            detail: format!("127.0.0.1:{port} in use or unavailable ({error})"),
        },
    }
}

/// Platform/remote checks the CLI appends to [`run_doctor`]'s list:
/// on Windows the WebClient registry values against the CyDrive contract
/// plus the service's running state, and always the Telegram advisory.
/// These touch the real OS, so they live outside the offline
/// [`run_doctor`].
pub fn platform_checks() -> Vec<CheckResult> {
    let mut results = Vec::new();
    // Attribute gating (not `if cfg!`): both calls are Windows-only items
    // (`webclient_service_check` has no non-Windows stub), so the block must
    // not even compile elsewhere — the `if cfg!(windows)` form used before
    // still type-checked the Windows symbols on unix.
    #[cfg(windows)]
    {
        results.push(evaluate_webclient_params(
            cydrive_platform::windows::read_webclient_params(),
        ));
        results.push(webclient_service_check());
    }
    results.push(telegram_connectivity_check());
    results
}

/// Pure verdict over the two WebClient registry values (the read-only leg
/// is `platform::windows::read_webclient_params`):
/// `None` (key/value missing or unreadable) → Warn with `fix-reg`
/// guidance; values off the contract
/// ([`FILE_SIZE_LIMIT_BYTES`], [`BASIC_AUTH_LEVEL`]) → Warn carrying the
/// current values; the contract pair → Ok.
pub fn evaluate_webclient_params(params: Option<(u32, u32)>) -> CheckResult {
    let name = "webclient registry".to_string();
    match params {
        None => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: "FileSizeLimitInBytes/BasicAuthLevel not readable or not set; run \
                     `cydrive fix-reg` in an elevated shell to tune them"
                .to_string(),
        },
        Some((limit, auth)) if limit == FILE_SIZE_LIMIT_BYTES && auth == BASIC_AUTH_LEVEL => {
            CheckResult {
                name,
                status: CheckStatus::Ok,
                detail: format!(
                    "FileSizeLimitInBytes={limit} and BasicAuthLevel={auth} match the CyDrive \
                     contract (4 GiB transfers, basic auth allowed)"
                ),
            }
        }
        Some((limit, auth)) => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!(
                "FileSizeLimitInBytes={limit} (want {FILE_SIZE_LIMIT_BYTES}), \
                 BasicAuthLevel={auth} (want {BASIC_AUTH_LEVEL}); run `cydrive fix-reg` in \
                 an elevated shell"
            ),
        },
    }
}

/// WebClient running state via `sc query webclient` (the same probe
/// `ensure_webclient_service` uses): RUNNING → Ok, anything else → Warn
/// (the service starts on demand with the first mount, so "stopped" is
/// not necessarily broken). Windows-only — compiled out elsewhere.
#[cfg(windows)]
fn webclient_service_check() -> CheckResult {
    let name = "webclient service".to_string();
    match std::process::Command::new("sc")
        .args(["query", "webclient"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            if text.contains("RUNNING") {
                CheckResult {
                    name,
                    status: CheckStatus::Ok,
                    detail: "WebClient service is running".to_string(),
                }
            } else {
                CheckResult {
                    name,
                    status: CheckStatus::Warn,
                    detail: "WebClient service is not running; it starts on demand with the \
                             first drive mount (`cydrive mount`)"
                        .to_string(),
                }
            }
        }
        Ok(output) => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!(
                "`sc query webclient` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        },
        Err(error) => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!("could not run `sc query webclient`: {error}"),
        },
    }
}

/// Telegram connectivity advisory: a real probe needs the configured
/// bot token and a live MTProto session — both belong to a running
/// `cydrive run`, not to an offline doctor pass. The check therefore
/// always reports this fixed Warn; verifying the connection is a manual
/// `cydrive run` (watch for the "connecting the Telegram transport"
/// step).
pub fn telegram_connectivity_check() -> CheckResult {
    CheckResult {
        name: "telegram connectivity".to_string(),
        status: CheckStatus::Warn,
        detail: "requires a live run; check via `cydrive run`".to_string(),
    }
}
