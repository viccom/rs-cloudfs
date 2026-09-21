//! Matrix legs. Every observation prints as `[server] <probe> <item>: value`
//! so the output can be diffed against the expected quirk table; pinned
//! expectations from the WD0 brief print `(expect X) [OK|MISMATCH]`.

use crate::client::{Config, DavServer, ALLPROP_PROPFIND};
use crate::xml::{self, PropfindEntry};
use anyhow::{bail, Context, Result};
use rand::RngCore;
use std::fmt::Display;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Bookkeeping

struct Stat {
    server: String,
    run_root: String,
    mismatches: u32,
    leg_errors: u32,
}

impl Stat {
    fn new(server: &str, run_root: &str) -> Stat {
        Stat {
            server: server.to_string(),
            run_root: run_root.to_string(),
            mismatches: 0,
            leg_errors: 0,
        }
    }

    fn note(&self, probe: &str, item: &str, val: impl Display) {
        println!("[{}] {} {}: {}", self.server, probe, item, val);
    }

    fn expect(&mut self, probe: &str, item: &str, obs: impl Display, exp: &str) {
        let o = obs.to_string();
        if o == exp {
            println!("[{}] {} {}: {} (expect {}) [OK]", self.server, probe, item, o, exp);
        } else {
            println!(
                "[{}] {} {}: {} (expect {}) [MISMATCH]",
                self.server, probe, item, o, exp
            );
            self.mismatches += 1;
        }
    }

    fn expect_contains(&mut self, probe: &str, item: &str, obs: impl Display, needle: &str) {
        let o = obs.to_string();
        if o.contains(needle) {
            println!(
                "[{}] {} {}: {} (expect contains {}) [OK]",
                self.server, probe, item, o, needle
            );
        } else {
            println!(
                "[{}] {} {}: {} (expect contains {}) [MISMATCH]",
                self.server, probe, item, o, needle
            );
            self.mismatches += 1;
        }
    }

    fn finish(&self) -> u32 {
        println!(
            "[{}] summary: {} mismatch(es), {} leg error(s)",
            self.server, self.mismatches, self.leg_errors
        );
        self.mismatches + self.leg_errors
    }
}

macro_rules! leg {
    ($stat:expr, $srv:expr, $name:literal, $f:expr) => {
        match $f(&mut $stat, &mut $srv).await {
            Ok(()) => {}
            Err(e) => {
                println!("[{}] {}: LEG ERROR: {e:#}", $stat.server, $name);
                $stat.leg_errors += 1;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Small helpers

fn header_str(resp: &reqwest::Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(none)")
        .to_string()
}

fn squash(s: &str, n: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let t = flat.trim();
    if t.chars().count() > n {
        let head: String = t.chars().take(n).collect();
        format!("{head}...[+{} chars]", t.chars().count() - n)
    } else {
        t.to_string()
    }
}

fn epoch_of(lm: &str) -> String {
    httpdate::parse_http_date(lm)
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|| "PARSE-FAIL".to_string())
}

async fn mkcol_deep(srv: &mut DavServer, path: &str) -> Result<()> {
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        acc.push_str(seg);
        acc.push('/');
        srv.mkcol(&acc).await?;
    }
    Ok(())
}

async fn propfind_status(srv: &mut DavServer, path: &str) -> Result<u16> {
    Ok(srv.propfind(path, "0", ALLPROP_PROPFIND).await?.status().as_u16())
}

async fn get_lastmod(srv: &mut DavServer, path: &str) -> Result<String> {
    let resp = srv.propfind(path, "0", ALLPROP_PROPFIND).await?;
    let code = resp.status().as_u16();
    if code != 207 {
        bail!("propfind {path}: {code}");
    }
    let body = resp.text().await?;
    let rows = xml::parse_multistatus(&body)?;
    xml::entries(&rows)
        .into_iter()
        .next()
        .and_then(|e| e.last_modified)
        .with_context(|| format!("no getlastmodified in PROPFIND response for {path}"))
}

fn inner_statuses(body: &str) -> String {
    match xml::parse_multistatus(body) {
        Ok(rows) if !rows.is_empty() => rows
            .iter()
            .map(|r| {
                format!(
                    "{} -> {}",
                    r.prop,
                    r.propstat_status.as_deref().unwrap_or("?")
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
        _ => "(no multistatus rows)".to_string(),
    }
}

fn print_entry(stat: &Stat, probe: &str, ctx: &str, e: &PropfindEntry) {
    stat.note(
        probe,
        &format!("{ctx} entry"),
        format!(
            "href={} collection={} len={} lastmod={} epoch={}",
            e.href,
            e.is_collection,
            e.content_length
                .map(|l| l.to_string())
                .unwrap_or_else(|| "none".into()),
            e.last_modified.clone().unwrap_or_else(|| "none".into()),
            epoch_of(&e.last_modified.clone().unwrap_or_default())
        ),
    );
}

// ---------------------------------------------------------------------------
// Leg 1: PROPFIND shapes

async fn propfind_shapes(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "propfind";
    let dir = format!("{}/propfind", stat.run_root);
    mkcol_deep(srv, &dir).await?;

    // Empty directory, Depth:1 -> only the self entry.
    let resp = srv.propfind(&format!("{dir}/"), "1", ALLPROP_PROPFIND).await?;
    stat.expect(probe, "depth1-empty code", resp.status().as_u16(), "207");
    let body = resp.text().await?;
    stat.note(probe, "depth1-empty raw head", squash(&body, 200));
    let entries = xml::entries(&xml::parse_multistatus(&body)?);
    stat.expect(probe, "depth1-empty entry-count", entries.len(), "1");
    for e in &entries {
        print_entry(stat, probe, "depth1-empty", e);
    }

    // One file added -> self + 1.
    srv.put_bytes(&format!("{dir}/hello.txt"), b"hello webdav spike")
        .await?;
    let resp = srv.propfind(&format!("{dir}/"), "1", ALLPROP_PROPFIND).await?;
    stat.expect(probe, "depth1-onefile code", resp.status().as_u16(), "207");
    let body = resp.text().await?;
    stat.note(probe, "depth1-onefile raw head", squash(&body, 200));
    let entries = xml::entries(&xml::parse_multistatus(&body)?);
    stat.expect(probe, "depth1-onefile entry-count", entries.len(), "2");
    for e in &entries {
        print_entry(stat, probe, "depth1-onefile", e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 2: mtime write forms (all expected to leave the real mtime alone)

async fn mtime_write(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "mtime";
    let is_rclone = srv.name == "rclone";
    let dir = format!("{}/mtime", stat.run_root);
    mkcol_deep(srv, &dir).await?;
    let f = format!("{dir}/mtime.txt");
    srv.put_bytes(&f, b"mtime-body-v1").await?;
    let baseline = get_lastmod(srv, &f).await?;
    stat.note(
        probe,
        "baseline lastmod",
        format!("{baseline} (epoch {})", epoch_of(&baseline)),
    );

    // Wire form A: dead-property style <lastmodified>epoch</lastmodified>.
    let body_a = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop>"#,
        r#"<D:lastmodified>1735689600</D:lastmodified>"#,
        r#"</D:prop></D:set></D:propertyupdate>"#
    );
    let resp = srv.proppatch(&f, body_a).await?;
    stat.expect(probe, "form-A lastmodified code", resp.status().as_u16(), "207");
    let txt = resp.text().await?;
    let inner = inner_statuses(&txt);
    stat.note(probe, "form-A inner propstat", &inner);
    if is_rclone {
        stat.expect_contains(probe, "form-A inner has", &inner, "403");
    } else {
        stat.expect_contains(probe, "form-A inner has", &inner, "200");
    }
    let after = get_lastmod(srv, &f).await?;
    stat.note(
        probe,
        "form-A lastmod after",
        format!("{after} (epoch {})", epoch_of(&after)),
    );
    stat.expect(
        probe,
        "form-A real mtime changed",
        if epoch_of(&after) == epoch_of(&baseline) {
            "no"
        } else {
            "yes"
        },
        "no",
    );

    // Wire form B: overwrite the live prop <getlastmodified>httpdate</...>.
    let body_b = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop>"#,
        r#"<D:getlastmodified>Thu, 01 Jan 2026 12:00:00 GMT</D:getlastmodified>"#,
        r#"</D:prop></D:set></D:propertyupdate>"#
    );
    let resp = srv.proppatch(&f, body_b).await?;
    stat.expect(probe, "form-B getlastmodified code", resp.status().as_u16(), "207");
    let txt = resp.text().await?;
    let inner = inner_statuses(&txt);
    stat.note(probe, "form-B inner propstat", &inner);
    if is_rclone {
        stat.expect_contains(probe, "form-B inner has", &inner, "403");
    } else {
        stat.expect_contains(probe, "form-B inner has", &inner, "409");
    }
    let after = get_lastmod(srv, &f).await?;
    stat.note(
        probe,
        "form-B lastmod after",
        format!("{after} (epoch {})", epoch_of(&after)),
    );
    stat.expect(
        probe,
        "form-B real mtime changed",
        if epoch_of(&after) == epoch_of(&baseline) {
            "no"
        } else {
            "yes"
        },
        "no",
    );

    // X-OC-Mtime header on PUT (ownCloud-style) — observation only.
    let f2 = format!("{dir}/ocmtime.txt");
    let resp = srv
        .put_bytes_headers(&f2, b"ocmtime-body", &[("x-oc-mtime", "1735689600")])
        .await?;
    stat.note(probe, "x-oc-mtime PUT code", resp.status().as_u16());
    let lm2 = get_lastmod(srv, &f2).await?;
    stat.note(
        probe,
        "x-oc-mtime lastmod",
        format!("{lm2} (epoch {}, target 1735689600)", epoch_of(&lm2)),
    );
    stat.note(
        probe,
        "x-oc-mtime effective",
        if epoch_of(&lm2) == "1735689600" {
            "yes"
        } else {
            "no"
        },
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 3: Range forms

async fn range_forms(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "range";
    let is_rclone = srv.name == "rclone";
    let dir = format!("{}/range", stat.run_root);
    mkcol_deep(srv, &dir).await?;
    let f = format!("{dir}/range.bin");
    let content = b"0123456789".repeat(10); // exactly 100 known bytes
    srv.put_bytes(&f, &content).await?;

    let resp = srv.get_range(&f, "bytes=0-9").await?;
    stat.expect(probe, "bytes=0-9 code", resp.status().as_u16(), "206");
    stat.note(probe, "bytes=0-9 content-range", header_str(&resp, "content-range"));
    let body = resp.bytes().await?;
    stat.note(
        probe,
        "bytes=0-9 body",
        format!("{} bytes: {}", body.len(), String::from_utf8_lossy(&body)),
    );

    let resp = srv.get_range(&f, "bytes=90-999999").await?;
    stat.expect(probe, "bytes=90-999999 code", resp.status().as_u16(), "206");
    stat.note(
        probe,
        "bytes=90-999999 content-range (EOF clamp?)",
        header_str(&resp, "content-range"),
    );
    let body = resp.bytes().await?;
    stat.note(probe, "bytes=90-999999 body-len", body.len());

    let resp = srv.get_range(&f, "bytes=999999-").await?;
    stat.expect(probe, "bytes=999999- code", resp.status().as_u16(), "416");

    let resp = srv.get_range(&f, "bytes=50-10").await?;
    let exp = if is_rclone { "416" } else { "200" };
    stat.expect(probe, "bytes=50-10 inverted code", resp.status().as_u16(), exp);
    let body = resp.bytes().await?;
    stat.note(probe, "bytes=50-10 body-len", body.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 4: MOVE forms

async fn move_forms(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "move";
    let is_rclone = srv.name == "rclone";
    let dir = format!("{}/move", stat.run_root);
    mkcol_deep(srv, &dir).await?;

    // File MOVE onto an existing destination, Overwrite:F.
    let a = format!("{dir}/a.txt");
    let b = format!("{dir}/b.txt");
    srv.put_bytes(&a, b"MOVE-A-CONTENT").await?;
    srv.put_bytes(&b, b"DEST-OLD-CONTENT").await?;
    let dest = srv.url_abs(&b)?;
    let resp = srv.move_(&a, &dest, Some(false)).await?;
    stat.expect(probe, "file Overwrite:F code", resp.status().as_u16(), "412");

    // Overwrite:T -> content transfers, source disappears.
    let resp = srv.move_(&a, &dest, Some(true)).await?;
    stat.expect(probe, "file Overwrite:T code", resp.status().as_u16(), "204");
    let resp = srv.get(&b).await?;
    let got = resp.bytes().await?;
    stat.expect(
        probe,
        "Overwrite:T dest content",
        String::from_utf8_lossy(&got),
        "MOVE-A-CONTENT",
    );
    stat.expect(probe, "Overwrite:T source gone", propfind_status(srv, &a).await?, "404");

    // Collection MOVE with trailing slashes on both sides.
    mkcol_deep(srv, &format!("{dir}/dir1")).await?;
    srv.put_bytes(&format!("{dir}/dir1/inner.txt"), b"dir-inner").await?;
    let dest = srv.url_abs(&format!("{dir}/dir2/"))?;
    let resp = srv.move_(&format!("{dir}/dir1/"), &dest, None).await?;
    stat.expect(probe, "dir trailing-slash code", resp.status().as_u16(), "201");
    stat.expect(
        probe,
        "dir moved content present",
        propfind_status(srv, &format!("{dir}/dir2/inner.txt")).await?,
        "207",
    );

    // Contrast: collection source WITHOUT trailing slash — apache answers
    // 301 (canonical redirect) and does NOT execute; rclone unpinned.
    mkcol_deep(srv, &format!("{dir}/dir3")).await?;
    srv.put_bytes(&format!("{dir}/dir3/inner.txt"), b"dir3-inner").await?;
    let dest = srv.url_abs(&format!("{dir}/dir4/"))?;
    let resp = srv.move_(&format!("{dir}/dir3"), &dest, None).await?;
    let code = resp.status().as_u16();
    if is_rclone {
        stat.note(probe, "dir no-slash code (contrast)", code);
    } else {
        stat.expect(probe, "dir no-slash code (apache: 301, not executed)", code, "301");
    }
    stat.note(probe, "dir no-slash location", header_str(&resp, "location"));
    let d4 = propfind_status(srv, &format!("{dir}/dir4/")).await?;
    if is_rclone {
        stat.note(probe, "dir4 present (contrast)", d4);
    } else {
        stat.expect(probe, "dir4 absent (move not executed)", d4, "404");
    }
    stat.note(
        probe,
        "dir3 present after contrast",
        propfind_status(srv, &format!("{dir}/dir3/")).await?,
    );

    // MOVE into a missing parent collection.
    let m = format!("{dir}/m.txt");
    srv.put_bytes(&m, b"missing-parent").await?;
    let dest = srv.url_abs(&format!("{dir}/missing-parent/m.txt"))?;
    let resp = srv.move_(&m, &dest, None).await?;
    let exp = if is_rclone { "403" } else { "500" };
    stat.expect(probe, "missing-parent dest code", resp.status().as_u16(), exp);

    // rclone only: path-only (relative) Destination form.
    if is_rclone {
        let r = format!("{dir}/rel.txt");
        srv.put_bytes(&r, b"relative-dest").await?;
        let rel_dest = format!("/{dir}/rel2.txt");
        let resp = srv.move_(&r, &rel_dest, None).await?;
        stat.expect(probe, "relative Destination code", resp.status().as_u16(), "201");
        stat.expect(
            probe,
            "relative dest moved present",
            propfind_status(srv, &format!("{dir}/rel2.txt")).await?,
            "207",
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 5: MKCOL forms

async fn mkcol_forms(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "mkcol";
    let is_rclone = srv.name == "rclone";
    let dir = format!("{}/mkcol", stat.run_root);
    mkcol_deep(srv, &dir).await?;

    // MKCOL on an EXISTING directory (trailing-slash URL):
    // rclone answers 201 (idempotent-ish quirk), apache 405.
    let resp = srv.mkcol(&format!("{dir}/")).await?;
    let exp = if is_rclone { "201" } else { "405" };
    stat.expect(probe, "existing-dir code", resp.status().as_u16(), exp);

    // MKCOL with a missing parent collection.
    let resp = srv.mkcol(&format!("{dir}/missing-parent/sub/")).await?;
    stat.expect(probe, "missing-parent code", resp.status().as_u16(), "409");
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 6: DELETE forms

async fn delete_forms(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "delete";
    let dir = format!("{}/delete", stat.run_root);
    mkcol_deep(srv, &dir).await?;

    let f = format!("{dir}/del.txt");
    srv.put_bytes(&f, b"delete-me").await?;
    let resp = srv.delete(&f).await?;
    stat.expect(probe, "file delete code", resp.status().as_u16(), "204");
    let resp = srv.delete(&f).await?;
    stat.expect(probe, "file re-delete code", resp.status().as_u16(), "404");

    mkcol_deep(srv, &format!("{dir}/deldir")).await?;
    srv.put_bytes(&format!("{dir}/deldir/inner.txt"), b"inner").await?;
    let resp = srv.delete(&format!("{dir}/deldir/")).await?;
    stat.expect(probe, "dir-with-children delete code", resp.status().as_u16(), "204");
    stat.expect(
        probe,
        "dir gone (recursive)",
        propfind_status(srv, &format!("{dir}/deldir/")).await?,
        "404",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 7: hand-rolled Digest chain (apache only; bare reqwest)

async fn send_bare(
    http: &reqwest::Client,
    method: &reqwest::Method,
    url: &str,
    authz: &str,
) -> Result<reqwest::Response> {
    Ok(http
        .request(method.clone(), url)
        .header("authorization", authz)
        .header("depth", "0")
        .header("content-type", "text/xml; charset=utf-8")
        .body(ALLPROP_PROPFIND)
        .send()
        .await?)
}

async fn digest_chain(cfg: &Config, stat: &mut Stat) -> Result<()> {
    let probe = "digest";
    let http = crate::client::bare_client()?;
    let method = reqwest::Method::from_bytes(b"PROPFIND")?;
    let base = reqwest::Url::parse(&cfg.apache_url)?;
    let uri = crate::client::request_uri(&base);

    // (a) unauthenticated request -> 401 + raw challenge.
    let resp = http
        .request(method.clone(), base.as_str())
        .header("depth", "0")
        .header("content-type", "text/xml; charset=utf-8")
        .body(ALLPROP_PROPFIND)
        .send()
        .await?;
    stat.expect(probe, "(a) unauth code", resp.status().as_u16(), "401");
    let www = header_str(&resp, "www-authenticate");
    stat.note(probe, "(a) www-authenticate", &www);

    // (b) parse the challenge (quote-aware).
    let ch = crate::digest::parse_challenge(&www)
        .with_context(|| "parse challenge from (a)")?;
    stat.note(probe, "(b) parsed realm", &ch.realm);
    stat.note(probe, "(b) parsed nonce", &ch.nonce);
    stat.note(probe, "(b) parsed algorithm", ch.algorithm.as_deref().unwrap_or("(none)"));
    stat.note(probe, "(b) parsed qop", ch.qop.as_deref().unwrap_or("(none)"));

    // (c) compute the response, (d) send once.
    let mut sess = crate::digest::DigestSession::from_challenge(&ch);
    sess.nc = 1;
    let authz = crate::digest::authorization_header(
        &cfg.user,
        &cfg.pass,
        "PROPFIND",
        &uri,
        &sess,
        &crate::digest::rand_cnonce(),
    );
    stat.note(probe, "(c) authorization", &authz);
    let resp = send_bare(&http, &method, &cfg.apache_url, &authz).await?;
    stat.expect(probe, "(d) authed code", resp.status().as_u16(), "207");
    let _ = resp.bytes().await; // drain

    // (e) same nonce, nc incremented (apache does not check nc replay).
    sess.nc = 2;
    let authz2 = crate::digest::authorization_header(
        &cfg.user,
        &cfg.pass,
        "PROPFIND",
        &uri,
        &sess,
        &crate::digest::rand_cnonce(),
    );
    stat.note(probe, "(e) authorization nc=2", &authz2);
    let resp = send_bare(&http, &method, &cfg.apache_url, &authz2).await?;
    stat.expect(probe, "(e) nc-increment code", resp.status().as_u16(), "207");
    let _ = resp.bytes().await;

    // (f) stale fixture: nonce expired after AuthDigestNonceLifetime=2s.
    let stale_base = reqwest::Url::parse(&cfg.apache_stale_url)?;
    let stale_uri = crate::client::request_uri(&stale_base);
    let resp = http
        .request(method.clone(), stale_base.as_str())
        .header("depth", "0")
        .body(ALLPROP_PROPFIND)
        .send()
        .await?;
    let www1 = header_str(&resp, "www-authenticate");
    let ch1 = crate::digest::parse_challenge(&www1)
        .with_context(|| "parse fresh challenge from stale fixture")?;
    stat.note(probe, "(f) stale-fixture fresh nonce", &ch1.nonce);

    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut sess1 = crate::digest::DigestSession::from_challenge(&ch1);
    sess1.nc = 1;
    let authz_old = crate::digest::authorization_header(
        &cfg.user,
        &cfg.pass,
        "PROPFIND",
        &stale_uri,
        &sess1,
        &crate::digest::rand_cnonce(),
    );
    let resp = send_bare(&http, &method, &cfg.apache_stale_url, &authz_old).await?;
    stat.expect(probe, "(f) expired-nonce code", resp.status().as_u16(), "401");
    let www2 = header_str(&resp, "www-authenticate");
    stat.note(probe, "(f) www-authenticate", &www2);
    let ch2 = crate::digest::parse_challenge(&www2)
        .with_context(|| "parse stale=true challenge")?;
    stat.expect(probe, "(f) stale flag", ch2.stale, "true");

    // Recompute with the new nonce and resend EXACTLY once.
    let mut sess2 = crate::digest::DigestSession::from_challenge(&ch2);
    sess2.nc = 1;
    let authz_new = crate::digest::authorization_header(
        &cfg.user,
        &cfg.pass,
        "PROPFIND",
        &stale_uri,
        &sess2,
        &crate::digest::rand_cnonce(),
    );
    stat.note(probe, "(f) authorization (new nonce)", &authz_new);
    let resp = send_bare(&http, &method, &cfg.apache_stale_url, &authz_new).await?;
    stat.expect(probe, "(f) renegotiated code", resp.status().as_u16(), "207");
    let _ = resp.bytes().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 8: chunked PUT (streaming body, no Content-Length)

async fn chunked_put(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "chunked";
    let dir = format!("{}/chunked", stat.run_root);
    mkcol_deep(srv, &dir).await?;
    if !srv.basic {
        // A 401 cannot replay a stream: make sure the digest session is
        // established before streaming.
        let _ = srv.propfind("", "0", ALLPROP_PROPFIND).await?;
    }

    const TOTAL: usize = 102_400; // 100 KiB
    let mut data = vec![0u8; TOTAL];
    rand::thread_rng().fill_bytes(&mut data);
    // Item type is Result so this is a TryStream (what Body::wrap_stream
    // requires); the producer never errors. tokio's mpsc Receiver does not
    // impl futures Stream (that lives in tokio-stream), so hand-roll it
    // over Receiver::poll_recv.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(4);
    let producer = tokio::spawn(async move {
        for chunk in data.chunks(8192) {
            if tx.send(Ok(chunk.to_vec())).await.is_err() {
                break;
            }
        }
    });
    struct ChanStream {
        rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
    }
    impl futures_core::Stream for ChanStream {
        type Item = Result<Vec<u8>, std::io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::pin::Pin::new(&mut self.rx).poll_recv(cx)
        }
    }
    let body = reqwest::Body::wrap_stream(ChanStream { rx });

    let f = format!("{dir}/chunked.bin");
    let resp = srv.put_stream(&f, body).await?;
    let _ = producer.await;
    stat.expect(probe, "chunked PUT code (no Content-Length)", resp.status().as_u16(), "201");
    let _ = resp.bytes().await;

    let resp = srv.propfind(&f, "0", ALLPROP_PROPFIND).await?;
    let txt = resp.text().await?;
    let rows = xml::parse_multistatus(&txt)?;
    let len = xml::entries(&rows)
        .into_iter()
        .next()
        .and_then(|e| e.content_length);
    stat.expect(
        probe,
        "getcontentlength after chunked PUT",
        len.map(|l| l.to_string())
            .unwrap_or_else(|| "none".into()),
        "102400",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 9: quota props

async fn quota_props(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "quota";
    let body = concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
        r#"<D:quota-used-bytes/><D:quota-available-bytes/>"#,
        r#"</D:prop></D:propfind>"#
    );
    let target = format!("{}/", stat.run_root);
    let resp = srv.propfind(&target, "0", body).await?;
    stat.expect(probe, "code", resp.status().as_u16(), "207");
    let txt = resp.text().await?;
    let rows = xml::parse_multistatus(&txt)?;
    for prop in ["quota-used-bytes", "quota-available-bytes"] {
        let found = rows
            .iter()
            .filter(|r| r.prop == prop)
            .map(|r| r.propstat_status.clone().unwrap_or_else(|| "?".into()))
            .collect::<Vec<_>>()
            .join(" | ");
        let obs = if found.is_empty() {
            "(absent)".to_string()
        } else {
            found
        };
        stat.expect_contains(probe, &format!("{prop} propstat"), obs, "404");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Leg 10: run-root cleanup

async fn cleanup_run_root(stat: &mut Stat, srv: &mut DavServer) -> Result<()> {
    let probe = "cleanup";
    let resp = srv.delete(&format!("{}/", stat.run_root)).await?;
    stat.expect(probe, "delete run-root code", resp.status().as_u16(), "204");
    let resp = srv
        .propfind(&format!("{}/", stat.run_root), "0", ALLPROP_PROPFIND)
        .await?;
    stat.expect(probe, "run-root gone", resp.status().as_u16(), "404");
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry points

pub async fn cmd_matrix() -> Result<i32> {
    let cfg = Config::from_env()?;
    let run_root = format!("wd0-{:06x}", rand::random::<u32>() & 0x00ff_ffff);
    println!("== webdav-spike matrix — run-root {run_root} ==");
    let mut total = 0u32;

    for (name, url, basic) in [
        ("rclone", cfg.rclone_url.clone(), true),
        ("apache", cfg.apache_url.clone(), false),
    ] {
        let mut srv = DavServer::new(name, &url, &cfg.user, &cfg.pass, basic)?;

        // Preflight OPTIONS (rclone allows anonymous; apache negotiates).
        let resp = srv.options().await?;
        let code = resp.status().as_u16();
        println!(
            "[{name}] preflight OPTIONS: {code} (allow: {})",
            header_str(&resp, "allow")
        );
        if !(200..300).contains(&code) {
            bail!("preflight OPTIONS on {url} -> {code}; is the fixture up?");
        }

        let mut stat = Stat::new(name, &run_root);
        leg!(stat, srv, "propfind_shapes", propfind_shapes);
        leg!(stat, srv, "mtime_write", mtime_write);
        leg!(stat, srv, "range_forms", range_forms);
        leg!(stat, srv, "move_forms", move_forms);
        leg!(stat, srv, "mkcol_forms", mkcol_forms);
        leg!(stat, srv, "delete_forms", delete_forms);
        if !basic {
            if let Err(e) = digest_chain(&cfg, &mut stat).await {
                println!("[apache] digest: LEG ERROR: {e:#}");
                stat.leg_errors += 1;
            }
        }
        leg!(stat, srv, "chunked_put", chunked_put);
        leg!(stat, srv, "quota_props", quota_props);
        leg!(stat, srv, "cleanup_run_root", cleanup_run_root);
        total += stat.finish();
    }

    println!("== matrix done — {total} mismatch(es)/leg error(s) total ==");
    Ok(i32::from(total != 0))
}

pub async fn cmd_cleanup() -> Result<i32> {
    let cfg = Config::from_env()?;
    for (name, url, basic) in [
        ("rclone", cfg.rclone_url.as_str(), true),
        ("apache", cfg.apache_url.as_str(), false),
    ] {
        let mut srv = DavServer::new(name, url, &cfg.user, &cfg.pass, basic)?;
        let resp = srv.propfind("", "1", ALLPROP_PROPFIND).await?;
        let code = resp.status().as_u16();
        if code != 207 {
            println!("[{name}] cleanup: root PROPFIND {code} — nothing to do");
            continue;
        }
        let body = resp.text().await?;
        let rows = xml::parse_multistatus(&body)?;
        let mut swept = 0u32;
        for e in xml::entries(&rows) {
            let Some(path) = srv.path_from_href(&e.href) else {
                continue;
            };
            let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            if !last.starts_with("wd0-") {
                continue;
            }
            let target = if e.is_collection {
                format!("{path}/")
            } else {
                path
            };
            let resp = srv.delete(&target).await?;
            println!("[{name}] cleanup delete {target}: {}", resp.status().as_u16());
            swept += 1;
        }
        println!("[{name}] cleanup: {swept} leftover item(s) swept");
    }
    println!("(apache-stale fixture is read-only for this spike — nothing written, nothing to sweep)");
    Ok(0)
}
