//! pan123-spike — Phase 6 (pan123 driver) 123-0 read-path spike: live 123pan
//! web-API probes (endpoint generations / auth / list+trash roundtrip /
//! download_info + Range + traffic quota). Shapes follow
//! examples/pan115_spike (anyhow + small modules + masked output); endpoint
//! candidates come from pan123-rs (2026-06) vs 123panNextGen (2026-09) and
//! every probe records which generation actually answers.
//!
//! ```text
//! pan123-spike sign-in                          # password login -> token persisted
//! pan123-spike probe-dydomain                   # /api/dydomain discovery liveness
//! pan123-spike probe-matrix [--fid N]           # endpoint generation matrix
//! pan123-spike probe-qr [--polls N]             # QR generate + poll (no human)
//! pan123-spike probe-list [--parent X] [--limit N] [--trashed]
//! pan123-spike probe-mkdir <name> [--parent X]  # create + 5060 repeat sample
//! pan123-spike gen-file <size-kb> [--out P]     # random payload + md5
//! pan123-spike probe-upload <file> [--parent X] # minimal single-slice chain
//! pan123-spike probe-trash <fid> <parent> [--gen old]   # correct payload + read-back
//! pan123-spike probe-trash-trap <fid> <parent> <name>  # wrong-payload trap repro
//! pan123-spike probe-download <fid> [--file P]  # traffic + info + Range 206
//! pan123-spike probe-user                       # quota / traffic fields
//! pan123-spike cleanup <prefix>                 # strict-prefix sweep + verify
//! ```
//!
//! Env: PAN123_SPIKE_TEST_DIR (default E:\GitHub\rs-CyDrive\test — gitignored,
//! outside the repo) for pan123-test-account.json / pan123-tokens.json;
//! PAN123_SPIKE_PROXY optional (default: direct, no system proxy).

mod api;
mod probes;
mod state;

use anyhow::Result;

fn usage() -> ! {
    eprintln!(
        "usage: pan123-spike <sign-in | probe-dydomain | probe-matrix [--fid N] | probe-qr [--polls N] |\n\
         \x20        probe-list [--parent X] [--limit N] [--trashed] | probe-mkdir <name> [--parent X] |\n\
         \x20        gen-file <size-kb> [--out P] | probe-upload <file> [--parent X] |\n\
         \x20        probe-trash <fid> <parent> [--gen old] | probe-trash-trap <fid> <parent> <name> |\n\
         \x20        probe-download <fid> [--file P] | probe-user | cleanup <prefix>>"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(cmd) = args.get(1).map(String::as_str) else {
        usage()
    };
    let rest = &args[2..];

    let result: Result<i32> = match cmd {
        "sign-in" => probes::cmd_sign_in(rest).await,
        "probe-dydomain" => probes::cmd_probe_dydomain(rest).await,
        "probe-matrix" => probes::cmd_probe_matrix(rest).await,
        "probe-qr" => probes::cmd_probe_qr(rest).await,
        "probe-list" => probes::cmd_probe_list(rest).await,
        "probe-mkdir" => probes::cmd_probe_mkdir(rest).await,
        "gen-file" => probes::cmd_gen_file(rest).await,
        "probe-upload" => probes::cmd_probe_upload(rest).await,
        "probe-trash" => probes::cmd_probe_trash(rest).await,
        "probe-trash-trap" => probes::cmd_probe_trash_trap(rest).await,
        "probe-download" => probes::cmd_probe_download(rest).await,
        "probe-heads" => probes::cmd_probe_heads(rest).await,
        "probe-user" => probes::cmd_probe_user(rest).await,
        "cleanup" => probes::cmd_cleanup(rest).await,
        _ => usage(),
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pan123-spike: {cmd} FAILED: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
