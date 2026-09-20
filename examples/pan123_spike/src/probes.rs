//! Probe commands — 123-0 read-path legs. Each command prints raw
//! (redacted) evidence lines; callers capture stdout as the sample record.

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};

use crate::api::{self, Spike};
use crate::state::{self, Paths, Tokens};

// ---------------------------------------------------------------- helpers

/// Find the first `href='...'` or `href="..."` http(s) link in an HTML body
/// (123panNextGen resolve level 2, widened to the whole body + both quote
/// styles — their [:500] window missed the spike's interstitial page).
fn extract_href(html: &str) -> Option<String> {
    for marker in ["href='", "href=\""] {
        if let Some(pos) = html.find(marker) {
            let start = pos + marker.len();
            let rest = &html[start..];
            let end = marker
                .ends_with('\'')
                .then(|| rest.find('\''))
                .flatten()
                .or_else(|| rest.find('"'));
            if let Some(end) = end {
                let url = &rest[..end];
                if url.starts_with("http") {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

/// Decode `params=<base64>` from a `/download-v2/` style URL (123panNextGen
/// resolve level 3 — plain self-decode, no auto_redirect injection).
pub(crate) fn decode_download_v2_params(url: &str) -> Option<String> {
    if !url.contains("/download-v2/") {
        return None;
    }
    let params = url.split("params=").nth(1)?.split('&').next()?;
    // url-safe base64 + missing padding
    let std = params.replace('-', "+").replace('_', "/");
    let padded = match std.len() % 4 {
        2 => format!("{std}=="),
        3 => format!("{std}="),
        _ => std,
    };
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    if s.starts_with("http") {
        Some(s)
    } else {
        None
    }
}

fn spike_with_tokens() -> Result<(Paths, Spike)> {
    let paths = Paths::from_env();
    let tokens =
        state::load_tokens(&paths).context("need pan123-tokens.json (run `sign-in` first)")?;
    let spike = Spike::build(Some(tokens))?;
    Ok((paths, spike))
}

fn bare_spike() -> Result<(Paths, Spike)> {
    Ok((Paths::from_env(), Spike::build(None)?))
}

/// GET list params in the 123panNextGen current shape.
pub(crate) fn list_query(
    parent: i64,
    page: u32,
    limit: u32,
    trashed: bool,
) -> Vec<(String, String)> {
    vec![
        ("driveId".into(), "0".into()),
        ("limit".into(), limit.to_string()),
        ("next".into(), "0".into()),
        ("orderBy".into(), "file_id".into()),
        ("orderDirection".into(), "desc".into()),
        ("parentFileId".into(), parent.to_string()),
        ("trashed".into(), trashed.to_string().to_lowercase()),
        ("SearchData".into(), String::new()),
        ("Page".into(), page.to_string()),
        ("OnlyLookAbnormalFile".into(), "0".into()),
    ]
}

/// Extract the `data.InfoList` (dual-cased) file rows from a list envelope.
pub(crate) fn info_list_of(body: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let data = v
                .get("data")?
                .get("InfoList")
                .or_else(|| v.get("data")?.get("infoList"))?;
            data.as_array().cloned()
        })
        .unwrap_or_default()
}

fn field<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let obj = item.as_object()?;
    keys.iter().find_map(|k| obj.get(*k))
}

pub(crate) fn field_i64(item: &Value, keys: &[&str]) -> Option<i64> {
    field(item, keys).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

pub(crate) fn field_str<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a str> {
    field(item, keys).and_then(|v| v.as_str())
}

pub(crate) async fn find_by_name(spike: &Spike, parent: i64, name: &str) -> Result<Option<Value>> {
    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let r = spike
        .api_get(&url, &list_query(parent, 1, 100, false))
        .await?;
    let (code, msg) = api::code_of(&r.body);
    if code != Some(0) {
        bail!("list for find_by_name failed: code={code:?} msg={msg}");
    }
    Ok(info_list_of(&r.body)
        .into_iter()
        .find(|it| field_str(it, &["FileName", "fileName"]) == Some(name)))
}

async fn trash_correct(spike: &Spike, fid: i64, new_gen: bool) -> Result<api::Resp> {
    let path = if new_gen {
        "/a/api/file/trash"
    } else {
        "/b/api/file/trash"
    };
    let url = format!("{}{path}", api::PRIMARY_BASE);
    // Minimal exact shape (plan §5.11 / task spec): uppercase FileId inside
    // fileTrashInfoList, event intoRecycle.
    let body = json!({
        "driveId": 0,
        "fileTrashInfoList": [{"FileId": fid}],
        "operation": true,
        "event": "intoRecycle",
    });
    spike.api_post_json(&url, &body, &[]).await
}

pub(crate) fn print_item(tag: &str, item: &Value) {
    println!(
        "[{tag}] FileId={:?} FileName={:?} Type={:?} Size={:?} UpdateAt={:?} CreateAt={:?} keys={}",
        field_i64(item, &["FileId", "fileId"]),
        field_str(item, &["FileName", "fileName"]),
        field_i64(item, &["Type", "type"]),
        field_i64(item, &["Size", "size"]),
        field(item, &["UpdateAt", "updateAt"])
            .map(|v| v.to_string())
            .unwrap_or_default(),
        field(item, &["CreateAt", "createAt"])
            .map(|v| v.to_string())
            .unwrap_or_default(),
        item.as_object()
            .map(|m| m.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default(),
    );
}

/// Simple hand-rolled flag parser: `--flag value` pairs, everything else
/// positional.
pub(crate) struct Args<'a> {
    rest: &'a [String],
}

impl<'a> Args<'a> {
    pub(crate) fn new(rest: &'a [String]) -> Self {
        Args { rest }
    }
    pub(crate) fn flag(&self, name: &str) -> Option<String> {
        let mut it = self.rest.iter();
        while let Some(a) = it.next() {
            if a == name {
                return it.next().cloned();
            }
        }
        None
    }
    pub(crate) fn positional(&self) -> Vec<&str> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.rest.len() {
            if self.rest[i].starts_with("--") {
                i += 2; // skip flag + value
            } else {
                out.push(self.rest[i].as_str());
                i += 1;
            }
        }
        out
    }
}

// ---------------------------------------------------------------- sign-in

/// Password login (123panNextGen current form): POST
/// `{PRIMARY|FALLBACK}/b/api/user/sign_in` `{"type":1,"passport",...}`,
/// success envelope is code==200 (NOT 0). Token persisted dual-head usable.
pub async fn cmd_sign_in(rest: &[String]) -> Result<i32> {
    let _ = Args::new(rest);
    let paths = Paths::from_env();
    let account = state::load_account(&paths)?;
    let spike = Spike::build(None)?;

    let body = json!({
        "type": 1,
        "passport": account.passport,
        "password": account.password,
    });
    let path = "/b/api/user/sign_in";

    let mut resp = None;
    for (label, base) in [
        ("primary", api::PRIMARY_BASE),
        ("fallback", api::FALLBACK_BASE),
    ] {
        let url = format!("{base}{path}");
        let mut last_err = None;
        for attempt in 1..=3u32 {
            match spike.api_post_json(&url, &body, &[]).await {
                Ok(r) => {
                    resp = Some((label, r));
                    break;
                }
                Err(e) => {
                    // transport error only — retry with discipline (<=3, >=2s)
                    eprintln!("[sign-in] {label} attempt {attempt} transport error: {e:#}");
                    last_err = Some(e);
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }
        if resp.is_some() {
            break;
        }
        if let Some(e) = last_err {
            eprintln!("[sign-in] {label} exhausted: {e:#}");
        }
    }
    let Some((label, r)) = resp else {
        bail!("sign-in unreachable on both domains");
    };
    println!("[sign-in] domain={label} url={} ", r.url);
    println!("[sign-in] {}", api::summarize(&r));
    println!("         body={}", api::redact(&r.body));

    let (code, msg) = api::code_of(&r.body);
    if code != Some(200) {
        println!("[SUMMARY] sign-in|ok=0|code={code:?}|msg={msg:?}");
        return Ok(1);
    }
    let token = serde_json::from_str::<Value>(&r.body)?
        .get("data")
        .and_then(|d| d.get("token"))
        .and_then(|t| t.as_str())
        .context("code==200 but data.token missing")?
        .to_string();

    let tokens = Tokens {
        token: token.clone(),
        login_uuid: spike.login_uuid.clone(),
        obtained_unix: state::now_unix(),
        user_info: Value::Null,
    };
    state::write_json_atomic(&paths.tokens(), &tokens)?;
    println!(
        "[sign-in] token persisted (masked={}) login_uuid={}",
        state::mask(&token),
        state::mask(&spike.login_uuid)
    );

    // Token liveness probe: lightest endpoints, both generations of list.
    let spike = Spike::build(Some(tokens))?;
    let probes: [(&str, &str); 2] = [
        (
            "liveness:list-new",
            &format!("{}/api/file/list/new", api::PRIMARY_BASE),
        ),
        (
            "liveness:list-old",
            &format!("{}/b/api/file/list/new", api::PRIMARY_BASE),
        ),
    ];
    for (tag, url) in probes {
        let mut q = list_query(0, 1, 1, false);
        match spike.api_get(url, &q).await {
            Ok(r) => {
                q.clear();
                println!("[{tag}] {}", api::summarize(&r));
            }
            Err(e) => eprintln!("[{tag}] transport error: {e:#}"),
        }
    }
    println!(
        "[SUMMARY] sign-in|ok=1|domain={label}|token={}",
        state::mask(&token)
    );
    Ok(0)
}

// ------------------------------------------------------------ probe-dydomain

/// `/api/dydomain` dynamic domain discovery (pan123-rs startup call):
/// still alive? which domains does it hand out?
pub async fn cmd_probe_dydomain(rest: &[String]) -> Result<i32> {
    let _ = Args::new(rest);
    let (_, spike) = bare_spike()?;
    for (label, base) in [
        ("login-base", api::LOGIN_BASE),
        ("primary", api::PRIMARY_BASE),
        ("fallback", api::FALLBACK_BASE),
    ] {
        let url = format!("{base}/api/dydomain");
        match spike.api_get(&url, &[]).await {
            Ok(r) => api::show(&format!("dydomain:{label}"), &r),
            Err(e) => eprintln!("[dydomain:{label}] transport error: {e:#}"),
        }
    }
    Ok(0)
}

// ------------------------------------------------------------ probe-matrix

/// Endpoint generation matrix (plan §3.1 rows): new vs old path shapes, on
/// both domains, no-signature form. Mutations use harmless payloads only.
pub async fn cmd_probe_matrix(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let fid: i64 = args.flag("--fid").and_then(|v| v.parse().ok()).unwrap_or(0);
    let (_paths, spike) = spike_with_tokens()?;

    struct Row {
        tag: &'static str,
        method: &'static str,
        path: &'static str,
        body: Option<Value>,
        query: Option<Vec<(String, String)>>,
        bases: &'static [(&'static str, &'static str)],
    }
    let both: &[(&str, &str)] = &[("www", api::PRIMARY_BASE), ("api278", api::FALLBACK_BASE)];
    let www_only: &[(&str, &str)] = &[("www", api::PRIMARY_BASE)];

    let mut rows: Vec<Row> = vec![
        Row {
            tag: "list-new",
            method: "GET",
            path: "/api/file/list/new",
            body: None,
            query: Some(list_query(0, 1, 2, false)),
            bases: www_only,
        },
        Row {
            tag: "list-old",
            method: "GET",
            path: "/b/api/file/list/new",
            body: None,
            query: Some(list_query(0, 1, 2, false)),
            bases: both,
        },
        Row {
            tag: "stat-info(/b/)",
            method: "POST",
            path: "/b/api/file/info",
            body: Some(json!({"fileIdList": [{"fileId": fid}]})),
            query: None,
            bases: both,
        },
        Row {
            tag: "user-info",
            method: "GET",
            path: "/b/api/user/info",
            body: None,
            query: None,
            bases: both,
        },
        Row {
            tag: "quota-report",
            method: "GET",
            path: "/b/api/restful/goapi/v1/user/report/info",
            body: None,
            query: None,
            bases: both,
        },
        Row {
            tag: "traffic-check(empty)",
            method: "POST",
            path: "/b/api/file/download/traffic/check",
            body: Some(json!({"fids": []})),
            query: None,
            bases: both,
        },
        Row {
            tag: "trash-new-empty(/a/)",
            method: "POST",
            path: "/a/api/file/trash",
            body: Some(json!({"driveId": 0, "fileTrashInfoList": [], "operation": true})),
            query: None,
            bases: www_only,
        },
        Row {
            tag: "trash-old-empty(/b/)",
            method: "POST",
            path: "/b/api/file/trash",
            body: Some(json!({"driveId": 0, "fileTrashInfoList": [], "operation": true})),
            query: None,
            bases: www_only,
        },
        Row {
            tag: "rename-new(/a/)",
            method: "POST",
            path: "/a/api/file/rename",
            body: Some(json!({"driveId": 0, "fileId": 0, "fileName": "x"})),
            query: None,
            bases: www_only,
        },
        Row {
            tag: "rename-old(/b/)",
            method: "POST",
            path: "/b/api/file/rename",
            body: Some(json!({"driveId": 0, "fileId": 0, "fileName": "x"})),
            query: None,
            bases: www_only,
        },
    ];

    if fid > 0 {
        // download_info generations need a real file's metadata.
        let info_url = format!("{}/b/api/file/info", api::PRIMARY_BASE);
        let info = spike
            .api_post_json(&info_url, &json!({"fileIdList": [{"fileId": fid}]}), &[])
            .await?;
        let (c, m) = api::code_of(&info.body);
        if c == Some(0) {
            if let Some(item) = info_list_of(&format!(
                "{{\"data\":{{\"InfoList\":{}}}}}",
                serde_json::to_string(
                    &serde_json::from_str::<Value>(&info.body)?
                        .get("data")
                        .cloned()
                        .unwrap_or(Value::Null)
                )
                .unwrap_or_default()
            ))
            .first()
            .cloned()
            .or_else(|| {
                serde_json::from_str::<Value>(&info.body)
                    .ok()?
                    .get("data")?
                    .get("InfoList")?
                    .as_array()?
                    .first()
                    .cloned()
            }) {
                let dl_body = json!({
                    "driveId": 0,
                    "etag": field_str(&item, &["Etag", "etag"]).unwrap_or(""),
                    "fileId": fid,
                    "s3keyFlag": field_str(&item, &["S3KeyFlag", "s3keyFlag"]).unwrap_or(""),
                    "type": field_i64(&item, &["Type", "type"]).unwrap_or(0),
                    "fileName": field_str(&item, &["FileName", "fileName"]).unwrap_or(""),
                    "size": field_i64(&item, &["Size", "size"]).unwrap_or(0),
                });
                rows.push(Row {
                    tag: "download-info-new(/a/)",
                    method: "POST",
                    path: "/a/api/file/download_info",
                    body: Some(dl_body.clone()),
                    query: None,
                    bases: www_only,
                });
                rows.push(Row {
                    tag: "download-info-old(/b/v2/)",
                    method: "POST",
                    path: "/b/api/v2/file/download_info",
                    body: Some(dl_body),
                    query: None,
                    bases: www_only,
                });
            }
        } else {
            eprintln!(
                "[matrix] /b/api/file/info failed code={c:?} msg={m} — skipping download_info rows"
            );
        }
    } else {
        eprintln!("[matrix] no --fid given — download_info rows skipped (run probe-download)");
    }

    for row in rows {
        for (dlabel, base) in row.bases {
            let url = format!("{}{}", base, row.path);
            let res = match row.method {
                "GET" => {
                    spike
                        .api_get(&url, row.query.as_deref().unwrap_or(&[]))
                        .await
                }
                _ => {
                    spike
                        .api_post_json(&url, row.body.as_ref().unwrap(), &[])
                        .await
                }
            };
            match res {
                Ok(r) => println!(
                    "[matrix] {} {} {} {} -> {}",
                    row.tag,
                    dlabel,
                    row.method,
                    row.path,
                    api::summarize(&r)
                ),
                Err(e) => eprintln!(
                    "[matrix] {} {dlabel} {} {} -> transport error: {e:#}",
                    row.tag, row.method, row.path
                ),
            }
        }
    }

    // Signature question: same list-new call with vs without dynamic_params.
    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let q = list_query(0, 1, 2, false);
    match spike.api_get(&url, &q).await {
        Ok(r) => println!("[matrix] sign=absent list-new -> {}", api::summarize(&r)),
        Err(e) => eprintln!("[matrix] sign=absent transport error: {e:#}"),
    }
    let mut qs = q.clone();
    qs.extend(api::dynamic_params());
    match spike.api_get(&url, &qs).await {
        Ok(r) => println!(
            "[matrix] sign=random-key list-new -> {}",
            api::summarize(&r)
        ),
        Err(e) => eprintln!("[matrix] sign=random-key transport error: {e:#}"),
    }
    Ok(0)
}

// --------------------------------------------------------------- probe-qr

/// QR no-human leg: generate + poll result a couple of times to capture the
/// loginStatus envelope. Confirmation (code==200 + token) requires a human
/// to scan — left to 123-4.
pub async fn cmd_probe_qr(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let polls: u32 = args
        .flag("--polls")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let (_, spike) = bare_spike()?;

    let gen_url = format!("{}/api/user/qr-code/generate", api::LOGIN_BASE);
    let r = spike.qr_get(&gen_url, &[]).await?;
    api::show("qr:generate", &r);
    let (code, msg) = api::code_of(&r.body);
    if code != Some(0) {
        println!("[SUMMARY] qr-generate|ok=0|code={code:?}|msg={msg:?}");
        return Ok(1);
    }
    let v = serde_json::from_str::<Value>(&r.body)?;
    let uni_id = v
        .get("data")
        .and_then(|d| d.get("uniID").or_else(|| d.get("uniId")))
        .and_then(|u| u.as_str())
        .context("code==0 but uniID missing")?
        .to_string();
    let qr_url = v
        .get("data")
        .and_then(|d| d.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    println!(
        "[qr] uniID={:?} url={:?}",
        state::mask(&uni_id),
        api::truncate(&qr_url, 200)
    );

    for i in 1..=polls {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let res_url = format!("{}/api/user/qr-code/result", api::LOGIN_BASE);
        let q = vec![("uniID".to_string(), uni_id.clone())];
        match spike.qr_get(&res_url, &q).await {
            Ok(r) => {
                api::show(&format!("qr:poll#{i}"), &r);
                let login_status = serde_json::from_str::<Value>(&r.body).ok().and_then(|v| {
                    v.get("data")
                        .and_then(|d| d.get("loginStatus"))
                        .and_then(|s| s.as_i64())
                });
                println!("[qr] poll#{i} loginStatus={login_status:?}");
            }
            Err(e) => eprintln!("[qr:poll#{i}] transport error: {e:#}"),
        }
    }

    // Third endpoint of the trio: wx_code (no human scanned, so no wxCode is
    // expected — the error/empty form itself is the evidence).
    let wx_url = format!("{}/api/user/qr-code/wx_code", api::LOGIN_BASE);
    match spike.qr_post_json(&wx_url, &json!({"uniID": uni_id})).await {
        Ok(r) => api::show("qr:wx_code(no-scan)", &r),
        Err(e) => eprintln!("[qr:wx_code] transport error: {e:#}"),
    }
    println!(
        "[SUMMARY] qr-no-human|ok=1|note=confirmation(code==200+token) needs a human scan — deferred to 123-4 setup wizard"
    );
    Ok(0)
}

// ------------------------------------------------------------- probe-list

/// List envelope + pagination evidence: Page 1 vs Page 2 (different ids =
/// Page works), field casing, time field forms.
pub async fn cmd_probe_list(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: u32 = args
        .flag("--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let trashed = args.flag("--trashed").is_some();
    let (_paths, spike) = spike_with_tokens()?;

    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let mut first_ids = Vec::new();
    for page in [1u32, 2] {
        let r = spike
            .api_get(&url, &list_query(parent, page, limit, trashed))
            .await?;
        api::show(&format!("list:page{page}"), &r);
        let items = info_list_of(&r.body);
        let ids: Vec<String> = items
            .iter()
            .map(|it| {
                field_i64(it, &["FileId", "fileId"])
                    .unwrap_or(-1)
                    .to_string()
            })
            .collect();
        println!(
            "[list] page={page} count={} ids=[{}] total={:?}",
            items.len(),
            ids.join(","),
            serde_json::from_str::<Value>(&r.body).ok().and_then(|v| v
                .get("data")?
                .get("Total")
                .or_else(|| v.get("data")?.get("total"))
                .cloned())
        );
        if page == 1 {
            for it in items.iter().take(3) {
                print_item(&format!("list:page{page}:item"), it);
            }
            first_ids = ids;
        } else if !first_ids.is_empty() {
            let overlap = first_ids.iter().filter(|i| ids.contains(i)).count();
            println!(
                "[list] pagination check: page2 overlaps page1 in {overlap}/{} ids (0 overlap = Page param works)"
            , first_ids.len());
        }
    }
    Ok(0)
}

// ------------------------------------------------------------- probe-mkdir

/// Create a test dir (new /a/ generation first, old /b/ fallback), then
/// repeat the exact create WITHOUT duplicate to sample the 5060 body.
pub async fn cmd_probe_mkdir(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(name) = pos.first() else {
        eprintln!("usage: probe-mkdir <name> [--parent X]");
        return Ok(2);
    };
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let (_paths, spike) = spike_with_tokens()?;

    // pan123-rs upload shape: no duplicate on first request (so the second
    // request can sample the bare conflict form).
    let body = json!({
        "driveId": 0,
        "etag": "",
        "fileName": name,
        "parentFileId": parent,
        "size": 0,
        "type": 1,
        "NotReuse": true,
    });

    let mut used_gen = "";
    let mut created: Option<Value> = None;
    for (gen, path) in [
        ("new(/a/)", "/a/api/file/upload_request"),
        ("old(/b/)", "/b/api/file/upload_request"),
    ] {
        let url = format!("{}{path}", api::PRIMARY_BASE);
        let r = spike.api_post_json(&url, &body, &[]).await?;
        api::show(&format!("mkdir:{gen}:first"), &r);
        let (code, msg) = api::code_of(&r.body);
        if code == Some(0) {
            used_gen = gen;
            created = serde_json::from_str::<Value>(&r.body).ok();
            break;
        }
        println!("[mkdir] {gen} refused code={code:?} msg={msg} — trying next generation");
    }
    let Some(created) = created else {
        bail!("mkdir failed on both generations");
    };
    let data = created.get("data").cloned().unwrap_or(Value::Null);
    let fid = data
        .get("Info")
        .and_then(|i| field_i64(i, &["FileId", "fileId"]))
        .or_else(|| field_i64(&data, &["FileId", "fileId"]))
        .context("created but FileId not found in response")?;
    println!("[mkdir] created via {used_gen} FileId={fid}");

    // 5060 sample: same name again, no duplicate.
    let path = if used_gen.starts_with("new") {
        "/a/api/file/upload_request"
    } else {
        "/b/api/file/upload_request"
    };
    let url = format!("{}{path}", api::PRIMARY_BASE);
    let r = spike.api_post_json(&url, &body, &[]).await?;
    api::show("mkdir:repeat-no-duplicate", &r);
    let (code, msg) = api::code_of(&r.body);
    println!(
        "[SUMMARY] mkdir|ok=1|gen={used_gen}|FileId={fid}|repeat_code={code:?}|repeat_msg={msg:?}"
    );
    Ok(0)
}

// --------------------------------------------------------- probe-dir-rename

/// 123-2 task 0: is `/a/api/file/rename` usable for DIRECTORIES?
/// mkdir `e2e_pan123_r<rand>` -> rename the directory (fileId = the dir) ->
/// list read-back (new name present / old gone / children preserved) ->
/// trash sweep + verify-empty. Retries <=3 with >=2s spacing; the sweep runs
/// even on failure (leftovers must be cleaned).
pub async fn cmd_probe_dir_rename(rest: &[String]) -> Result<i32> {
    let _ = Args::new(rest);
    let (_paths, spike) = spike_with_tokens()?;
    let stamp = format!("e2e_pan123_r{}", &state::rand_hex(4));
    let old_name = stamp.clone();
    let new_name = format!("{stamp}b");
    let prefix = stamp.clone();

    // Sweep helper: trash every root entry with the stamp prefix, verify empty.
    async fn sweep(spike: &Spike, prefix: &str) -> Result<()> {
        let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
        let mut victims = Vec::new();
        for page in 1..=10u32 {
            let r = spike
                .api_get(&url, &list_query(0, page, 100, false))
                .await?;
            let (code, msg) = api::code_of(&r.body);
            if code != Some(0) {
                bail!("sweep list page {page} failed code={code:?} msg={msg}");
            }
            let items = info_list_of(&r.body);
            let n = items.len();
            for it in items {
                let name = field_str(&it, &["FileName", "fileName"]).unwrap_or("");
                if name.starts_with(&prefix) {
                    victims.push((
                        field_i64(&it, &["FileId", "fileId"]).unwrap_or(0),
                        name.to_string(),
                    ));
                }
            }
            if n < 100 {
                break;
            }
        }
        for (fid, name) in &victims {
            let r = trash_correct(spike, *fid, true).await?;
            let (code, msg) = api::code_of(&r.body);
            println!("[sweep] trash {name:?} (fid={fid}) code={code:?} msg={msg:?}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let r = spike.api_get(&url, &list_query(0, 1, 100, false)).await?;
        let remaining: Vec<String> = info_list_of(&r.body)
            .into_iter()
            .filter_map(|it| field_str(&it, &["FileName", "fileName"]).map(str::to_string))
            .filter(|n| n.starts_with(&prefix))
            .collect();
        println!(
            "[sweep] trashed={} remaining_with_prefix={} verdict={}",
            victims.len(),
            remaining.len(),
            if remaining.is_empty() { "clean" } else { "INCOMPLETE" }
        );
        Ok(())
    }

    let result = async {
        // 1. mkdir the directory (retry <=3, spacing >=2s).
        let mkdir_body = json!({
            "driveId": 0,
            "etag": "",
            "fileName": old_name,
            "parentFileId": 0,
            "size": 0,
            "type": 1,
            "NotReuse": true,
        });
        let url = format!("{}/a/api/file/upload_request", api::PRIMARY_BASE);
        let mut fid: Option<i64> = None;
        for attempt in 1..=3u32 {
            let r = spike.api_post_json(&url, &mkdir_body, &[]).await?;
            api::show(&format!("mkdir:{attempt}"), &r);
            let (code, msg) = api::code_of(&r.body);
            if code == Some(0) {
                let v: Value = serde_json::from_str(&r.body)?;
                fid = v
                    .get("data")
                    .and_then(|d| d.get("Info"))
                    .and_then(|i| field_i64(i, &["FileId", "fileId"]))
                    .or_else(|| {
                        v.get("data")
                            .and_then(|d| field_i64(d, &["FileId", "fileId"]))
                    });
                if fid.is_some() {
                    break;
                }
                bail!("mkdir code=0 but FileId missing from data.Info");
            }
            eprintln!("[mkdir] attempt {attempt} refused code={code:?} msg={msg}");
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
        let fid = fid.context("mkdir failed after 3 attempts")?;
        println!("[dir-rename] mkdir ok FileId={fid} name={old_name:?}");

        // 2. rename the DIRECTORY: new generation first, then old.
        let rename_body = json!({
            "driveId": 0,
            "fileId": fid,
            "fileName": new_name,
        });
        let mut used_gen = "";
        for (gen, path) in [
            ("new(/a/)", "/a/api/file/rename"),
            ("old(/b/)", "/b/api/file/rename"),
        ] {
            let url = format!("{}{path}", api::PRIMARY_BASE);
            let r = spike.api_post_json(&url, &rename_body, &[]).await?;
            api::show(&format!("dir-rename:{gen}"), &r);
            let (code, msg) = api::code_of(&r.body);
            if code == Some(0) {
                used_gen = gen;
                break;
            }
            println!("[dir-rename] {gen} refused code={code:?} msg={msg} — trying next");
        }
        if used_gen.is_empty() {
            bail!("directory rename refused on BOTH generations");
        }

        // 3. list read-back: new name present, old gone.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let found_new = find_by_name(&spike, 0, &new_name).await?;
        let found_old = find_by_name(&spike, 0, &old_name).await?;
        let new_type = found_new
            .as_ref()
            .and_then(|it| field_i64(it, &["Type", "type"]));
        println!(
            "[dir-rename] read-back new_name_found={} old_name_found={} new_type={:?}",
            found_new.is_some(),
            found_old.is_some(),
            new_type
        );
        let ok = found_new.is_some() && found_old.is_none();
        println!(
            "[SUMMARY] dir_rename|usable={}|gen={used_gen}|same_fid={:?}|type_preserved={:?}",
            ok,
            found_new
                .as_ref()
                .and_then(|it| field_i64(it, &["FileId", "fileId"]))
                .map(|f| f == fid),
            new_type.map(|t| t == 1)
        );
        if !ok {
            bail!("read-back mismatch: rename not effective");
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // 4. mandatory sweep (even on failure).
    let sweep_result = sweep(&spike, &prefix).await;
    result?;
    sweep_result?;
    Ok(0)
}

// -------------------------------------------------------------- gen-file

/// Random-content payload file + MD5 (for upload etag).
pub async fn cmd_gen_file(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let size_kb: u64 = pos.first().and_then(|v| v.parse().ok()).unwrap_or(2048);
    let paths = Paths::from_env();
    let out = args
        .flag("--out")
        .map(std::path::PathBuf::from)
        .unwrap_or(paths.payload());
    let mut buf = vec![0u8; (size_kb * 1024) as usize];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut buf);
    std::fs::write(&out, &buf).with_context(|| format!("write {}", out.display()))?;
    let digest = format!("{:x}", md5::compute(&buf));
    println!(
        "[gen-file] {} bytes={} md5={}",
        out.display(),
        buf.len(),
        digest
    );
    Ok(0)
}

// ----------------------------------------------------------- probe-upload

/// Minimal single-slice upload chain (only what group ④ needs to have a
/// downloadable file): upload_request(type 0) -> presign -> PUT ->
/// s3_complete -> upload_complete -> list read-back. Full upload semantics
/// (duplicate modes etc.) belong to the write-path leg.
pub async fn cmd_probe_upload(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(file) = pos.first() else {
        eprintln!("usage: probe-upload <file> [--parent X]");
        return Ok(2);
    };
    let parent: i64 = args
        .flag("--parent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let (_paths, spike) = spike_with_tokens()?;

    let data = std::fs::read(file).with_context(|| format!("read {file}"))?;
    let etag = format!("{:x}", md5::compute(&data));
    let name = std::path::Path::new(file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pan123-spike-file.bin");
    println!("[upload] file={file} size={} md5={etag}", data.len());

    let body = json!({
        "driveId": 0,
        "etag": etag,
        "fileName": name,
        "parentFileId": parent,
        "size": data.len(),
        "type": 0,
    });
    // File upload stays on /b/ per 123panNextGen (mkdir uses /a/).
    let mut up: Option<Value> = None;
    for (gen, path) in [
        ("old(/b/)", "/b/api/file/upload_request"),
        ("new(/a/)", "/a/api/file/upload_request"),
    ] {
        let url = format!("{}{path}", api::PRIMARY_BASE);
        let r = spike.api_post_json(&url, &body, &[]).await?;
        api::show(&format!("upload-request:{gen}"), &r);
        let (code, _msg) = api::code_of(&r.body);
        if code == Some(0) {
            up = Some(serde_json::from_str(&r.body)?);
            println!("[upload] upload_request accepted via {gen}");
            break;
        }
    }
    let up = up.context("upload_request refused on both generations")?;
    let d = up.get("data").cloned().unwrap_or(Value::Null);
    if d.get("Reuse").and_then(|r| r.as_bool()).unwrap_or(false) {
        let fid = field_i64(&d, &["FileId", "fileId"]).unwrap_or(0);
        println!("[SUMMARY] upload|ok=1|rapid=1|FileId={fid}");
        return Ok(0);
    }

    let bucket = field_str(&d, &["Bucket", "bucket"])
        .context("Bucket missing")?
        .to_string();
    let key = field_str(&d, &["Key", "key"])
        .context("Key missing")?
        .to_string();
    let upload_id = field_str(&d, &["UploadId", "uploadId"])
        .context("UploadId missing")?
        .to_string();
    let storage_node = field_str(&d, &["StorageNode", "storageNode"])
        .context("StorageNode missing")?
        .to_string();
    let up_file_id = field_i64(&d, &["FileId", "fileId"]).context("temp FileId missing")?;
    let slice_size = field_i64(&d, &["SliceSize", "sliceSize"]);
    println!(
        "[upload] bucket={bucket:?} key_len={} upload_id_len={} node_len={} tmp_file_id={up_file_id} slice_size={slice_size:?}",
        key.len(),
        upload_id.len(),
        storage_node.len()
    );

    // Single-part presign (both refs agree /b/, key casing StorageNode).
    let auth_url = format!("{}/b/api/file/s3_upload_object/auth", api::PRIMARY_BASE);
    let auth_body = json!({
        "bucket": bucket,
        "key": key,
        "partNumberStart": 1,
        "partNumberEnd": 2,
        "uploadId": upload_id,
        "StorageNode": storage_node,
    });
    let r = spike.api_post_json(&auth_url, &auth_body, &[]).await?;
    api::show("upload:presign", &r);
    let (code, msg) = api::code_of(&r.body);
    if code != Some(0) {
        bail!("presign refused code={code:?} msg={msg}");
    }
    let put_url = serde_json::from_str::<Value>(&r.body)?
        .get("data")
        .and_then(|d| d.get("presignedUrls").or_else(|| d.get("PresignedUrls")))
        .and_then(|p| p.get("1").or_else(|| p.get(1usize)).cloned())
        .and_then(|u| u.as_str().map(|s| s.to_string()))
        .context("presignedUrls[\"1\"] missing")?;
    println!(
        "[upload] presigned url head={:?}",
        api::truncate(&put_url, 120)
    );

    // Pure PUT on the transfer face — no 123pan headers (§5.12).
    let put = spike
        .transfer
        .put(&put_url)
        .header("content-length", data.len().to_string())
        .body(data.clone())
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await?;
    println!(
        "[upload] PUT part -> HTTP {} etag={:?}",
        put.status().as_u16(),
        put.headers().get("etag").and_then(|v| v.to_str().ok())
    );
    if !put.status().is_success() {
        bail!("PUT part failed: HTTP {}", put.status());
    }

    // s3 confirm chain (123panNextGen current): list parts -> complete.
    let comp_body = json!({
        "bucket": bucket,
        "key": key,
        "uploadId": upload_id,
        "storageNode": storage_node,
    });
    let list_parts_url = format!("{}/b/api/file/s3_list_upload_parts", api::PRIMARY_BASE);
    let r = spike
        .api_post_json(&list_parts_url, &comp_body, &[])
        .await?;
    api::show("upload:s3_list_parts", &r);
    let s3_complete_url = format!(
        "{}/b/api/file/s3_complete_multipart_upload",
        api::PRIMARY_BASE
    );
    let r = spike
        .api_post_json(&s3_complete_url, &comp_body, &[])
        .await?;
    api::show("upload:s3_complete", &r);

    // upload_complete: new (no /v2) first, then old.
    let mut done = false;
    for (gen, path, body) in [
        (
            "new",
            "/b/api/file/upload_complete",
            json!({"fileId": up_file_id}),
        ),
        (
            "old",
            "/b/api/file/upload_complete/v2",
            json!({
                "fileId": up_file_id,
                "bucket": bucket,
                "fileSize": data.len(),
                "key": key,
                "isMultipart": false,
                "uploadId": upload_id,
                "StorageNode": storage_node,
            }),
        ),
    ] {
        let url = format!("{}{path}", api::PRIMARY_BASE);
        let r = spike.api_post_json(&url, &body, &[]).await?;
        api::show(&format!("upload:complete:{gen}"), &r);
        let (code, _msg) = api::code_of(&r.body);
        if code == Some(0) {
            done = true;
            println!("[upload] upload_complete accepted via {gen}");
            break;
        }
    }
    if !done {
        bail!("upload_complete refused on both generations");
    }

    // Read-back: file visible in parent list.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let found = find_by_name(&spike, parent, name).await?;
    match found {
        Some(item) => {
            print_item("upload:readback", &item);
            let fid = field_i64(&item, &["FileId", "fileId"]).unwrap_or(0);
            println!("[SUMMARY] upload|ok=1|FileId={fid}|size={}", data.len());
            Ok(0)
        }
        None => {
            println!("[SUMMARY] upload|ok=0|readback=missing");
            bail!("uploaded file not visible in parent list")
        }
    }
}

// ------------------------------------------------------------- probe-trash

/// Correct minimal payload delete + read-back verification (gone from list
/// = really deleted; code=0 but still there = silent-failure trap).
pub async fn cmd_probe_trash(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    if pos.len() < 2 {
        eprintln!("usage: probe-trash <fid> <parent> [--new|--old]");
        return Ok(2);
    }
    let fid: i64 = pos[0].parse()?;
    let parent: i64 = pos[1].parse()?;
    let new_gen = match args.flag("--gen").as_deref() {
        Some("old") => false,
        _ => true,
    };
    let (_paths, spike) = spike_with_tokens()?;

    let r = trash_correct(&spike, fid, new_gen).await?;
    api::show("trash:correct-payload", &r);
    let (code, msg) = api::code_of(&r.body);
    println!("[trash] code={code:?} msg={msg:?}");

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // read-back: find by id in parent list
    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let lr = spike
        .api_get(&url, &list_query(parent, 1, 100, false))
        .await?;
    let still = info_list_of(&lr.body)
        .into_iter()
        .any(|it| field_i64(&it, &["FileId", "fileId"]) == Some(fid));
    // recycle-bin view (trashed=true)
    let tr = spike
        .api_get(&url, &list_query(parent, 1, 100, true))
        .await?;
    let in_trash = info_list_of(&tr.body)
        .into_iter()
        .any(|it| field_i64(&it, &["FileId", "fileId"]) == Some(fid));
    println!(
        "[SUMMARY] trash|code={code:?}|still_in_parent={still}|in_recycle_view={in_trash}|verdict={}",
        if still { "SILENT-FAILURE (code=0 but not deleted)" } else if in_trash { "intoRecycle confirmed" } else { "deleted (not visible in recycle view)" }
    );
    Ok(0)
}

/// Trap reproduction: WRONG payload shapes -> observe code=0 while the
/// file survives. `--shape extra-keys` = info dict with extra keys inside
/// the list; `--shape bare-dict` = single dict NOT wrapped in a list (the
/// documented 123panNextGen silent-ignore form, wiki.md:779).
pub async fn cmd_probe_trash_trap(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    if pos.len() < 3 {
        eprintln!("usage: probe-trash-trap <fid> <parent> <name> [--shape extra-keys|bare-dict]");
        return Ok(2);
    }
    let fid: i64 = pos[0].parse()?;
    let parent: i64 = pos[1].parse()?;
    let name = pos[2].to_string();
    let shape = args.flag("--shape").unwrap_or_else(|| "extra-keys".into());
    let (_paths, spike) = spike_with_tokens()?;

    let url = format!("{}/a/api/file/trash", api::PRIMARY_BASE);
    let entry = json!({
        "FileId": fid,
        "FileName": name,
        "ParentFileId": parent,
        "Type": 1,
        "Size": 0,
    });
    let body = json!({
        "driveId": 0,
        "fileTrashInfoList": if shape == "bare-dict" { entry } else { json!([entry]) },
        "operation": true,
        "event": "intoRecycle",
    });
    let r = spike.api_post_json(&url, &body, &[]).await?;
    api::show(&format!("trash-trap:{shape}"), &r);
    let (code, _msg) = api::code_of(&r.body);

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let found = find_by_name(&spike, parent, &name).await?;
    let survived = found
        .map(|it| field_i64(&it, &["FileId", "fileId"]) == Some(fid))
        .unwrap_or(false);
    println!(
        "[SUMMARY] trash-trap|shape={shape}|code={code:?}|file_survived={survived}|verdict={}",
        if survived {
            "TRAP REPRODUCED: code=0 but nothing deleted"
        } else {
            "no trap: wrong payload actually deleted or errored"
        }
    );
    Ok(0)
}

// ---------------------------------------------------------- probe-download

/// traffic/check numbers -> info (metadata + endpoint liveness) ->
/// download_info (new + old generations) -> CDN GET Range bytes=100- (assert
/// 206 + Content-Range) -> one dlink reuse attempt.
pub async fn cmd_probe_download(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(fid_s) = pos.first() else {
        eprintln!("usage: probe-download <fid> [--file local-path]");
        return Ok(2);
    };
    let fid: i64 = fid_s.parse()?;
    let local = args.flag("--file");
    let (_paths, spike) = spike_with_tokens()?;

    // 1. traffic check with the real fid.
    let t_url = format!("{}/b/api/file/download/traffic/check", api::PRIMARY_BASE);
    let r = spike
        .api_post_json(&t_url, &json!({"fids": [fid]}), &[])
        .await?;
    api::show("download:traffic-check", &r);

    // 2. info (metadata for download_info payload; also §3.1 liveness row).
    let i_url = format!("{}/b/api/file/info", api::PRIMARY_BASE);
    let r = spike
        .api_post_json(&i_url, &json!({"fileIdList": [{"fileId": fid}]}), &[])
        .await?;
    api::show("download:info", &r);
    let (code, msg) = api::code_of(&r.body);
    if code != Some(0) {
        bail!("info failed code={code:?} msg={msg} — cannot build download_info payload");
    }
    let item = serde_json::from_str::<Value>(&r.body)?
        .get("data")
        .and_then(|d| d.get("InfoList").or_else(|| d.get("infoList")))
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .cloned()
        .context("info code=0 but InfoList empty")?;
    print_item("download:info-item", &item);

    let dl_body = json!({
        "driveId": 0,
        "etag": field_str(&item, &["Etag", "etag"]).unwrap_or(""),
        "fileId": fid,
        "s3keyFlag": field_str(&item, &["S3KeyFlag", "s3keyFlag"]).unwrap_or(""),
        "type": field_i64(&item, &["Type", "type"]).unwrap_or(0),
        "fileName": field_str(&item, &["FileName", "fileName"]).unwrap_or(""),
        "size": field_i64(&item, &["Size", "size"]).unwrap_or(0),
    });

    // 3. download_info: NEW generation first.
    struct DlOutcome {
        url: String,
        via: String,
    }
    let mut outcomes: Vec<DlOutcome> = Vec::new();
    for (gen, path) in [
        ("new(/a/)", "/a/api/file/download_info"),
        ("old(/b/v2/)", "/b/api/v2/file/download_info"),
    ] {
        let url = format!("{}{path}", api::PRIMARY_BASE);
        let r = spike.api_post_json(&url, &dl_body, &[]).await?;
        api::show(&format!("download_info:{gen}"), &r);
        let (code, msg) = api::code_of(&r.body);
        if code != Some(0) {
            eprintln!("[download] {gen} refused code={code:?} msg={msg}");
            continue;
        }
        let d = serde_json::from_str::<Value>(&r.body)?
            .get("data")
            .cloned()
            .unwrap_or(Value::Null);
        let direct = field_str(&d, &["RedirectUrl", "redirect_url"]);
        let dl = field_str(&d, &["DownloadUrl", "downloadUrl"]);
        let dispatch = d
            .get("DispatchList")
            .or_else(|| d.get("dispatchList"))
            .and_then(|l| l.as_array())
            .and_then(|a| a.first())
            .and_then(|it| field_str(it, &["Prefix", "prefix"]));
        let chosen = direct
            .map(|s| s.to_string())
            .or_else(|| dl.map(|s| s.to_string()));
        let full = chosen.map(|c| match dispatch {
            Some(p) if dl.map(|d| d == c).unwrap_or(false) => format!("{p}{c}"),
            _ => c,
        });
        println!(
            "[download] {gen} data_keys={} direct={:?} dl_len={:?} dispatch_prefix_len={:?}",
            d.as_object()
                .map(|m| m.keys().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_default(),
            direct.is_some(),
            dl.map(|s| s.len()),
            dispatch.map(|s| s.len()),
        );
        if let Some(f) = full {
            outcomes.push(DlOutcome {
                url: f,
                via: gen.to_string(),
            });
        }
    }
    if outcomes.is_empty() {
        bail!("no download URL from either generation");
    }
    for o in &outcomes {
        println!(
            "[download] url via {} head={:?}",
            o.via,
            api::truncate(&o.url, 140)
        );
    }

    // 4. CDN GET with Range. The web-identity DownloadUrl may be a web-pro2
    // interstitial (plain HTML or JSON redirect body — D5: link RESOLUTION
    // only, no auto_redirect injection). Resolution: Location header ->
    // HTML href anywhere in body -> params= base64 self-decode.
    let mut resp_206: Option<(Vec<u8>, Option<String>, String)> = None;
    for outcome in &outcomes {
        println!("[download] === resolving via {} ===", outcome.via);
        let mut current = outcome.url.clone();
        for hop in 1..=4u32 {
            let req = spike
                .transfer
                .get(&current)
                .header("range", "bytes=100-")
                .timeout(std::time::Duration::from_secs(120));
            let r = req.send().await;
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[download] hop#{hop} transport error: {e:#}");
                    break;
                }
            };
            let status = r.status().as_u16();
            let ct = r
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let cr = r
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            println!(
                "[download] hop#{hop} HTTP {status} content-type={ct:?} content-range={cr:?} url_head={:?}",
                api::truncate(&r.url().to_string(), 140)
            );
            if status == 206 {
                let final_url = r.url().to_string();
                resp_206 = Some((r.bytes().await?.to_vec(), cr, final_url));
                break;
            }
            if status >= 300 && status < 400 {
                if let Some(loc) = r
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string())
                {
                    println!("         -> following Location header");
                    current = loc;
                    continue;
                }
            }
            let body = r.text().await.unwrap_or_default();
            if ct.contains("json") {
                println!("         json body={}", api::redact(&body));
                let next = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
                    v.get("data")
                        .and_then(|d| {
                            field_str(d, &["redirect_url", "RedirectUrl"])
                                .or_else(|| field_str(d, &["DownloadUrl", "downloadUrl"]))
                        })
                        .map(|s| s.to_string())
                        .or_else(|| {
                            field_str(&v, &["redirect_url", "RedirectUrl"]).map(|s| s.to_string())
                        })
                });
                match next {
                    Some(n) => {
                        current = n;
                        continue;
                    }
                    None => {
                        eprintln!("         json body without redirect url — dead end");
                        break;
                    }
                }
            }
            if ct.contains("html") || body.starts_with("<!DOCTYPE") {
                // scan the whole body for an href link (single or double quoted)
                let found = extract_href(&body);
                println!(
                    "         html len={} href={:?}",
                    body.len(),
                    found.as_deref().map(|h| api::truncate(h, 140))
                );
                if let Some(h) = found {
                    current = h;
                    continue;
                }
                // fallback: decode params= from a download-v2 style URL
                if let Some(inner) = decode_download_v2_params(&current) {
                    println!(
                        "         -> params base64 self-decode: {:?}",
                        api::truncate(&inner, 140)
                    );
                    current = inner;
                    continue;
                }
                eprintln!("         html without resolvable link — dead end");
                break;
            }
            eprintln!("         body head={}", api::truncate(&body, 300));
            break;
        }
        if resp_206.is_some() {
            break;
        }
    }
    let Some((bytes, content_range, final_url)) = resp_206 else {
        bail!("no 206 from CDN GET");
    };
    println!(
        "[download] RANGE OK: got {} bytes, content-range={content_range:?}",
        bytes.len()
    );
    if let Some(local_path) = local {
        let local_bytes =
            std::fs::read(&local_path).with_context(|| format!("read {local_path}"))?;
        let expect = &local_bytes[100.min(local_bytes.len())..];
        let match_verdict = expect == bytes.as_slice();
        println!(
            "[download] byte-compare with {local_path}: {} (expect {} bytes, got {})",
            if match_verdict { "MATCH" } else { "MISMATCH" },
            expect.len(),
            bytes.len()
        );
        let out = Paths::from_env().dir.join("pan123-spike-range-body.bin");
        std::fs::write(&out, &bytes)?;
    }

    // 5. dlink reuse: ONE more tiny GET on the same resolved URL.
    let req = spike
        .transfer
        .get(&final_url)
        .header("range", "bytes=0-99")
        .timeout(std::time::Duration::from_secs(60));
    match req.send().await {
        Ok(r) => println!(
            "[download] dlink-reuse(second GET) HTTP {} content-range={:?}",
            r.status().as_u16(),
            r.headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
        ),
        Err(e) => eprintln!("[download] dlink-reuse transport error: {e:#}"),
    }
    println!("[SUMMARY] download|ok=1|range=206|content_range={content_range:?}");
    Ok(0)
}

// -------------------------------------------------------------- probe-heads

/// Dual-head necessity: same list call with both auth heads, Authorization
/// only, Cookie only, and neither.
pub async fn cmd_probe_heads(rest: &[String]) -> Result<i32> {
    let _ = Args::new(rest);
    let (_paths, spike) = spike_with_tokens()?;
    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let q = list_query(0, 1, 1, false);
    for head in ["both", "bearer", "cookie", "none"] {
        match spike.api_get_single_head(&url, &q, head).await {
            Ok(r) => println!("[heads:{head}] {}", api::summarize(&r)),
            Err(e) => eprintln!("[heads:{head}] transport error: {e:#}"),
        }
    }
    // Characterize the "none" form on a user-scoped endpoint: an anonymous
    // list of an EMPTY root also returns code=0, so list alone cannot
    // distinguish auth failure from an empty result there.
    let u2 = format!("{}/b/api/user/info", api::PRIMARY_BASE);
    for head in ["none", "cookie"] {
        match spike.api_get_single_head(&u2, &[], head).await {
            Ok(r) => println!("[heads:user-info:{head}] {}", api::summarize(&r)),
            Err(e) => eprintln!("[heads:user-info:{head}] transport error: {e:#}"),
        }
    }
    Ok(0)
}

// ------------------------------------------------------------ probe-user

/// Quota / traffic-limit fields (record only — D5: no bypass).
pub async fn cmd_probe_user(rest: &[String]) -> Result<i32> {
    let _ = Args::new(rest);
    let (_paths, spike) = spike_with_tokens()?;
    for (tag, path) in [
        ("user-info", "/b/api/user/info"),
        ("quota-report", "/b/api/restful/goapi/v1/user/report/info"),
    ] {
        let url = format!("{}{path}", api::PRIMARY_BASE);
        match spike.api_get(&url, &[]).await {
            Ok(r) => {
                api::show(tag, &r);
                if let Ok(v) = serde_json::from_str::<Value>(&r.body) {
                    for key in [
                        "DirectTraffic",
                        "directTraffic",
                        "unlimited",
                        "Unlimited",
                        "UseTotalSpace",
                        "UseSpace",
                        "TotalSpace",
                        "totalSpace",
                        "Traffic",
                        "traffic",
                        "ExpirationTime",
                        "IsVip",
                        "isVip",
                    ] {
                        if let Some(val) = v.get("data").and_then(|d| d.get(key)) {
                            println!("         {key}={val}");
                        }
                    }
                }
            }
            Err(e) => eprintln!("[{tag}] transport error: {e:#}"),
        }
    }
    Ok(0)
}

// --------------------------------------------------------------- cleanup

/// Strict-prefix sweep: trash every root entry whose name starts with the
/// given prefix, then read-back verify the prefix is gone.
pub async fn cmd_cleanup(rest: &[String]) -> Result<i32> {
    let args = Args::new(rest);
    let pos = args.positional();
    let Some(prefix) = pos.first() else {
        eprintln!("usage: cleanup <prefix>");
        return Ok(2);
    };
    let (_paths, spike) = spike_with_tokens()?;

    let url = format!("{}/api/file/list/new", api::PRIMARY_BASE);
    let mut victims: Vec<(i64, String)> = Vec::new();
    for page in 1..=10u32 {
        let r = spike
            .api_get(&url, &list_query(0, page, 100, false))
            .await?;
        let (code, msg) = api::code_of(&r.body);
        if code != Some(0) {
            bail!("list page {page} failed code={code:?} msg={msg}");
        }
        let items = info_list_of(&r.body);
        let n = items.len();
        for it in items {
            let name = field_str(&it, &["FileName", "fileName"]).unwrap_or("");
            if name.starts_with(prefix) {
                victims.push((
                    field_i64(&it, &["FileId", "fileId"]).unwrap_or(0),
                    name.to_string(),
                ));
            }
        }
        if n < 100 {
            break;
        }
    }
    println!("[cleanup] prefix={prefix:?} victims={}", victims.len());
    for (fid, name) in &victims {
        let r = trash_correct(&spike, *fid, true).await?;
        let (code, msg) = api::code_of(&r.body);
        println!("[cleanup] trash {name:?} (fid={fid}) code={code:?} msg={msg:?}");
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let r = spike.api_get(&url, &list_query(0, 1, 100, false)).await?;
    let remaining: Vec<String> = info_list_of(&r.body)
        .into_iter()
        .map(|it| {
            field_str(&it, &["FileName", "fileName"])
                .unwrap_or("")
                .to_string()
        })
        .filter(|n| n.starts_with(prefix))
        .collect();
    println!(
        "[SUMMARY] cleanup|trashed={}|remaining_with_prefix={}|verdict={}",
        victims.len(),
        remaining.len(),
        if remaining.is_empty() {
            "clean"
        } else {
            "INCOMPLETE"
        }
    );
    if !remaining.is_empty() {
        bail!("cleanup incomplete: {remaining:?}");
    }
    Ok(0)
}
