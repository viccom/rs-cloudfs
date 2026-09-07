//! baidu-spike — Batch S verification-driven spike against Baidu Pan
//! (rs-cloudfs Phase 1, 2026-09-07). Precedent:
//! crates/drivers/ck-telegram/examples/history_spike.rs.
//!
//! Subcommands (each prints `[SUMMARY] ...` evidence lines):
//!
//! ```text
//! baidu-spike refresh          # (S-1) OAuth refresh_token grant, cache to %TEMP%
//! baidu-spike qps              # (S-2) list x10 / superfile2 x10 / download x5 (seq + burst)
//! baidu-spike resume abort     # (S-3) phase 1: precreate + 3 parts, then hard-abort
//! baidu-spike resume continue  # (S-3) phase 2: precreate diff-set, upload only missing
//! baidu-spike rapid            # (S-4) return_type=1/2 precreate branches
//! baidu-spike dlink            # (S-5) 302+Range recheck + dlink cache-duration probe
//! baidu-spike throughput       # (S-6) 1 GiB three-step upload + streamed download
//! baidu-spike cleanup          # delete everything under the spike remote dir
//! ```
//!
//! Env overrides: BAIDU_SPIKE_INSTANCE, BAIDU_SPIKE_PCFS_GO,
//! BAIDU_SPIKE_CLIENT_ID/SECRET, BAIDU_SPIKE_TMP.
//! Run: cargo run --manifest-path examples/baidu_spike/Cargo.toml --release -- <cmd>

mod api;
mod cmds;
mod common;
mod gen;

fn usage() -> ! {
    eprintln!(
        "usage: baidu-spike <refresh|qps|resume abort|resume continue|rapid|dlink|throughput|cleanup>"
    );
    eprintln!("debug:  baidu-spike <ls|rapid-probe|dl-try>");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(cmd) = args.get(1).map(String::as_str) else {
        usage()
    };
    let sub = args.get(2).map(String::as_str);

    let result = match (cmd, sub) {
        ("refresh", _) => cmds::refresh().await,
        ("qps", _) => cmds::qps().await,
        ("resume", Some("abort")) => cmds::resume_abort().await,
        ("resume", Some("continue")) => cmds::resume_continue().await,
        ("rapid", _) => cmds::rapid().await,
        ("rapid-probe", _) => cmds::rapid_probe().await,
        ("dlink", _) => cmds::dlink().await,
        ("throughput", _) => cmds::throughput().await,
        ("ls", _) => cmds::ls().await,
        ("dl-try", _) => cmds::dl_try().await,
        ("cleanup", _) => cmds::cleanup().await,
        _ => usage(),
    };

    if let Err(e) = result {
        let chain = format!("{e:#}");
        eprintln!("baidu-spike: {cmd} FAILED: {}", common::scrub(&chain));
        std::process::exit(1);
    }
}
