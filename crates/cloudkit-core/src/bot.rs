//! Bot command surface (Python `_process_bot_command` baseline,
//! `telegram_client.py:94-141`).
//!
//! One inbound command text is matched against the Python prefix chain
//! (help -> stats -> search -> get; unknown text is silently ignored) and
//! answered through [`ChatCap::send_text`] /
//! [`ChatCap::send_document`] — the reply target is the configured
//! chat, the same peer the inbound stream filters on (the CHAT capability
//! trait since the Batch R split; callers obtain it by probing
//! `CloudTransport::as_chat`). Reply-send failures propagate to the
//! caller (the inbound worker warns and keeps consuming).
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
//!
//! Tier-1 (2026-09-03) adds `/ls /mkdir /rm /quota /queue` to the same
//! `startswith` chain ahead of the unknown fallthrough; every reply text
//! is self-designed (no baseline) and pinned by `tests/bot.rs`. `/rm`
//! keeps the Python baseline's remote-deletion stance: only the metadata
//! row and the local cache copy go, the remote Telegram messages stay.

use std::sync::Arc;

use crate::database::MetaDatabase;
use crate::rel_path::RelPath;
use crate::transport::{ChatCap, StorageError};
use crate::vfs::{Vfs, VfsError};

/// Errors surfaced while handling one bot command.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    /// Metadata lookup/persistence failed.
    #[error("db error: {0}")]
    Db(#[from] crate::database::DbError),
    /// Sending the reply (or the document) failed.
    #[error("transport error: {0}")]
    Transport(#[from] StorageError),
    /// Hydrating the requested file failed.
    #[error("vfs error: {0}")]
    Vfs(#[from] crate::vfs::VfsError),
}

/// Help text: the Python baseline (`telegram_client.py:99-107`) verbatim
/// plus the five tier-1 command lines (self-designed, inserted ahead of
/// the trailing info footer).
const HELP_TEXT: &str = "🚀 **CyDrive Cloud Storage Engine v2.0**\nDeveloped by Cynet Security Team (https://cynetx.ir)\n\n**Available Commands:**\n📊 `/stats` - View cloud storage analytics\n🔍 `/search <query>` - Search files in your drive\n📥 `/get <filename>` - Download a file directly\n📂 `/ls [path]` - List a directory\n📁 `/mkdir <path>` - Create a directory\n🗑️ `/rm <path>` - Delete a file\n💾 `/quota` - View storage usage\n📋 `/queue` - View the upload queue\nℹ️ Send any file to this chat to save it to your Windows Drive!";

/// Max rows of a `/search` reply (Python `results[:15]`).
const SEARCH_LIMIT: usize = 15;

/// Max entry rows of a `/ls` reply; the overflow folds into one
/// `… and {n} more` line (tier-1, self-designed).
const LS_LIMIT: usize = 20;

/// Bytes per MiB — the stats size math (`total_bytes / (1024 * 1024)`).
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// Handles one bot command text; replies via transport (chat = configured chat).
///
/// Unknown text produces no reply (`Ok(())` without sending), mirroring
/// the Python `if/elif` chain falling through.
pub async fn handle_command(
    db: &Arc<MetaDatabase>,
    vfs: &Vfs,
    transport: &dyn ChatCap,
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
    } else if text.starts_with("/ls") {
        handle_ls(db, transport, text).await?;
    } else if text.starts_with("/mkdir") {
        handle_mkdir(vfs, transport, text).await?;
    } else if text.starts_with("/rm") {
        handle_rm(vfs, transport, text).await?;
    } else if text.starts_with("/quota") {
        handle_quota(db, transport, drive_letter).await?;
    } else if text.starts_with("/queue") {
        handle_queue(db, vfs, transport).await?;
    }
    Ok(())
}

/// `/search <query>`: missing-arg warning, no-results notice, or the
/// top-15 result rows (dirs first via the DB's ordering, folder/file
/// icon, `size // 1024` KB — floor division, Python `//` parity).
async fn handle_search(
    db: &Arc<MetaDatabase>,
    transport: &dyn ChatCap,
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
    transport: &dyn ChatCap,
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

/// `/ls [path]`: the root (or an explicit directory path) lists its
/// children — a header line echoing the path, then up to [`LS_LIMIT`]
/// entry rows in the DB's order (dirs first, name ASC), folding the
/// overflow into one `… and {n} more` line; an empty directory notes
/// `(empty)`. A plain file path answers with that file's single entry
/// line; a missing path answers `no such directory`. All wording is
/// self-designed (tier-1, no Python baseline).
async fn handle_ls(
    db: &Arc<MetaDatabase>,
    transport: &dyn ChatCap,
    text: &str,
) -> Result<(), BotError> {
    // The path defaults to the root (`/ls` == `/ls /`).
    let token = split_maxsplit1(text).unwrap_or("/");
    let Some(rel) = token_to_rel_path(token) else {
        transport
            .send_text(&format!("no such directory: {token}"))
            .await?;
        return Ok(());
    };
    // The root has no row of its own — it is always a listable directory.
    let row = if rel.is_root() {
        None
    } else {
        db.get_file(rel.as_str())?
    };
    match row {
        // A file path answers with its single entry line, nothing else.
        Some(row) if !row.is_dir => {
            let size_kb = row.size.div_euclid(1024);
            transport
                .send_text(&format!("f {} ({size_kb} KB)", row.name))
                .await?;
            return Ok(());
        }
        // Missing, and not the root: the directory does not exist.
        None if !rel.is_root() => {
            transport
                .send_text(&format!("no such directory: {token}"))
                .await?;
            return Ok(());
        }
        // A directory row, or the root: listed below.
        _ => {}
    }
    let entries = db.list_dir(rel.as_str())?;
    let mut lines = vec![format!("📁 {}", rel.as_str())];
    if entries.is_empty() {
        lines.push("(empty)".to_string());
    } else {
        for entry in entries.iter().take(LS_LIMIT) {
            if entry.is_dir {
                lines.push(format!("d {}/", entry.name));
            } else {
                // KB floor division, same as /search.
                let size_kb = entry.size.div_euclid(1024);
                lines.push(format!("f {} ({size_kb} KB)", entry.name));
            }
        }
        let rest = entries.len().saturating_sub(LS_LIMIT);
        if rest > 0 {
            lines.push(format!("… and {rest} more"));
        }
    }
    transport.send_text(&lines.join("\n")).await?;
    Ok(())
}

/// `/mkdir <path>`: creates the directory row through [`Vfs::create_dir`]
/// (DB only — no filesystem directory, mirroring the WebDAV layer);
/// `Exists` / `ParentMissing` get their own short replies, anything else
/// propagates. Self-designed wording (tier-1).
async fn handle_mkdir(vfs: &Vfs, transport: &dyn ChatCap, text: &str) -> Result<(), BotError> {
    let Some(token) = split_maxsplit1(text) else {
        transport.send_text("usage: /mkdir <path>").await?;
        return Ok(());
    };
    // A polluted token cannot name a path; the usage line is the closest
    // honest answer.
    let Some(rel) = token_to_rel_path(token) else {
        transport.send_text("usage: /mkdir <path>").await?;
        return Ok(());
    };
    match vfs.create_dir(&rel) {
        Ok(()) => transport.send_text(&format!("created: {token}")).await?,
        Err(VfsError::Exists(_)) => {
            transport
                .send_text(&format!("already exists: {token}"))
                .await?
        }
        Err(VfsError::ParentMissing(_)) => {
            transport
                .send_text(&format!("parent missing: {token}"))
                .await?
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// `/rm <path>`: deletes the file's metadata row and its local cache copy
/// ([`Vfs::remove_file`]); the remote Telegram messages are deliberately
/// kept (Python parity) and the reply says so. Self-designed wording
/// (tier-1).
async fn handle_rm(vfs: &Vfs, transport: &dyn ChatCap, text: &str) -> Result<(), BotError> {
    let Some(token) = split_maxsplit1(text) else {
        transport.send_text("usage: /rm <path>").await?;
        return Ok(());
    };
    // A polluted token cannot name a stored path — "no such file" it is.
    let Some(rel) = token_to_rel_path(token) else {
        transport
            .send_text(&format!("no such file: {token}"))
            .await?;
        return Ok(());
    };
    match vfs.remove_file(&rel).await {
        Ok(()) => {
            transport
                .send_text(&format!(
                    "deleted: {token} (remote Telegram messages are kept)"
                ))
                .await?
        }
        Err(VfsError::NotFound(_)) => {
            transport
                .send_text(&format!("no such file: {token}"))
                .await?
        }
        Err(VfsError::IsDirectory(_)) => {
            transport
                .send_text(&format!("is a directory: {token}"))
                .await?
        }
        // The refused delete (review H2 / plan F2): the local cache copy
        // is still the only copy of the bytes — tell the user to retry
        // once the upload lands.
        Err(VfsError::UploadPending(_)) => {
            transport
                .send_text(&format!(
                    "still uploading, try again after it finishes: {token}"
                ))
                .await?
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// `/quota`: the mapped drive letter, floor-divided MB total, dir count
/// and the pending-upload tally (`Stats::pending_uploads`). Self-designed
/// wording (tier-1).
async fn handle_quota(
    db: &Arc<MetaDatabase>,
    transport: &dyn ChatCap,
    drive_letter: &str,
) -> Result<(), BotError> {
    let stats = db.get_stats()?;
    // MB floor division, /stats style (`total_bytes // (1024*1024)`).
    let total_mb = stats.total_bytes.div_euclid(1024 * 1024);
    let quota_text = format!(
        "💾 /{drive_letter}\nfiles: {} ({total_mb} MB)\ndirs: {}\npending uploads: {}",
        stats.total_files, stats.total_dirs, stats.pending_uploads
    );
    transport.send_text(&quota_text).await?;
    Ok(())
}

/// `/queue`: the four atomic queue counters plus the DB pending-uploads
/// tally — the exact two sources the dashboard's `/api/queue` combines.
/// One line, self-designed wording (tier-1).
async fn handle_queue(
    db: &Arc<MetaDatabase>,
    vfs: &Vfs,
    transport: &dyn ChatCap,
) -> Result<(), BotError> {
    let queue = vfs.queue_stats();
    let pending = db.get_stats()?.pending_uploads;
    transport
        .send_text(&format!(
            "queue: enqueued={} succeeded={} retries={} degraded={} pending={pending}",
            queue.enqueued, queue.succeeded, queue.retries, queue.degraded
        ))
        .await?;
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
