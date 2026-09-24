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

use cloudkit_core::credentials::{CredentialError, CredentialStore, BOT_TOKEN};
use cloudkit_core::database::MetaDatabase;
use cloudkit_platform::{BASIC_AUTH_LEVEL, FILE_SIZE_LIMIT_BYTES};

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
    database_check("database", db_path)
}

/// [`check_database`] with the check name injected — the multi-volume
/// doctor reports one db check per volume (`"volume <name>: database"`)
/// over the same verdict semantics.
fn database_check(name: &str, db_path: Option<&PathBuf>) -> CheckResult {
    let name = name.to_string();
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
    let mut results = webclient_checks();
    results.push(telegram_connectivity_check());
    results
}

/// The OS platform leg shared by every backend (WebClient registry +
/// service state on Windows; empty elsewhere) — the doctor command
/// composes it with the configured backend's own advisory
/// ([`telegram_connectivity_check`] / [`backend_checks`] +
/// [`baidu_connectivity_check`]), replacing [`platform_checks`]' fixed
/// telegram tail on non-telegram instances.
pub fn webclient_checks() -> Vec<CheckResult> {
    // Windows-only 填充（下方 cfg 块）编译掉时 mut 即 unused——平台差异
    // 经 cfg_attr 吸收（两平台 clippy -D warnings 同时干净）。
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut results = Vec::new();
    // Attribute gating (not `if cfg!`): both calls are Windows-only items
    // (`webclient_service_check` has no non-Windows stub), so the block must
    // not even compile elsewhere — the `if cfg!(windows)` form used before
    // still type-checked the Windows symbols on unix.
    #[cfg(windows)]
    {
        results.push(evaluate_webclient_params(
            cloudkit_platform::windows::read_webclient_params(),
        ));
        results.push(webclient_service_check());
    }
    results
}

/// The WinFsp installation checks (Phase 3 / WF4, K40): a **Warn** — never
/// a Fail — when WinFsp is absent, because the default `mount_backend`
/// is now `"winfsp"` and an unavailable runtime means drive letters do
/// not mount (no net use fallback) — the check surfaces that before the
/// boot does. The detection is a pure
/// registry + file read ([`cloudkit_platform::windows::winfsp_install`]),
/// so it needs no `winfsp` feature, no DLL load and no SDK — and it is
/// exactly the information the operator needs *before* setting
/// `mount_backend = "winfsp"`. Empty off Windows (the probe cannot
/// mean anything there), like [`webclient_checks`].
pub fn winfsp_checks() -> Vec<CheckResult> {
    // Attribute gating (not `if cfg!`), the `webclient_checks` precedent:
    // on unix the Windows-only symbols must not even compile. Each arm is
    // a whole expression, which also keeps clippy's `vec_init_then_push`
    // quiet on Windows (one check per platform shape, written literally).
    #[cfg(windows)]
    {
        vec![evaluate_winfsp_install(
            cloudkit_platform::windows::winfsp_install().as_ref(),
        )]
    }
    #[cfg(not(windows))]
    Vec::new()
}

/// Pure verdict over the WinFsp install probe (the read-only leg is
/// `platform::windows::winfsp_install`):
///
/// * `None` — not installed (or the registry is unreadable) → **Warn**
///   with the install pointer *and* the reassurance that the WebDAV
///   backend needs nothing (the default stays workable);
/// * `Some` with the runtime DLL → Ok, naming the install directory and
///   what a winfsp mount additionally needs (a `--features winfsp`
///   build);
/// * `Some` without the DLL — a half install → Warn (the winfsp mount
///   would fail at the preload; the operator should reinstall).
pub fn evaluate_winfsp_install(install: Option<&cloudkit_platform::WinFspInstall>) -> CheckResult {
    let name = "winfsp".to_string();
    match install {
        None => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: "WinFsp is not installed (or its `InstallDir` registry value is \
                     unreadable); install it from https://winfsp.dev/install/ to use \
                     `mount_backend = \"winfsp\"` — the default `mount_backend = \"webdav\"` \
                     needs nothing installed"
                .to_string(),
        },
        Some(install) if install.dll.is_none() => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!(
                "{} exists but {} is missing — the installation looks incomplete; reinstall \
                 from https://winfsp.dev/install/ (the WebDAV backend keeps working \
                 meanwhile)",
                install.install_dir.display(),
                cloudkit_platform::WINFSP_X64_DLL,
            ),
        },
        Some(install) => CheckResult {
            name,
            status: CheckStatus::Ok,
            // The trailing note is feature-aware: a build without the
            // winfsp feature tells the operator what to rebuild; a build
            // that carries it says so (the static "needs a --features
            // winfsp build" text used to print even on winfsp builds).
            detail: format!(
                "WinFsp installed at {} ({}) — {}",
                install.install_dir.display(),
                install
                    .dll
                    .as_ref()
                    .map(|dll| dll.display().to_string())
                    .unwrap_or_default(),
                if cfg!(feature = "winfsp") {
                    "this build carries the winfsp mount backend: `mount_backend = \"winfsp\"` \
                     is ready"
                } else {
                    "`mount_backend = \"winfsp\"` additionally needs a `--features winfsp` build"
                },
            ),
        },
    }
}

/// The backend-specific offline checks (Phase 2 / B3b dispatch unit):
/// appended by the CLI for baidu/local instances (telegram keeps its
/// fixed advisory instead). The baidu NETWORK leg (token liveness) is
/// [`baidu_connectivity_check`] over [`crate::baidu_backend_probe`] —
/// assembled separately because it dials out.
///
/// - **baidu**: the K18 proxy notice (a `proxy_url` has no effect — the
///   driver always connects directly);
/// - **local**: the root exists-and-is-writable probe (create + write +
///   remove, mirroring the cache check) and the K12 sync-unsupported
///   warning when `sync_url` is set (the task never starts), plus the
///   K18 proxy notice;
/// - **sftp** (Phase 4 / SF3): the sync-unsupported warning (the remote
///   filesystem is the source of truth, same ruling as local) plus the
///   K18 proxy notice. The connectivity leg — including the D2 host-key
///   fingerprint gate — is [`sftp_connectivity_check`] over
///   [`crate::sftp_backend_probe`], assembled separately because it
///   dials out.
pub fn backend_checks(cfg: &cloudkit_core::config::CyDriveConfig) -> Vec<CheckResult> {
    use cloudkit_core::config::Backend;
    let mut results = Vec::new();
    if let Some(notice) = crate::proxy_ineffective_warning(cfg) {
        results.push(CheckResult {
            name: "proxy_url".to_string(),
            status: CheckStatus::Warn,
            detail: notice.to_string(),
        });
    }
    match cfg.backend {
        Backend::Telegram => {}
        Backend::Baidu => {}
        Backend::Local => {
            if let Some(warning) = crate::local_sync_unsupported_warning(cfg) {
                results.push(CheckResult {
                    name: "sync".to_string(),
                    status: CheckStatus::Warn,
                    detail: warning.to_string(),
                });
            }
            results.push(check_local_root(cfg.local_root.as_deref()));
        }
        Backend::Sftp => {
            // The K12 warning covers both non-sync backends; the text
            // follows the backend (SFTP_SYNC_UNSUPPORTED for sftp).
            if let Some(warning) = crate::local_sync_unsupported_warning(cfg) {
                results.push(CheckResult {
                    name: "sync".to_string(),
                    status: CheckStatus::Warn,
                    detail: warning.to_string(),
                });
            }
        }
        Backend::Pan115 => {
            // The K12 warning covers the non-sync backends only (pan115
            // participates in sync, so this is a no-op today — kept for
            // symmetry with baidu).
            if let Some(warning) = crate::local_sync_unsupported_warning(cfg) {
                results.push(CheckResult {
                    name: "sync".to_string(),
                    status: CheckStatus::Warn,
                    detail: warning.to_string(),
                });
            }
            // D3 note: an unset pan115_root means the volume maps the
            // whole account — deletions land in the 115 recycle bin
            // (recoverable via the official client), but every path the
            // account holds becomes visible through this volume.
            if cfg.pan115_root.as_deref().unwrap_or("0") == "0" {
                results.push(CheckResult {
                    name: "pan115_root".to_string(),
                    status: CheckStatus::Warn,
                    detail: "pan115_root is unset: this volume maps the whole account \
                             (set a folder id to scope it; deletes go to the 115 \
                             recycle bin and recover via the official client)"
                        .to_string(),
                });
            }
        }
        // Phase 6 / 123-4: the pan123 offline checks — the K12 sync
        // warning is a no-op today (pan123 participates in sync like
        // baidu/pan115, kept for symmetry), and the D3 root note pairs
        // with the recycle-bin semantics as the double safety net: an
        // unset pan123_root maps the whole account through this volume.
        Backend::Pan123 => {
            if let Some(warning) = crate::local_sync_unsupported_warning(cfg) {
                results.push(CheckResult {
                    name: "sync".to_string(),
                    status: CheckStatus::Warn,
                    detail: warning.to_string(),
                });
            }
            // D3 note: an unset pan123_root means the volume maps the
            // whole account — deletions land in the 123 recycle bin
            // (recoverable via the official client), but every path the
            // account holds becomes visible through this volume.
            if cfg.pan123_root.as_deref().unwrap_or("0") == "0" {
                results.push(CheckResult {
                    name: "pan123_root".to_string(),
                    status: CheckStatus::Warn,
                    detail: "pan123_root is unset (or set to \"0\"): this volume maps \
                             the whole account (set a folder id to scope it; deletes \
                             go to the 123 recycle bin and recover via the official \
                             client)"
                        .to_string(),
                });
            }
        }
        // Phase 7 / WD4: the webdav offline checks — the K12 warning is
        // a no-op (webdav participates in sync, the pan115/pan123
        // ruling; kept for symmetry), and the D3 TLS-hatch warn makes a
        // `webdav_accept_invalid_certs = true` volume declare its risk
        // on every doctor pass (the assembly-time warn alone scrolls
        // away). The connectivity leg — the D1 five-state verdicts — is
        // [`webdav_connectivity_check`] over
        // [`crate::webdav_backend_probe`], assembled separately in
        // main.rs because it dials out.
        Backend::Webdav => {
            if let Some(warning) = crate::local_sync_unsupported_warning(cfg) {
                results.push(CheckResult {
                    name: "sync".to_string(),
                    status: CheckStatus::Warn,
                    detail: warning.to_string(),
                });
            }
            if cfg.webdav_accept_invalid_certs == Some(true) {
                results.push(CheckResult {
                    name: "webdav_accept_invalid_certs".to_string(),
                    status: CheckStatus::Warn,
                    detail: "webdav_accept_invalid_certs is enabled: TLS certificates are \
                             NOT verified for this volume (the self-signed escape hatch) — \
                             keep this off untrusted networks"
                        .to_string(),
                });
            }
        }
    }
    results
}

/// The sftp connectivity leg (Phase 4 / SF3): renders
/// [`crate::sftp_backend_probe`]'s verdict into one check result.
///
/// The D2 distinction is the load-bearing part: an unpinned host key is
/// **not** a failure of this machine — it is the explicit-accept step
/// every new server requires, so it surfaces as a Warn carrying the
/// server's actual fingerprint and the exact key to paste it into
/// (the non-interactive acceptance path — no popup, per D2's ruling).
/// A **changed** fingerprint is a Fail (the MITM signal, refused by
/// design); auth failures and unreachable servers Fail with their
/// actionable detail. Feature-gated with the driver (K30 pattern): a
/// binary without `sftp` has no probe value to render.
#[cfg(feature = "sftp")]
pub fn sftp_connectivity_check(probe: &ck_sftp::SftpProbe) -> CheckResult {
    use ck_sftp::SftpProbe;
    match probe {
        SftpProbe::Alive => CheckResult {
            name: "sftp connectivity".to_string(),
            status: CheckStatus::Ok,
            detail: "connected, authenticated, and the host-key fingerprint matches".to_string(),
        },
        SftpProbe::HostKeyUnpinned(actual) => CheckResult {
            name: "sftp host key".to_string(),
            status: CheckStatus::Warn,
            detail: format!(
                "the server's host key is not accepted yet; it presented {actual} — copy \
                 that value into this volume's sftp_host_fingerprint key to accept it \
                 (the driver refuses to connect until then: no silent trust-on-first-use)"
            ),
        },
        SftpProbe::HostKeyChanged { expected, actual } => CheckResult {
            name: "sftp host key".to_string(),
            status: CheckStatus::Fail,
            detail: format!(
                "the server's host key CHANGED: expected {expected}, got {actual} — a \
                 man-in-the-middle is one possible cause; the connection is refused. If \
                 the server was legitimately reinstalled, remove the old \
                 sftp_host_fingerprint value and accept the new one only after verifying \
                 it out-of-band"
            ),
        },
        SftpProbe::AuthFailed(detail) => CheckResult {
            name: "sftp authentication".to_string(),
            status: CheckStatus::Fail,
            detail: format!(
                "authentication failed: {detail} — check sftp_username, sftp_password / \
                 sftp_private_key_path and the passphrase"
            ),
        },
        SftpProbe::Unreachable(detail) => CheckResult {
            name: "sftp connectivity".to_string(),
            status: CheckStatus::Fail,
            detail: format!("cannot reach the server: {detail}"),
        },
    }
}

/// The local backend's root probe: `None` (no config to read a root
/// from) is a Warn mirroring the db/cache checks; an existing writable
/// directory is Ok (create_dir_all + write-and-remove probe file — the
/// cache check's shape); anything else is a Fail with guidance.
fn check_local_root(root: Option<&str>) -> CheckResult {
    let name = "local root".to_string();
    let Some(root) = root else {
        return CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: "no local_root to check (no usable config)".to_string(),
        };
    };
    let path = PathBuf::from(root);
    let probe = path.join(".cydrive-doctor-probe");
    // create_dir_all mirrors the driver factory's own boot behavior
    // (LocalDriver::new creates the root) — a deep path that boot would
    // create must not fail here either.
    let probe_result = std::fs::create_dir_all(&path)
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
            detail: format!(
                "{} cannot be created or written: {error} — set local_root to a \
                 directory this user can write (an absolute path)",
                path.display()
            ),
        },
    }
}

/// The doctor verdict over the baidu probe's outcome
/// ([`crate::baidu_backend_probe`]): Alive → Ok (the token-liveness
/// detail names the read-only probe and where writability IS
/// discovered); NeedsReauth → Fail with the re-setup guidance (the
/// human-actionable case); Unreachable → Warn (transient — retry /
/// check the network path).
pub fn baidu_connectivity_check(probe: &crate::BackendProbe) -> CheckResult {
    let name = "baidu connectivity".to_string();
    match probe {
        crate::BackendProbe::Alive => CheckResult {
            name,
            status: CheckStatus::Ok,
            detail: "token alive (uinfo + quota answered) and the backend root lists \
                     read-only; root writability is discovered on the first upload"
                .to_string(),
        },
        crate::BackendProbe::NeedsReauth(reason) => CheckResult {
            name,
            status: CheckStatus::Fail,
            // 人话重授权指引（§7a：凭据失效不得死循环重试——指向 setup）
            detail: format!(
                "{reason}; re-run `cydrive setup` with a fresh refresh_token to \
                 re-authorize this instance"
            ),
        },
        crate::BackendProbe::Unreachable(reason) => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!(
                "the baidu backend did not answer ({reason}); check the network path \
                 (baidu connects DIRECTLY — a proxy_url does not apply) and retry `cydrive \
                 doctor`"
            ),
        },
    }
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

// ------------------------------------------------- multi-volume (MV4) ---

/// The multi-volume doctor body (Phase 2.5 / MV4): the process-level
/// checks — config presence carrying the volume count plus both listen
/// ports (the same occupancy probes the single-volume doctor runs) — and
/// one [`volume_checks`] group per discovered volume. Deliberately absent
/// versus the single-volume [`run_doctor`] are the process-level
/// db/cache/credentials checks: in multi-volume mode those paths and
/// secrets belong to the volumes, so they are probed per volume instead
/// (the process config's default paths would be phantom targets).
pub fn run_doctor_multi(
    process: &cloudkit_core::config::CyDriveConfig,
    volumes: &[cloudkit_core::config::VolumeConfig],
) -> Vec<CheckResult> {
    let mut results = vec![
        CheckResult {
            name: "config".to_string(),
            status: CheckStatus::Ok,
            detail: format!(
                "multi-volume config.toml with {} volume(s) under {}",
                volumes.len(),
                process.volumes_dir.as_deref().unwrap_or("volumes"),
            ),
        },
        check_port("webdav port", process.webdav_port),
        check_port("web ui port", process.web_ui_port),
    ];
    for spec in volumes {
        results.extend(volume_checks(spec));
    }
    results
}

/// One volume's check group (Phase 2.5 / MV4), each result named
/// `"volume <name>: <check>"`:
///
/// - **config** — the K21-resolved settings pass the same
///   [`CyDriveConfig::validate`] the multi-volume run flow gates every
///   volume through (the resolution itself mirrors boot: it creates the
///   volume home, like the driver factory creates a local root);
/// - **home** — the volume's home directory is creatable and writable
///   (the cache check's write-and-remove probe);
/// - **database** — the resolved db path opens and answers `get_stats`
///   ([`database_check`]'s semantics: fresh file → Warn, unopenable →
///   Fail);
/// - the volume's [`backend_checks`] over the resolved settings (local
///   root writability, the K18 proxy notice), renamed per volume.
///
/// Drive-letter conflicts never appear here: [`load_volumes`] already
/// refuses them at discovery (K27). All checks run independently — one
/// broken check must not mask its siblings.
pub fn volume_checks(spec: &cloudkit_core::config::VolumeConfig) -> Vec<CheckResult> {
    let prefix = format!("volume {}:", spec.name);
    let mut results = Vec::new();

    // -- config legality (K21 resolution + validate, the run gate's face)
    let resolved = crate::resolve_volume_settings(spec);
    let settings = match &resolved {
        Ok(settings) => match settings.validate() {
            Ok(()) => CheckResult {
                name: format!("{prefix} config"),
                status: CheckStatus::Ok,
                detail: format!(
                    "{} valid ({} backend)",
                    spec.file_path.display(),
                    spec.settings.backend.as_str()
                ),
            },
            Err(error) => CheckResult {
                name: format!("{prefix} config"),
                status: CheckStatus::Fail,
                detail: format!("invalid configuration: {error}"),
            },
        },
        Err(error) => CheckResult {
            name: format!("{prefix} config"),
            status: CheckStatus::Fail,
            detail: format!(
                "resolving the volume's home/settings under {} failed: {error:#}",
                spec.file_path.display()
            ),
        },
    };
    results.push(settings);

    // -- home directory creatable/writable
    match crate::volume_home(spec) {
        Ok(home) => {
            let probe = home.join(".cydrive-doctor-probe");
            let probe_result = std::fs::create_dir_all(&home)
                .and_then(|()| std::fs::write(&probe, b"probe"))
                .and_then(|()| std::fs::remove_file(&probe));
            results.push(match probe_result {
                Ok(()) => CheckResult {
                    name: format!("{prefix} home"),
                    status: CheckStatus::Ok,
                    detail: format!("{} exists and is writable", home.display()),
                },
                Err(error) => CheckResult {
                    name: format!("{prefix} home"),
                    status: CheckStatus::Fail,
                    detail: format!("{} is not writable: {error}", home.display()),
                },
            });
        }
        Err(error) => results.push(CheckResult {
            name: format!("{prefix} home"),
            status: CheckStatus::Fail,
            detail: format!("resolving the volume home failed: {error:#}"),
        }),
    }

    // -- database: the resolved db path when resolution succeeded, the
    //    read-only re-derivation otherwise (the probe still runs — one
    //    broken check must not remove its siblings)
    let db_path = match &resolved {
        Ok(settings) => Some(PathBuf::from(&settings.db_path)),
        Err(_) => crate::volume_home(spec)
            .ok()
            .map(|home| crate::resolve_volume_path(&home, &spec.settings.db_path)),
    };
    results.push(database_check(
        &format!("{prefix} database"),
        db_path.as_ref(),
    ));

    // -- the volume's backend-specific offline checks, renamed per volume
    if let Ok(settings) = &resolved {
        for check in backend_checks(settings) {
            results.push(CheckResult {
                name: format!("{prefix} {}", check.name),
                status: check.status,
                detail: check.detail,
            });
        }
    }
    results
}
/// The pan115 connectivity leg (Phase 5 / 115-4): renders
/// [`crate::pan115_backend_probe`]'s verdict into one check result.
///
/// The auth distinction is the load-bearing part: a dead token pair is
/// **not** a transient failure — it needs a re-scan (or hand-filled
/// tokens), so `NeedsReauth` Fails with that actionable detail;
/// `RateLimited` warns because the driver already entered its hard
/// backoff window (D4/K69.3) and the retry is self-scheduled. Feature-
/// gated with the driver (K30 pattern): a binary without `pan115` has
/// no probe value to render.
#[cfg(feature = "pan115")]
pub fn pan115_connectivity_check(probe: &ck_pan115::Pan115Probe) -> CheckResult {
    use ck_pan115::Pan115Probe;
    match probe {
        Pan115Probe::Alive { uid, free, total } => CheckResult {
            name: "pan115_connectivity".to_string(),
            status: CheckStatus::Ok,
            detail: match total {
                Some(total) => format!(
                    "token alive for account {uid}; {} / {} bytes free",
                    free, total
                ),
                None => format!("token alive for account {uid} (quota unknown)"),
            },
        },
        Pan115Probe::NeedsReauth => CheckResult {
            name: "pan115_connectivity".to_string(),
            status: CheckStatus::Fail,
            detail: "the pan115 token pair is no longer accepted — re-scan the QR in \
                     `cydrive setup`, or write fresh pan115_access_token / \
                     pan115_refresh_token values into the volume config"
                .to_string(),
        },
        Pan115Probe::RateLimited { window } => CheckResult {
            name: "pan115_connectivity".to_string(),
            status: CheckStatus::Warn,
            detail: format!(
                "the 115 account hit its access cap (770004); the driver backs off for \
                 {}s before retrying — lower the request rate if this repeats",
                window.as_secs()
            ),
        },
        Pan115Probe::Unreachable { detail } => CheckResult {
            name: "pan115_connectivity".to_string(),
            status: CheckStatus::Fail,
            detail: format!("could not reach 115: {detail}"),
        },
    }
}

/// 流量/空间字节的人话化（GiB/MiB 两档足够诊断面——123-0 实测日额
/// ≈10GiB、空间 2TiB 量级）。
#[cfg(feature = "pan123")]
fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// The pan123 connectivity leg (Phase 6 / 123-4): renders
/// [`crate::pan123_backend_probe`]'s verdict into one check result.
///
/// The auth distinction is the load-bearing part: a dead token is **not**
/// a transient failure — the web API has no refresh (K76.4), so
/// `NeedsReauth` Fails with the re-scan / re-paste guidance. The Alive
/// detail carries the **D5 traffic face**: the daily download remain
/// human-readably, with the VIP guidance when the account is not a
/// member (a 123pan VIP lifts the cap) and a Warn when the quota is
/// exhausted (downloads will hard-fail `RateLimited` until it resets).
/// Feature-gated with the driver (K30 pattern): a binary without
/// `pan123` has no probe value to render.
#[cfg(feature = "pan123")]
pub fn pan123_connectivity_check(probe: &ck_pan123::Pan123Probe) -> CheckResult {
    use ck_pan123::Pan123Probe;
    match probe {
        Pan123Probe::Alive {
            uid,
            free,
            total,
            traffic_remain,
            vip,
        } => {
            let quota_detail = match total {
                Some(total) => format!("{} of {} free", human_bytes(*free), human_bytes(*total)),
                None => "space unknown".to_string(),
            };
            if *traffic_remain == Some(0) {
                return CheckResult {
                    name: "pan123_connectivity".to_string(),
                    status: CheckStatus::Warn,
                    detail: format!(
                        "token alive for account {uid} ({quota_detail}); the daily download \
                         traffic quota is EXHAUSTED — downloads fail until it resets (tomorrow); \
                         a 123pan VIP subscription lifts the cap (D5: not bypassed)"
                    ),
                };
            }
            let traffic_detail = match traffic_remain {
                Some(remain) => format!(
                    "daily download traffic remaining: {}{}",
                    human_bytes(*remain),
                    if *vip {
                        String::new()
                    } else {
                        " (a 123pan VIP lifts this cap)".to_string()
                    }
                ),
                None => "daily download traffic: unknown (the check failed)".to_string(),
            };
            CheckResult {
                name: "pan123_connectivity".to_string(),
                status: CheckStatus::Ok,
                detail: format!("token alive for account {uid} ({quota_detail}); {traffic_detail}"),
            }
        }
        Pan123Probe::NeedsReauth => CheckResult {
            name: "pan123_connectivity".to_string(),
            status: CheckStatus::Fail,
            detail: "the pan123 token is no longer accepted (the web API has no refresh — \
                     it lives ~90 days): re-run the QR scan / sign_in in `cydrive setup`, or \
                     paste a fresh pan123_token value into the volume config"
                .to_string(),
        },
        Pan123Probe::Unreachable { detail } => CheckResult {
            name: "pan123_connectivity".to_string(),
            status: CheckStatus::Fail,
            detail: format!("could not reach 123pan: {detail}"),
        },
    }
}

/// The webdav connectivity leg (Phase 7 / WD4): renders
/// [`ck_webdav::WebdavProbe`]'s five-state verdict into one check
/// result. The network leg lives in the driver (`ck_webdav::probe` —
/// OPTIONS + the D1 auth negotiation); this is the pure renderer, so
/// every state carries its actionable way out (K31 style) and no
/// credential material ever reaches the text:
///
/// - ① `Alive` → **Ok**, reporting the `DAV:` class and `Allow:` summary
///   (both headers optional per RFC — absent keeps the Ok);
/// - ② `CredentialsRejected` → **Fail**, keeping the driver's
///   same-source detail (missing credentials / Basic refused / NTLM /
///   rejected-after-negotiation) and naming the credential keys;
/// - ③ `ReachableNoAuth` → **Warn** (anonymous servers are legal) with
///   the configure-credentials suggestion;
/// - ④ `Unreachable` → **Fail**, keeping the transport cause plus the
///   webdav_url / network / proxy checklist;
/// - ⑤ `TlsUntrusted` → **Warn** (the explicit-accept semantic, the sftp
///   HostKeyUnpinned precedent): reachable, the escape-hatch key named,
///   the security note attached.
///
/// Feature-gated with the driver (K30 pattern): a binary without
/// `webdav` skips the leg entirely (the main.rs call site).
#[cfg(feature = "webdav")]
pub fn webdav_connectivity_check(probe: &ck_webdav::WebdavProbe) -> CheckResult {
    use ck_webdav::WebdavProbe;
    let name = "webdav_connectivity".to_string();
    match probe {
        WebdavProbe::Alive { dav_class, allow } => {
            // 头缺席的降级措辞（RFC 4918/9110 不强制两头——不因缺席翻脸）。
            let dav = dav_class
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| "none advertised".to_string());
            let allow = allow
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| "none advertised".to_string());
            CheckResult {
                name,
                status: CheckStatus::Ok,
                detail: format!("reachable and authenticated (DAV class: {dav}; allows: {allow})"),
            }
        }
        WebdavProbe::CredentialsRejected { detail } => CheckResult {
            name,
            status: CheckStatus::Fail,
            detail: format!(
                "{detail} — check webdav_username and webdav_password in config.toml (or \
                 the volume file)"
            ),
        },
        WebdavProbe::ReachableNoAuth => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: "reachable without authentication: the server allows anonymous access \
                     — set webdav_username and webdav_password if this share is meant to \
                     require them"
                .to_string(),
        },
        WebdavProbe::Unreachable { detail } => CheckResult {
            name,
            status: CheckStatus::Fail,
            detail: format!(
                "cannot reach the server ({detail}) — check webdav_url, the network path \
                 to it, and any proxy in front of the server"
            ),
        },
        WebdavProbe::TlsUntrusted { detail } => CheckResult {
            name,
            status: CheckStatus::Warn,
            detail: format!(
                "the server is reachable but its TLS certificate failed validation \
                 ({detail}); if it uses a self-signed certificate, set \
                 webdav_accept_invalid_certs = true in the volume config — note this \
                 disables certificate verification for the volume (a security risk on \
                 untrusted networks)"
            ),
        },
    }
}
