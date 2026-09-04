//! Red tests for the `cydrive status` subcommand body (plan
//! `docs/plans/2026-09-04-status-and-automount.md`, contract C3).
//!
//! The subcommand is data collection + pure rendering, pinned here in
//! two layers:
//!
//! - [`collect_status`] probes a real control channel (`ControlServer`
//!   bound against a tempdir config) and stand-in TCP listeners for the
//!   WebDAV/dashboard ports — `collect_status` only asks "does the port
//!   answer a connect", so a listener that never accepts is a faithful
//!   "something is listening" double;
//! - [`render_status`] is pure and its whole output is pinned verbatim
//!   (label column 12 wide, control line omitted without a control file,
//!   stale control marked, dashboard `disabled` when the UI is off).
//!
//! The instance row carries the version line from the control channel's
//! PING reply, so the expected literals glue in
//! `env!("CARGO_PKG_VERSION")` instead of hard-coding a version.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;

use cydrive_cli::control::ControlServer;
use cydrive_cli::{collect_status, render_status, StatusReport};
use cydrive_core::config::CyDriveConfig;

// ------------------------------------------------------------- helpers ---

/// A config anchored in `dir` (the `control_channel`/`run_e2e` shape) with
/// the two probe ports injected: the control file lands in `dir` itself.
fn temp_config(dir: &Path, webdav_port: u16, web_ui_port: u16) -> CyDriveConfig {
    CyDriveConfig {
        db_path: dir.join("meta.db").to_string_lossy().into_owned(),
        cache_path: dir.join("cache").to_string_lossy().into_owned(),
        webdav_host: "127.0.0.1".to_string(),
        webdav_port,
        web_ui_host: "127.0.0.1".to_string(),
        web_ui_port,
        enable_web_ui: true,
        ..CyDriveConfig::default()
    }
}

fn listener_port(listener: &TcpListener) -> u16 {
    listener.local_addr().expect("local addr").port()
}

// ------------------------------------------------------------ scenarios ---

/// C3: with a live control channel and both ports answering, all five
/// fields carry their healthy values — the PING reply on `instance`, the
/// control file's address, the glued URLs, and (nothing on this machine
/// maps an ephemeral-loopback URL) no mount.
#[tokio::test]
async fn collect_status_full_picture() {
    let dir = tempfile::tempdir().expect("temp dir");
    let webdav = TcpListener::bind("127.0.0.1:0").expect("bind the WebDAV stand-in");
    let dashboard = TcpListener::bind("127.0.0.1:0").expect("bind the dashboard stand-in");
    let cfg = temp_config(
        dir.path(),
        listener_port(&webdav),
        listener_port(&dashboard),
    );

    let server = ControlServer::bind(&cfg)
        .await
        .expect("bind the control server");
    let control_addr: SocketAddr = server.local_addr();
    tokio::spawn(async move {
        let _ = server.run(|| {}).await;
    });

    let report = collect_status(&cfg).await;

    assert!(
        report
            .instance
            .as_deref()
            .is_some_and(|reply| reply.contains("OK: cydrive")),
        "a live instance yields its PING reply on instance: {:?}",
        report.instance
    );
    assert!(
        report
            .instance
            .as_deref()
            .is_some_and(|reply| reply.contains(env!("CARGO_PKG_VERSION"))),
        "the instance reply carries this binary's version: {:?}",
        report.instance
    );
    let expected_control = control_addr.to_string();
    assert_eq!(
        report.control.as_deref(),
        Some(expected_control.as_str()),
        "the control file's address lands on control"
    );
    let expected_webdav = format!("http://127.0.0.1:{}", listener_port(&webdav));
    assert_eq!(
        report.webdav.as_deref(),
        Some(expected_webdav.as_str()),
        "an answering WebDAV port yields its glued URL"
    );
    let expected_dashboard = format!("http://127.0.0.1:{}", listener_port(&dashboard));
    assert_eq!(
        report.dashboard.as_deref(),
        Some(expected_dashboard.as_str()),
        "an answering dashboard port yields its glued URL"
    );
    assert_eq!(
        report.mount, None,
        "nothing maps this ephemeral loopback URL"
    );
}

/// C3: with no control file and nothing listening, every probe degrades
/// to its `None` value — the all-down picture the renderer turns into
/// the "not running / not reachable / not mounted" report.
#[tokio::test]
async fn collect_status_all_down() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Ports nothing serves: bind `:0`, read the port, drop the listener —
    // the ephemeral port is free again (the re-bind race window is
    // negligible for one probe).
    let webdav_port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe a free port");
        listener_port(&probe)
    };
    let web_ui_port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe a free port");
        listener_port(&probe)
    };
    let cfg = temp_config(dir.path(), webdav_port, web_ui_port);

    let report = collect_status(&cfg).await;

    assert_eq!(report.instance, None, "no instance is running");
    assert_eq!(report.control, None, "no control file exists");
    assert_eq!(report.webdav, None, "nothing answers the WebDAV port");
    assert_eq!(report.dashboard, None, "nothing answers the dashboard port");
    assert_eq!(report.mount, None, "nothing is mounted");
}

/// C3: the renderer's whole output is pinned verbatim — running picture,
/// all-down picture, and the stale-control + dashboard-down fragments
/// (label column 12 wide; no control line without a control file; the
/// instance parenthetical is the PING reply verbatim).
#[test]
fn render_status_format_pinned() {
    let running = StatusReport {
        instance: Some(format!("OK: cydrive {}", env!("CARGO_PKG_VERSION"))),
        control: Some("127.0.0.1:38869".to_string()),
        webdav: Some("http://127.0.0.1:8289".to_string()),
        dashboard: Some("http://127.0.0.1:8288".to_string()),
        mount: Some("Y:".to_string()),
    };
    let rendered = render_status(
        &running,
        "http://127.0.0.1:8289",
        Some("http://127.0.0.1:8288".to_string()),
    );
    let expected = concat!(
        "instance:   running (OK: cydrive ",
        env!("CARGO_PKG_VERSION"),
        ")\n",
        "control:    127.0.0.1:38869\n",
        "webdav:     http://127.0.0.1:8289 listening\n",
        "dashboard:  http://127.0.0.1:8288 listening\n",
        "mount:      Y: -> http://127.0.0.1:8289",
    );
    assert_eq!(rendered, expected);

    // All down, dashboard disabled: the control line is omitted (no
    // control file) and the disabled dashboard says so.
    let down = StatusReport {
        instance: None,
        control: None,
        webdav: None,
        dashboard: None,
        mount: None,
    };
    let rendered = render_status(&down, "http://127.0.0.1:8289", None);
    let expected = concat!(
        "instance:   not running\n",
        "webdav:     http://127.0.0.1:8289 not reachable\n",
        "dashboard:  disabled\n",
        "mount:      not mounted",
    );
    assert_eq!(rendered, expected);

    // Stale control file (address present, instance dead) and a
    // configured-but-dead dashboard: the stale marker on the control
    // line, `not reachable` on the dashboard line.
    let stale = StatusReport {
        instance: None,
        control: Some("127.0.0.1:1".to_string()),
        webdav: Some("http://127.0.0.1:8289".to_string()),
        dashboard: None,
        mount: None,
    };
    let rendered = render_status(
        &stale,
        "http://127.0.0.1:8289",
        Some("http://127.0.0.1:8288".to_string()),
    );
    let expected = concat!(
        "instance:   not running\n",
        "control:    127.0.0.1:1 (stale)\n",
        "webdav:     http://127.0.0.1:8289 listening\n",
        "dashboard:  http://127.0.0.1:8288 not reachable\n",
        "mount:      not mounted",
    );
    assert_eq!(rendered, expected);
}
