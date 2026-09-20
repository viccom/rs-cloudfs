//! pan123-spike — Phase 6 (pan123 driver) 123-0 spike: live 123pan web-API
//! probes. Read-path legs (endpoint generations / auth / list+trash
//! roundtrip / download_info + Range + traffic quota) and write-path legs
//! (full seven-step upload chain, duplicate 1-vs-2 semantics, rapid-upload
//! Reuse shape, part retention = resume, empty/omitted/wrong etag).
//! Shapes follow examples/pan115_spike (anyhow + small modules + masked
//! output); endpoint casing follows 123panNextGen (2026-09) verbatim and
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
//! pan123-spike probe-upload <file> [--parent X] # minimal single-slice chain (old presign)
//! pan123-spike probe-trash <fid> <parent> [--gen old]   # correct payload + read-back
//! pan123-spike probe-trash-trap <fid> <parent> <name>  # wrong-payload trap repro
//! pan123-spike probe-download <fid> [--file P]  # traffic + info + Range 206
//! pan123-spike probe-user                       # quota / traffic fields
//! pan123-spike cleanup <prefix>                 # strict-prefix sweep + verify
//! pan123-spike upload-full <file> [--parent X] [--name X] [--part-mb N]  # ⑤ seven steps
//! pan123-spike probe-duplicate <v1> <v2> [--parent X] [--dup 1|2]        # 钉死 a
//! pan123-spike probe-reuse <file> [--parent X]                           # 钉死 b
//! pan123-spike probe-resume <file> [--parent X]                          # 钉死 c
//! pan123-spike probe-etag <file> [--parent X]                            # 钉死 d
//! pan123-spike complete-v2 <fid> <bucket> <key> <uploadId> <node> <size> [--form new|v2] [--name X]
//! pan123-spike probe-single                    # single-part completion matrix (E0..E4)
//! pan123-spike list-prefix <prefix> [--parent X]
//! ```
//!
//! Env: PAN123_SPIKE_TEST_DIR (default E:\GitHub\rs-CyDrive\test — gitignored,
//! outside the repo) for pan123-test-account.json / pan123-tokens.json;
//! PAN123_SPIKE_PROXY optional (default: direct, no system proxy).

mod api;
mod probes;
mod state;
mod upload;

use anyhow::Result;

fn usage() -> ! {
    eprintln!(
        "usage: pan123-spike <sign-in | probe-dydomain | probe-matrix [--fid N] | probe-qr [--polls N] |\n\
         \x20        probe-list [--parent X] [--limit N] [--trashed] | probe-mkdir <name> [--parent X] |\n\
         \x20        gen-file <size-kb> [--out P] | probe-upload <file> [--parent X] |\n\
         \x20        probe-trash <fid> <parent> [--gen old] | probe-trash-trap <fid> <parent> <name> |\n\
         \x20        probe-download <fid> [--file P] | probe-user | cleanup <prefix> |\n\
         \x20        upload-full <file> [--parent X] [--name X] [--part-mb N] |\n\
         \x20        probe-duplicate <v1> <v2> [--parent X] | probe-reuse <file> [--parent X] |\n\
         \x20        probe-resume <file> [--parent X] | probe-etag <file> [--parent X]>"
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
        "upload-full" => upload::cmd_upload_full(rest).await,
        "probe-duplicate" => upload::cmd_probe_duplicate(rest).await,
        "probe-reuse" => upload::cmd_probe_reuse(rest).await,
        "probe-resume" => upload::cmd_probe_resume(rest).await,
        "probe-etag" => upload::cmd_probe_etag(rest).await,
        "complete-v2" => upload::cmd_complete_v2(rest).await,
        "probe-single" => upload::cmd_probe_single(rest).await,
        "list-prefix" => upload::cmd_list_prefix(rest).await,
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
