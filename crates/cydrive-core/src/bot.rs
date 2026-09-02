//! Bot command surface (Python `_process_bot_command` baseline,
//! `telegram_client.py:94-141`).
//!
//! One inbound command text is matched against the Python prefix chain
//! (help -> stats -> search -> get; unknown text is silently ignored) and
//! answered through [`CloudTransport::send_text`] /
//! [`CloudTransport::send_document`] — the reply target is the configured
//! chat, the same peer the inbound stream filters on. Reply-send failures
//! propagate to the caller (the inbound worker warns and keeps consuming).
//!
//! Mirrored Python quirks (frozen contract):
//!
//! * the text is trimmed before matching (`event.message.text.strip()`);
//! * matching is `startswith`, so "/helpme" answers help and "/stats-x"
//!   answers stats;
//! * `size_gb >= 1` (not `>= 1024`) picks the GB branch — the bot stats
//!   command differs from the upload-path size string here;
//! * search results are `"\n".join`ed over a header line that already
//!   carries its own trailing `\n`, capped at 15 rows.
//!
//! `/get` is the adjudicated completion of a command the Python baseline
//! promised in its help text but never implemented (adjudication
//! 2026-09-02, `docs/decisions.md`): exact virtual-path lookup first,
//! then a name search whose result must be unique, then hydrate +
//! send_document. Its reply texts are self-designed (no baseline to
//! mirror) and noted as such below.

use std::sync::Arc;

use crate::database::MetaDatabase;
use crate::rel_path::RelPath;
use crate::transport::CloudTransport;
use crate::vfs::Vfs;

/// Errors surfaced while handling one bot command.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    /// Metadata lookup/persistence failed.
    #[error("db error: {0}")]
    Db(#[from] crate::database::DbError),
    /// Sending the reply (or the document) failed.
    #[error("transport error: {0}")]
    Transport(#[from] crate::transport::TransportError),
    /// Hydrating the requested file failed.
    #[error("vfs error: {0}")]
    Vfs(#[from] crate::vfs::VfsError),
}

/// Help text, verbatim Python baseline (`telegram_client.py:99-107`).
const HELP_TEXT: &str = "🚀 **CyDrive Cloud Storage Engine v2.0**\nDeveloped by Cynet Security Team (https://cynetx.ir)\n\n**Available Commands:**\n📊 `/stats` - View cloud storage analytics\n🔍 `/search <query>` - Search files in your drive\n📥 `/get <filename>` - Download a file directly\nℹ️ Send any file to this chat to save it to your Windows Drive!";

/// Max rows of a `/search` reply (Python `results[:15]`).
const SEARCH_LIMIT: usize = 15;

/// Bytes per MiB — the stats size math (`total_bytes / (1024 * 1024)`).
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// Handles one bot command text; replies via transport (chat = configured chat).
///
/// Unknown text produces no reply (`Ok(())` without sending), mirroring
/// the Python `if/elif` chain falling through.
pub async fn handle_command(
    db: &Arc<MetaDatabase>,
    vfs: &Vfs,
    transport: &dyn CloudTransport,
    drive_letter: &str,
    text: &str,
) -> Result<(), BotError> {
    // Python: `text = event.message.text.strip()` before any matching.
    let text = text.trim();

    if text.starts_with("/start") || text.starts_with("/help") {
        transport.send_text(HELP_TEXT).await?;
    } else if text.starts_with("/stats") {
        let stats = db.get_stats()?;
        // Python: size_mb = total_bytes / (1024*1024) (true division),
        // size_gb = size_mb / 1024, GB branch when size_gb >= 1.
        let size_mb = stats.total_bytes as f64 / BYTES_PER_MB;
        let size_gb = size_mb / 1024.0;
        let size_str = if size_gb >= 1.0 {
            format!("{size_gb:.2} GB")
        } else {
            format!("{size_mb:.2} MB")
        };
        let stats_text = format!(
            "📊 **CyDrive Storage Statistics**\n\n\
             📁 **Total Files:** `{}`\n\
             🗂️ **Total Folders:** `{}`\n\
             ☁️ **Total Cloud Storage:** `{}`\n\
             ✅ **Synced Files:** `{}`\n\
             🖥️ **Mapped Windows Drive:** `{}`",
            stats.total_files, stats.total_dirs, size_str, stats.uploaded_files, drive_letter
        );
        transport.send_text(&stats_text).await?;
    } else if text.starts_with("/search") {
        handle_search(db, transport, text).await?;
    } else if text.starts_with("/get") {
        handle_get(db, vfs, transport, text).await?;
    }
    Ok(())
}

/// `/search <query>`: missing-arg warning, no-results notice, or the
/// top-15 result rows (dirs first via the DB's ordering, folder/file
/// icon, `size // 1024` KB — floor division, Python `//` parity).
async fn handle_search(
    db: &Arc<MetaDatabase>,
    transport: &dyn CloudTransport,
    text: &str,
) -> Result<(), BotError> {
    let Some(query) = split_maxsplit1(text) else {
        transport
            .send_text("⚠️ Please specify a search keyword. e.g. `/search document.pdf`")
            .await?;
        return Ok(());
    };

    let results = db.search_files(query)?;
    if results.is_empty() {
        transport
            .send_text(&format!("🔍 No files found matching: `{query}`"))
            .await?;
        return Ok(());
    }

    // Python: lines = ["🔍 **Search Results:**\n"], one row appended per
    // result, reply = "\n".join(lines) — the header keeps its trailing
    // newline and the joined text ends without one.
    let mut lines = vec!["🔍 **Search Results:**\n".to_string()];
    for row in results.iter().take(SEARCH_LIMIT) {
        let icon = if row.is_dir { "📁" } else { "📄" };
        // Python `item["size"] // 1024` floors; div_euclid matches it
        // (a plain `/` would truncate toward zero on negatives).
        let size_kb = row.size.div_euclid(1024);
        lines.push(format!("{icon} `{}` ({} KB)", row.name, size_kb));
    }
    transport.send_text(&lines.join("\n")).await?;
    Ok(())
}

/// `/get <token>`: exact virtual path first, then a name search that must
/// be unique; a hit is hydrated and shipped back as a document (the
/// document is the reply — no extra text follows).
async fn handle_get(
    db: &Arc<MetaDatabase>,
    vfs: &Vfs,
    transport: &dyn CloudTransport,
    text: &str,
) -> Result<(), BotError> {
    let Some(token) = split_maxsplit1(text) else {
        // Self-designed text (baseline never implemented /get); mirrors
        // the /search missing-arg wording (adjudication 2026-09-02).
        transport
            .send_text("⚠️ Please specify a file name. e.g. `/get document.pdf`")
            .await?;
        return Ok(());
    };

    // Exact path: normalize the token (a leading '/' is accepted and
    // re-prefixed; polluted tokens can never match a stored path).
    let exact = match token_to_rel_path(token) {
        Some(rel) => db.get_file(rel.as_str())?,
        None => None,
    };
    let row = match exact {
        Some(row) => row,
        None => {
            let matches = db.search_files(token)?;
            match matches.len() {
                // Self-designed texts (adjudication 2026-09-02): 0 hits
                // and non-unique hits each get their own short notice.
                0 => {
                    transport
                        .send_text(&format!("❌ File not found: `{token}`"))
                        .await?;
                    return Ok(());
                }
                1 => matches.into_iter().next().expect("len checked 1"),
                count => {
                    transport
                        .send_text(&format!(
                            "⚠️ Ambiguous match for `{token}`: {count} files found, be more specific"
                        ))
                        .await?;
                    return Ok(());
                }
            }
        }
    };

    // Rows are always written through RelPath validation, so a stored
    // path parses; if one ever fails, "not found" is the honest answer.
    let Some(rel) = RelPath::new(&row.rel_path).ok() else {
        transport
            .send_text(&format!("❌ File not found: `{token}`"))
            .await?;
        return Ok(());
    };

    // Hydrate, then ship the local copy as a document. Either the
    // hydration or the local read failing answers with the same short
    // fetch-failed notice (self-designed wording, adjudication
    // 2026-09-02) — the command stays answerable instead of erroring the
    // worker.
    let fetched = match vfs.hydrate(&rel).await {
        Ok(local) => std::fs::read(&local).map_err(|err| err.to_string()),
        Err(error) => Err(error.to_string()),
    };
    match fetched {
        Ok(bytes) => transport.send_document(&row.name, &bytes).await?,
        Err(err) => {
            transport
                .send_text(&format!("⚠️ Failed to fetch `{}`: {err}", rel.as_str()))
                .await?;
        }
    }
    Ok(())
}

/// Python `str.split(maxsplit=1)` (no separator) semantics: leading
/// whitespace is skipped, the tail is split off at the first whitespace
/// run and kept verbatim (embedded spaces included); a tail that is empty
/// after the whitespace run means there is no second part.
fn split_maxsplit1(text: &str) -> Option<&str> {
    let start = text.find(|c: char| !c.is_whitespace())?;
    let rest = &text[start..];
    let tail = rest
        .find(char::is_whitespace)
        .map(|at| rest[at..].trim_start())?;
    (!tail.is_empty()).then_some(tail)
}

/// Normalizes a `/get` token into a virtual path: a leading `/` is
/// stripped, then the remainder is re-prefixed (`/get /a/b.txt` and
/// `/get a/b.txt` both resolve to `/a/b.txt`). Polluted tokens cannot
/// form a valid [`RelPath`] and yield `None` (treated as "no exact
/// match").
fn token_to_rel_path(token: &str) -> Option<RelPath> {
    let stripped = token.strip_prefix('/').unwrap_or(token);
    RelPath::new(&format!("/{stripped}")).ok()
}
