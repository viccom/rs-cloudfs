//! The `cydrive volumes` subcommand and the multi-volume face of
//! `cydrive status` (Phase 2.5 / MV4): read-only listings over the
//! discovered volume manifest.
//!
//! Design boundary: **configuration facts only** — these commands do not
//! talk to a running instance, so they never fabricate runtime state
//! (live status belongs to `cydrive status`'s probes and the dashboard's
//! `/api/volumes`). Reads are side-effect-free by construction: the K21
//! path resolution shares the assembly's anchor (`<volumes_dir>/<name>/`)
//! and `./`-stripping but skips its `create_dir_all`, and db reads guard
//! on `exists()` first because [`MetaDatabase::open`] would CREATE the
//! file — the same reason doctor's db check guards (a listing command
//! must not materialize a volume's state directories).

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use cloudkit_core::config::VolumeConfig;
use cloudkit_core::database::{MetaDatabase, Stats};

use crate::{volume_home, DiscoveredConfig};

// ------------------------------------------------------- volumes listing ---

/// One row of the `cydrive volumes` listing: the configuration facts of
/// one discovered volume. `drive_letter` uses presence semantics (K27):
/// `None` means the volume file did not set the key — such a volume
/// mounts nothing by default and the rendered table shows the `-`
/// placeholder, never the parsed `"Y:"` default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRow {
    /// The volume's name (the file stem).
    pub name: String,
    /// The volume's backend (`"local"` / `"telegram"` / `"baidu"`).
    pub backend: String,
    /// The explicitly-claimed drive letter (`"V:"`), `None` when the
    /// volume file left the key unset.
    pub drive_letter: Option<String>,
    /// The volume's metadata db path, K21-resolved into the volume home
    /// (read-only resolution — no directory is created).
    pub db_path: PathBuf,
    /// The volume file's own path on disk.
    pub file_path: PathBuf,
}

/// Resolves one volume's db path read-only (K21): the same anchor and
/// `./`-stripping as [`crate::resolve_volume_settings`], minus its
/// `create_dir_all` side effect — a listing must not materialize the
/// volume home.
fn readonly_db_path(spec: &VolumeConfig) -> Result<PathBuf> {
    let home = volume_home(spec)?;
    Ok(crate::resolve_volume_path(&home, &spec.settings.db_path))
}

/// Canonical display form of a claimed drive letter (`"v"` / `"V"` /
/// `"V:"` all render `"V:"`) — the mounter's own canonicalisation.
fn canonical_letter(letter: &str) -> String {
    format!("{}:", letter.trim_end_matches(':').to_ascii_uppercase())
}

/// Builds the listing rows for a discovered volume manifest, in the
/// manifest's stable file-name order.
pub fn collect_volume_rows(volumes: &[VolumeConfig]) -> Result<Vec<VolumeRow>> {
    volumes
        .iter()
        .map(|spec| {
            Ok(VolumeRow {
                name: spec.name.clone(),
                backend: spec.settings.backend.as_str().to_string(),
                drive_letter: spec
                    .explicit_drive_letter
                    .then(|| canonical_letter(&spec.settings.drive_letter)),
                db_path: readonly_db_path(spec)?,
                file_path: spec.file_path.clone(),
            })
        })
        .collect()
}

/// Renders the listing table: one row per volume, the drive column `-`
/// for unclaimed letters.
pub fn render_volume_rows(rows: &[VolumeRow]) -> String {
    use comfy_table::{Cell, Table};
    let mut table = Table::new();
    table.set_header(vec![
        Cell::new("Volume"),
        Cell::new("Backend"),
        Cell::new("Drive"),
        Cell::new("Database"),
        Cell::new("Volume file"),
    ]);
    for row in rows {
        table.add_row(vec![
            Cell::new(&row.name),
            Cell::new(&row.backend),
            Cell::new(row.drive_letter.as_deref().unwrap_or("-")),
            Cell::new(row.db_path.display().to_string()),
            Cell::new(row.file_path.display().to_string()),
        ]);
    }
    table.to_string()
}

/// The single-volume-mode answer: names the mode and carries the one-line
/// migration hint (set `volumes_dir`, move the volume-scoped keys into
/// per-volume files).
pub fn render_single_volume_hint() -> String {
    "Single-volume mode: config.toml carries no `volumes_dir`, so this process serves one \
     drive described by config.toml itself.\n\
     To run multiple volumes: set `volumes_dir = \"volumes\"` in config.toml, move the \
     volume-scoped keys (backend, credentials, db_path, drive_letter, ...) into one \
     `volumes/<name>.toml` file per volume, then run `cydrive volumes` again."
        .to_string()
}

/// The whole `cydrive volumes` output for a discovery outcome: the
/// manifest table in multi-volume mode, the migration hint in
/// single-volume mode. Discovery failures never reach here — the command
/// propagates the discoverer's actionable error instead (a missing or
/// empty volumes directory is a config problem, not an empty listing).
pub fn volumes_report(discovered: &DiscoveredConfig) -> Result<String> {
    match discovered {
        DiscoveredConfig::Multi { volumes, .. } => {
            let rows = collect_volume_rows(volumes)?;
            let mut report = String::new();
            let _ = writeln!(report, "{} volume(s) configured:", rows.len());
            let _ = write!(report, "{}", render_volume_rows(&rows));
            Ok(report)
        }
        DiscoveredConfig::Single(_) => Ok(render_single_volume_hint()),
    }
}

// ---------------------------------------------------- status volume stats ---

/// Renders the RV2 runtime-volume section `cydrive status` shows when a
/// live instance answers its `LIST` (K48): the control channel's reply
/// (`OK: N volume(s)` + one row per volume) becomes a small section.
/// `None` for an `ERR` reply — the caller words that case itself.
pub fn format_runtime_volumes_section(reply: &str) -> Option<String> {
    let mut lines = reply.lines();
    let header = lines.next()?;
    if !header.starts_with("OK:") {
        return None;
    }
    let mut section = String::from("runtime volumes (live, via the control channel):\n");
    for line in lines {
        section.push_str(line.trim_end());
        section.push('\n');
    }
    Some(section)
}

/// One volume's db read outcome for the multi-volume `status` report.
#[derive(Debug, Clone)]
pub enum VolumeDbOutcome {
    /// The db opened and answered `get_stats`.
    Stats(Stats),
    /// The db file does not exist yet (a volume that never ran) — a
    /// status read must not create it.
    NotCreatedYet,
    /// The file exists but could not be opened or read (the error text
    /// carried for the table).
    Unreadable(String),
}

/// One volume's row in the multi-volume `status` stats table.
#[derive(Debug, Clone)]
pub struct VolumeStatsRow {
    /// The volume's name.
    pub name: String,
    /// The volume's backend.
    pub backend: String,
    /// The K21-resolved db path that was read.
    pub db_path: PathBuf,
    /// The read outcome.
    pub outcome: VolumeDbOutcome,
}

/// Reads every volume's metadata db **read-only** (works with the process
/// down — the same open + `get_stats` read the single-volume `stats`
/// command uses), guarded on `exists()` so a fresh volume reports
/// [`VolumeDbOutcome::NotCreatedYet`] instead of being materialized.
pub fn collect_volume_stats(volumes: &[VolumeConfig]) -> Result<Vec<VolumeStatsRow>> {
    volumes
        .iter()
        .map(|spec| {
            let db_path = readonly_db_path(spec)?;
            let outcome = if !db_path.exists() {
                VolumeDbOutcome::NotCreatedYet
            } else {
                match MetaDatabase::open(&db_path).and_then(|db| db.get_stats()) {
                    Ok(stats) => VolumeDbOutcome::Stats(stats),
                    Err(error) => VolumeDbOutcome::Unreadable(format!("{error}")),
                }
            };
            Ok(VolumeStatsRow {
                name: spec.name.clone(),
                backend: spec.settings.backend.as_str().to_string(),
                db_path,
                outcome,
            })
        })
        .collect()
}

/// Renders the multi-volume status stats section: the volume-listing
/// header line, then one table row per volume with the files/storage/
/// uploaded/pending columns (the `stats` command's own numbers) and the
/// db location (with the not-created / unreadable notes).
pub fn render_volume_stats(rows: &[VolumeStatsRow]) -> String {
    use comfy_table::{Cell, Table};
    let mut report = String::new();
    let names = rows
        .iter()
        .map(|row| row.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(report, "volumes: {} configured ({})", rows.len(), names);
    let _ = writeln!(report);
    let mut table = Table::new();
    table.set_header(vec![
        Cell::new("Volume"),
        Cell::new("Backend"),
        Cell::new("Files"),
        Cell::new("Storage"),
        Cell::new("Uploaded"),
        Cell::new("Pending"),
        Cell::new("Database"),
    ]);
    for row in rows {
        let (files, storage, uploaded, pending, note) = match &row.outcome {
            VolumeDbOutcome::Stats(stats) => (
                stats.total_files.to_string(),
                crate::format_storage_size(stats.total_bytes),
                stats.uploaded_files.to_string(),
                stats.pending_uploads.to_string(),
                String::new(),
            ),
            VolumeDbOutcome::NotCreatedYet => (
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                " (not created yet)".to_string(),
            ),
            VolumeDbOutcome::Unreadable(error) => (
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                format!(" (unreadable: {error})"),
            ),
        };
        table.add_row(vec![
            Cell::new(&row.name),
            Cell::new(&row.backend),
            Cell::new(files),
            Cell::new(storage),
            Cell::new(uploaded),
            Cell::new(pending),
            Cell::new(format!("{}{note}", row.db_path.display())),
        ]);
    }
    let _ = write!(report, "{table}");
    report
}
