//! quick-xml multistatus parsing for the spike. Element names are matched
//! by LOCAL name with the prefix ignored — the two fixtures emit
//! different prefixes (rclone uses `D:`, apache cycles `D:`/`lp1:`/`g0:`/
//! `ns0:`-style generated prefixes), all bound to "DAV:". Prefixes are
//! split at the LAST ':' so even an exotic multi-segment prefix cannot
//! confuse the local-name extraction.
//!
//! Structure caveat handled here: inside `<propstat>` the `<prop>` child
//! precedes `<status>` (RFC 4918 ordering), so per-prop rows capture a
//! propstat ordinal during the walk and statuses are joined afterwards.

use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::Reader;

/// One flattened prop observation: which href it belonged to, the inner
/// propstat status line, the prop local name, its text content, and (for
/// `resourcetype`) whether a `collection` child was present.
#[derive(Debug, Clone)]
pub struct PropRow {
    pub href: String,
    pub propstat_status: Option<String>,
    pub prop: String,
    pub text: String,
    pub collection_child: bool,
}

/// A PROPFIND response entry projected onto the fields the driver needs.
#[derive(Debug, Clone)]
pub struct PropfindEntry {
    pub href: String,
    pub is_collection: bool,
    pub content_length: Option<u64>,
    pub last_modified: Option<String>,
}

fn local_name(qname: quick_xml::name::QName<'_>) -> String {
    let raw = String::from_utf8_lossy(qname.as_ref());
    match raw.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => raw.into_owned(),
    }
}

/// Text content of an event, trimmed. (Entity unescaping is not needed
/// for the hrefs / numbers / IMF-fixdates this spike reads; our test
/// names are plain ASCII.)
fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Walk a 207 multistatus body into flat prop rows. Non-multistatus
/// bodies simply yield no rows.
pub fn parse_multistatus(xml: &str) -> Result<Vec<PropRow>> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut href = String::new();
    let mut href_sinking = false;
    let mut status_buf = String::new();
    let mut status_sinking = false;
    // statuses[propstat_n] is filled when the propstat's <status> closes.
    let mut statuses: Vec<Option<String>> = Vec::new();
    let mut propstat_n: usize = 0;
    let mut cur_prop: Option<String> = None;
    let mut cur_text = String::new();
    let mut collection_child = false;
    // (href, propstat ordinal, prop, text, collection_child)
    let mut raw: Vec<(String, usize, String, String, bool)> = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = local_name(e.name());
                let parent = stack.last().cloned().unwrap_or_default();
                if name == "response" {
                    href.clear();
                } else if name == "propstat" {
                    propstat_n = statuses.len();
                    statuses.push(None);
                } else if name == "href" && parent == "response" {
                    href_sinking = true;
                } else if name == "status" && parent == "propstat" {
                    status_sinking = true;
                    status_buf.clear();
                } else if name == "collection" && parent == "resourcetype" {
                    collection_child = true;
                } else if parent == "prop" && cur_prop.is_none() {
                    cur_prop = Some(name.clone());
                    cur_text.clear();
                    collection_child = false;
                }
                stack.push(name);
            }
            Event::Empty(e) => {
                let name = local_name(e.name());
                let parent = stack.last().cloned().unwrap_or_default();
                if parent == "prop" && cur_prop.is_none() {
                    raw.push((href.clone(), propstat_n, name, String::new(), false));
                } else if parent == "resourcetype" && name == "collection" {
                    collection_child = true;
                }
            }
            Event::Text(t) => {
                let txt = text_of(t.as_ref());
                if href_sinking {
                    href.push_str(&txt);
                } else if status_sinking {
                    status_buf.push_str(&txt);
                } else if cur_prop.is_some() {
                    cur_text.push_str(&txt);
                }
            }
            Event::CData(t) => {
                let txt = text_of(t.as_ref());
                if href_sinking {
                    href.push_str(&txt);
                } else if status_sinking {
                    status_buf.push_str(&txt);
                } else if cur_prop.is_some() {
                    cur_text.push_str(&txt);
                }
            }
            Event::End(_) => {
                let name = stack.pop().unwrap_or_default();
                if name == "href" {
                    href_sinking = false;
                } else if name == "status" {
                    status_sinking = false;
                    if propstat_n < statuses.len() {
                        statuses[propstat_n] = Some(status_buf.trim().to_string());
                    }
                } else if let Some(prop) = cur_prop.clone() {
                    if prop == name {
                        raw.push((
                            href.clone(),
                            propstat_n,
                            prop,
                            cur_text.trim().to_string(),
                            collection_child,
                        ));
                        cur_prop = None;
                        cur_text.clear();
                        collection_child = false;
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let rows = raw
        .into_iter()
        .map(|(href, n, prop, text, collection_child)| PropRow {
            href,
            propstat_status: statuses.get(n).cloned().flatten(),
            prop,
            text,
            collection_child,
        })
        .collect();
    Ok(rows)
}

/// Group prop rows into one entry per response href (first-seen order).
pub fn entries(rows: &[PropRow]) -> Vec<PropfindEntry> {
    let mut out: Vec<PropfindEntry> = Vec::new();
    for r in rows {
        let idx = match out.iter().position(|e| e.href == r.href) {
            Some(i) => i,
            None => {
                out.push(PropfindEntry {
                    href: r.href.clone(),
                    is_collection: false,
                    content_length: None,
                    last_modified: None,
                });
                out.len() - 1
            }
        };
        let entry = &mut out[idx];
        if r.prop == "resourcetype" && r.collection_child {
            entry.is_collection = true;
        }
        if r.prop == "getcontentlength" {
            entry.content_length = r.text.parse().ok();
        }
        if r.prop == "getlastmodified" && !r.text.is_empty() {
            entry.last_modified = Some(r.text.clone());
        }
    }
    out
}
