//! webdav-spike — Phase 7 (WebDAV driver) WD0 spike: pin the live quirk
//! matrix of two WebDAV servers with automated probes.
//!
//! Fixtures: rclone serve webdav v1.60.1 (Basic auth) and Apache mod_dav
//! 2.4.58 (Digest auth; plus a `/dav-stale/` vhost with
//! AuthDigestNonceLifetime=2s for stale-nonce re-negotiation).
//!
//! Legs: PROPFIND shapes / mtime write forms / Range forms / MOVE forms /
//! MKCOL quirks / DELETE forms / hand-rolled RFC 7616 Digest chain (nc
//! replay + stale=true renegotiation) / chunked (no Content-Length) PUT /
//! quota props. Every observation prints as `[server] <probe> <item>: value`
//! so the output can be diffed against the expected quirk table.
//!
//! ```text
//! webdav-spike matrix    # full matrix on both servers + self-cleanup
//! webdav-spike cleanup   # sweep leftover wd0-* trees on both servers
//! ```
//!
//! Env (all required, never printed): WEBDAV_SPIKE_RCLONE_URL /
//! WEBDAV_SPIKE_APACHE_URL / WEBDAV_SPIKE_APACHE_STALE_URL /
//! WEBDAV_SPIKE_USER / WEBDAV_SPIKE_PASS.

mod client;
mod digest;
mod probes;
mod xml;

fn usage() -> ! {
    eprintln!("usage: webdav-spike <matrix | cleanup>");
    eprintln!("  matrix   full quirk matrix on rclone + apache fixtures, then self-cleanup");
    eprintln!("  cleanup  sweep leftover wd0-* trees on both fixtures");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("matrix") => probes::cmd_matrix().await,
        Some("cleanup") => probes::cmd_cleanup().await,
        _ => usage(),
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("webdav-spike FAILED: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
