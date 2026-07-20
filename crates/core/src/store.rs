use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::crypto::SessionCrypto;
use crate::model::{
    ArchiveDeletePreview, ArchiveSession, CaptureQuality, CaptureResolution, FrameEvidence,
    Preferences, PurgeResult, SessionSummary, SharedFrame, TimelineEvent, TimelinePage,
};
use crate::presentation::{PresentedTimeline, group_presented_events, present_observed_events};
use crate::process::ffmpeg_command;

pub const SESSION_DB: &str = "session.sqlite3";
pub const MEDIA_FILE: &str = "capture.mp4";
const PREFERENCES_KEY: &str = "preferences_v1";
const ACTIVE_ARCHIVE_SESSION_KEY: &str = "active_archive_session_id";
const DEFAULT_ARCHIVE_SESSION_NAME: &str = "Default Session";

pub fn data_root() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base).join("CdxVidExt"))
}

fn validate_storage_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("storage locations must be absolute Windows paths");
    }
    fs::create_dir_all(path)
        .with_context(|| format!("could not create storage location {}", path.display()))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("could not resolve storage location {}", path.display()))?;
    let probe = canonical.join(format!(".cdxvidext-write-probe-{}", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .with_context(|| format!("storage location is not writable: {}", canonical.display()))?;
    file.write_all(b"Flight Recorder storage check")?;
    drop(file);
    fs::remove_file(&probe)?;
    Ok(canonical)
}

fn update_retention_activation(
    current: &crate::model::RetentionPolicy,
    requested: &mut crate::model::RetentionPolicy,
) {
    requested.applies_after_utc = if requested.enabled {
        if current.enabled {
            current
                .applies_after_utc
                .clone()
                .or_else(|| Some(Utc::now().to_rfc3339()))
        } else {
            Some(Utc::now().to_rfc3339())
        }
    } else {
        None
    };
}

fn active_retention_boundary(
    policy: &crate::model::RetentionPolicy,
) -> Result<Option<DateTime<Utc>>> {
    if !policy.enabled {
        return Ok(None);
    }
    policy
        .applies_after_utc
        .as_deref()
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&Utc))
                .context("retention activation timestamp is invalid")
        })
        .transpose()
}

fn normalize_archive_name(value: &str) -> String {
    value.trim().to_lowercase()
}

fn validate_archive_name(value: &str) -> Result<(&str, String)> {
    let trimmed = value.trim();
    let length = trimmed.chars().count();
    if !(1..=80).contains(&length) {
        bail!("session name must contain between 1 and 80 characters");
    }
    Ok((trimmed, normalize_archive_name(trimmed)))
}

fn validate_existing_descendant(root: &Path, path: &Path) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .with_context(|| format!("could not resolve storage root {}", root.display()))?;
    let path = path
        .canonicalize()
        .with_context(|| format!("could not resolve registered evidence {}", path.display()))?;
    if path == root || !path.starts_with(&root) {
        bail!(
            "refusing to delete registered evidence outside its storage root: {}",
            path.display()
        );
    }
    Ok(path)
}

pub struct Store {
    root: PathBuf,
    index: Mutex<Connection>,
}

impl Store {
    pub fn open_default() -> Result<Arc<Self>> {
        Self::open(data_root()?)
    }

    pub fn open(root: PathBuf) -> Result<Arc<Self>> {
        fs::create_dir_all(root.join("sessions"))?;
        fs::create_dir_all(root.join("exports"))?;
        let index_path = root.join("index.sqlite3");
        let connection = Connection::open(&index_path)
            .with_context(|| format!("failed to open {}", index_path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                started_at_utc TEXT NOT NULL,
                ended_at_utc TEXT,
                state TEXT NOT NULL,
                duration_ms INTEGER,
                monitor_name TEXT NOT NULL,
                output_width INTEGER NOT NULL,
                output_height INTEGER NOT NULL,
                frame_count INTEGER NOT NULL DEFAULT 0,
                event_count INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                media_path TEXT NOT NULL,
                display_name TEXT,
                storage_root TEXT,
                session_path TEXT,
                archive_session_id TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at_utc DESC);
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS shared_frames (
                share_id TEXT PRIMARY KEY,
                snapshot_id TEXT,
                session_id TEXT NOT NULL,
                requested_offset_ms INTEGER NOT NULL,
                frame_number INTEGER NOT NULL,
                offset_100ns INTEGER NOT NULL,
                offset_ms REAL NOT NULL,
                image_path TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                nearest_event_json TEXT,
                archive_session_id TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_shared_frames_created
                ON shared_frames(created_at_utc ASC);
            CREATE TABLE IF NOT EXISTS snapshot_exports (
                snapshot_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                requested_offset_ms INTEGER NOT NULL,
                frame_number INTEGER NOT NULL,
                offset_100ns INTEGER NOT NULL,
                offset_ms REAL NOT NULL,
                image_path TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                created_at_utc TEXT NOT NULL,
                nearest_event_json TEXT,
                archive_session_id TEXT,
                storage_root TEXT,
                UNIQUE(session_id, requested_offset_ms)
            );
            CREATE INDEX IF NOT EXISTS idx_snapshot_exports_created
                ON snapshot_exports(created_at_utc ASC);
            CREATE TABLE IF NOT EXISTS archive_sessions (
                archive_session_id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                normalized_name TEXT NOT NULL UNIQUE,
                created_at_utc TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_archive_sessions_created
                ON archive_sessions(created_at_utc DESC);
            ",
        )?;
        let has_display_name = {
            let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
            let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
            columns
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .any(|name| name == "display_name")
        };
        if !has_display_name {
            connection.execute("ALTER TABLE sessions ADD COLUMN display_name TEXT", [])?;
        }
        let session_columns = {
            let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !session_columns.iter().any(|name| name == "storage_root") {
            connection.execute("ALTER TABLE sessions ADD COLUMN storage_root TEXT", [])?;
        }
        if !session_columns.iter().any(|name| name == "session_path") {
            connection.execute("ALTER TABLE sessions ADD COLUMN session_path TEXT", [])?;
        }
        if !session_columns
            .iter()
            .any(|name| name == "archive_session_id")
        {
            connection.execute(
                "ALTER TABLE sessions ADD COLUMN archive_session_id TEXT",
                [],
            )?;
        }
        let shared_columns = {
            let mut statement = connection.prepare("PRAGMA table_info(shared_frames)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !shared_columns.iter().any(|name| name == "snapshot_id") {
            connection.execute("ALTER TABLE shared_frames ADD COLUMN snapshot_id TEXT", [])?;
        }
        if !shared_columns
            .iter()
            .any(|name| name == "nearest_event_json")
        {
            connection.execute(
                "ALTER TABLE shared_frames ADD COLUMN nearest_event_json TEXT",
                [],
            )?;
        }
        if !shared_columns
            .iter()
            .any(|name| name == "archive_session_id")
        {
            connection.execute(
                "ALTER TABLE shared_frames ADD COLUMN archive_session_id TEXT",
                [],
            )?;
        }
        let snapshot_columns = {
            let mut statement = connection.prepare("PRAGMA table_info(snapshot_exports)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !snapshot_columns
            .iter()
            .any(|name| name == "archive_session_id")
        {
            connection.execute(
                "ALTER TABLE snapshot_exports ADD COLUMN archive_session_id TEXT",
                [],
            )?;
        }
        if !snapshot_columns.iter().any(|name| name == "storage_root") {
            connection.execute(
                "ALTER TABLE snapshot_exports ADD COLUMN storage_root TEXT",
                [],
            )?;
        }
        let legacy_snapshots = {
            let mut statement = connection.prepare(
                "SELECT share_id, session_id, requested_offset_ms, frame_number,
                        offset_100ns, offset_ms, image_path, mime_type,
                        created_at_utc, nearest_event_json
                 FROM shared_frames WHERE snapshot_id IS NULL",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, f64>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (
            share_id,
            session_id,
            requested_offset_ms,
            frame_number,
            offset_100ns,
            offset_ms,
            image_path,
            mime_type,
            created_at_utc,
            nearest_event_json,
        ) in legacy_snapshots
        {
            let existing_snapshot_id = connection
                .query_row(
                    "SELECT snapshot_id FROM snapshot_exports
                     WHERE session_id = ?1 AND requested_offset_ms = ?2",
                    params![session_id, requested_offset_ms],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let snapshot_id =
                existing_snapshot_id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
            connection.execute(
                "INSERT OR IGNORE INTO snapshot_exports(
                    snapshot_id, session_id, requested_offset_ms, frame_number,
                    offset_100ns, offset_ms, image_path, mime_type,
                    created_at_utc, nearest_event_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    snapshot_id,
                    session_id,
                    requested_offset_ms,
                    frame_number,
                    offset_100ns,
                    offset_ms,
                    image_path,
                    mime_type,
                    created_at_utc,
                    nearest_event_json,
                ],
            )?;
            connection.execute(
                "UPDATE shared_frames SET snapshot_id = ?2 WHERE share_id = ?1",
                params![share_id, snapshot_id],
            )?;
        }
        let legacy_paths = {
            let mut statement = connection.prepare(
                "SELECT session_id, media_path, storage_root, session_path
                 FROM sessions WHERE storage_root IS NULL OR session_path IS NULL",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (session_id, media_path, stored_root, stored_session) in legacy_paths {
            let media_path = PathBuf::from(media_path);
            let session_path = stored_session
                .map(PathBuf::from)
                .or_else(|| media_path.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| root.join("sessions").join(&session_id));
            let storage_root = stored_root
                .map(PathBuf::from)
                .or_else(|| session_path.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| root.join("sessions"));
            connection.execute(
                "UPDATE sessions SET storage_root = ?2, session_path = ?3 WHERE session_id = ?1",
                params![
                    session_id,
                    storage_root.to_string_lossy(),
                    session_path.to_string_lossy()
                ],
            )?;
        }
        let selected_archive = connection
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [ACTIVE_ARCHIVE_SESSION_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .filter(|archive_id| {
                connection
                    .query_row(
                        "SELECT 1 FROM archive_sessions WHERE archive_session_id = ?1",
                        [archive_id],
                        |_| Ok(()),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .is_some()
            });
        let archive_session_id = if let Some(archive_id) = selected_archive {
            archive_id
        } else if let Some(archive_id) = connection
            .query_row(
                "SELECT archive_session_id FROM archive_sessions
                 ORDER BY created_at_utc DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            archive_id
        } else {
            let archive_id = uuid::Uuid::now_v7().to_string();
            connection.execute(
                "INSERT INTO archive_sessions(
                    archive_session_id, display_name, normalized_name, created_at_utc
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    archive_id,
                    DEFAULT_ARCHIVE_SESSION_NAME,
                    normalize_archive_name(DEFAULT_ARCHIVE_SESSION_NAME),
                    Utc::now().to_rfc3339(),
                ],
            )?;
            archive_id
        };
        connection.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ACTIVE_ARCHIVE_SESSION_KEY, archive_session_id],
        )?;
        connection.execute(
            "UPDATE sessions SET archive_session_id = ?1 WHERE archive_session_id IS NULL",
            [&archive_session_id],
        )?;
        connection.execute(
            "UPDATE snapshot_exports
             SET archive_session_id = COALESCE(
                 (SELECT sessions.archive_session_id FROM sessions
                  WHERE sessions.session_id = snapshot_exports.session_id), ?1)
             WHERE archive_session_id IS NULL",
            [&archive_session_id],
        )?;
        connection.execute(
            "UPDATE shared_frames
             SET archive_session_id = COALESCE(
                 (SELECT snapshot_exports.archive_session_id FROM snapshot_exports
                  WHERE snapshot_exports.snapshot_id = shared_frames.snapshot_id),
                 (SELECT sessions.archive_session_id FROM sessions
                  WHERE sessions.session_id = shared_frames.session_id), ?1)
             WHERE archive_session_id IS NULL",
            [&archive_session_id],
        )?;
        let snapshots_without_roots = {
            let mut statement = connection.prepare(
                "SELECT snapshot_id, image_path FROM snapshot_exports WHERE storage_root IS NULL",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (snapshot_id, image_path) in snapshots_without_roots {
            let path = PathBuf::from(image_path);
            let storage_root = path
                .parent()
                .and_then(Path::parent)
                .or_else(|| path.parent())
                .unwrap_or(&root)
                .to_string_lossy()
                .into_owned();
            connection.execute(
                "UPDATE snapshot_exports SET storage_root = ?2 WHERE snapshot_id = ?1",
                params![snapshot_id, storage_root],
            )?;
        }
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_archive_started
                 ON sessions(archive_session_id, started_at_utc DESC);
             CREATE INDEX IF NOT EXISTS idx_snapshot_exports_archive
                 ON snapshot_exports(archive_session_id, created_at_utc ASC);
             CREATE INDEX IF NOT EXISTS idx_shared_frames_archive
                 ON shared_frames(archive_session_id, created_at_utc ASC);",
        )?;
        Ok(Arc::new(Self {
            root,
            index: Mutex::new(connection),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list_archive_sessions(&self) -> Result<Vec<ArchiveSession>> {
        let connection = self.index.lock();
        let mut statement = connection.prepare(
            "SELECT archive_session_id, display_name, created_at_utc
             FROM archive_sessions ORDER BY created_at_utc DESC, rowid DESC",
        )?;
        statement
            .query_map([], |row| {
                Ok(ArchiveSession {
                    archive_session_id: row.get(0)?,
                    display_name: row.get(1)?,
                    created_at_utc: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn active_archive_session(&self) -> Result<ArchiveSession> {
        let archive_id = self
            .get_setting(ACTIVE_ARCHIVE_SESSION_KEY)?
            .context("no archive session is selected")?;
        self.get_archive_session(&archive_id)
    }

    pub fn get_archive_session(&self, archive_id: &str) -> Result<ArchiveSession> {
        self.index
            .lock()
            .query_row(
                "SELECT archive_session_id, display_name, created_at_utc
                 FROM archive_sessions WHERE archive_session_id = ?1",
                [archive_id],
                |row| {
                    Ok(ArchiveSession {
                        archive_session_id: row.get(0)?,
                        display_name: row.get(1)?,
                        created_at_utc: row.get(2)?,
                    })
                },
            )
            .with_context(|| format!("session {archive_id} was not found"))
    }

    pub fn create_archive_session(&self, display_name: &str) -> Result<ArchiveSession> {
        let (display_name, normalized_name) = validate_archive_name(display_name)?;
        let archive_id = uuid::Uuid::now_v7().to_string();
        let created_at_utc = Utc::now().to_rfc3339();
        let mut connection = self.index.lock();
        let transaction = connection.transaction()?;
        transaction
            .execute(
                "INSERT INTO archive_sessions(
                    archive_session_id, display_name, normalized_name, created_at_utc
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![archive_id, display_name, normalized_name, created_at_utc],
            )
            .map_err(|error| {
                if error.to_string().contains("UNIQUE constraint failed") {
                    anyhow::anyhow!("a session with that name already exists")
                } else {
                    error.into()
                }
            })?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ACTIVE_ARCHIVE_SESSION_KEY, archive_id],
        )?;
        transaction.commit()?;
        Ok(ArchiveSession {
            archive_session_id: archive_id,
            display_name: display_name.to_owned(),
            created_at_utc,
        })
    }

    pub fn select_archive_session(&self, archive_id: &str) -> Result<ArchiveSession> {
        let archive = self.get_archive_session(archive_id)?;
        self.set_setting(ACTIVE_ARCHIVE_SESSION_KEY, archive_id)?;
        Ok(archive)
    }

    pub fn rename_archive_session(
        &self,
        archive_id: &str,
        display_name: &str,
    ) -> Result<ArchiveSession> {
        let (display_name, normalized_name) = validate_archive_name(display_name)?;
        let changed = self
            .index
            .lock()
            .execute(
                "UPDATE archive_sessions
                 SET display_name = ?2, normalized_name = ?3
                 WHERE archive_session_id = ?1",
                params![archive_id, display_name, normalized_name],
            )
            .map_err(|error| {
                if error.to_string().contains("UNIQUE constraint failed") {
                    anyhow::anyhow!("a session with that name already exists")
                } else {
                    error.into()
                }
            })?;
        if changed == 0 {
            bail!("session {archive_id} was not found");
        }
        self.get_archive_session(archive_id)
    }

    pub fn archive_delete_preview(&self, archive_id: &str) -> Result<ArchiveDeletePreview> {
        let archive = self.get_archive_session(archive_id)?;
        let connection = self.index.lock();
        let (flights, pinned_flights) = connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN pinned != 0 THEN 1 ELSE 0 END), 0)
             FROM sessions WHERE archive_session_id = ?1",
            [archive_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let snapshots = connection.query_row(
            "SELECT COUNT(*) FROM snapshot_exports WHERE archive_session_id = ?1",
            [archive_id],
            |row| row.get::<_, i64>(0),
        )?;
        let shared_frames = connection.query_row(
            "SELECT COUNT(*) FROM shared_frames WHERE archive_session_id = ?1",
            [archive_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(ArchiveDeletePreview {
            archive_session_id: archive.archive_session_id,
            display_name: archive.display_name,
            flights: flights as usize,
            pinned_flights: pinned_flights as usize,
            snapshots: snapshots as usize,
            shared_frames: shared_frames as usize,
        })
    }

    pub fn delete_archive_session(
        &self,
        archive_id: &str,
        expected: &ArchiveDeletePreview,
    ) -> Result<ArchiveSession> {
        let selected = self.active_archive_session()?;
        if selected.archive_session_id != archive_id {
            bail!("only the selected session can be deleted");
        }
        let current = self.archive_delete_preview(archive_id)?;
        if &current != expected {
            bail!("session contents changed; review the updated deletion summary");
        }
        let (flight_paths, snapshot_paths) = {
            let connection = self.index.lock();
            let mut flight_statement = connection.prepare(
                "SELECT storage_root, session_path FROM sessions
                 WHERE archive_session_id = ?1",
            )?;
            let flight_paths = flight_statement
                .query_map([archive_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut snapshot_statement = connection.prepare(
                "SELECT storage_root, image_path FROM snapshot_exports
                 WHERE archive_session_id = ?1",
            )?;
            let snapshot_paths = snapshot_statement
                .query_map([archive_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            (flight_paths, snapshot_paths)
        };
        let mut validated_snapshots = HashSet::new();
        for (root, path) in &snapshot_paths {
            let path = PathBuf::from(path);
            if path.exists() {
                validated_snapshots.insert(validate_existing_descendant(Path::new(root), &path)?);
            }
        }
        let mut validated_flights = HashSet::new();
        for (root, path) in &flight_paths {
            let path = PathBuf::from(path);
            if path.exists() {
                validated_flights.insert(validate_existing_descendant(Path::new(root), &path)?);
            }
        }
        let mut archive_directories = HashSet::new();
        for (root, _) in flight_paths.iter().chain(snapshot_paths.iter()) {
            let root = PathBuf::from(root);
            let archive_dir = root.join(archive_id);
            if archive_dir.exists() {
                archive_directories.insert(validate_existing_descendant(&root, &archive_dir)?);
            }
        }
        for path in validated_snapshots {
            if path.is_file() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to delete snapshot {}", path.display()))?;
            }
        }
        for path in validated_flights {
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to delete flight {}", path.display()))?;
            }
        }
        for path in archive_directories {
            if path.is_dir() {
                fs::remove_dir_all(&path).with_context(|| {
                    format!("failed to delete session directory {}", path.display())
                })?;
            }
        }
        let mut connection = self.index.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM settings
             WHERE key IN ('selected_session', 'selected_offset_ms')
               AND EXISTS(
                   SELECT 1 FROM sessions
                   WHERE sessions.archive_session_id = ?1
                     AND (settings.key = 'selected_offset_ms'
                          OR settings.value = sessions.session_id)
               )",
            [archive_id],
        )?;
        transaction.execute(
            "DELETE FROM shared_frames WHERE archive_session_id = ?1",
            [archive_id],
        )?;
        transaction.execute(
            "DELETE FROM snapshot_exports WHERE archive_session_id = ?1",
            [archive_id],
        )?;
        transaction.execute(
            "DELETE FROM sessions WHERE archive_session_id = ?1",
            [archive_id],
        )?;
        transaction.execute(
            "DELETE FROM archive_sessions WHERE archive_session_id = ?1",
            [archive_id],
        )?;
        let next = transaction
            .query_row(
                "SELECT archive_session_id, display_name, created_at_utc
                 FROM archive_sessions ORDER BY created_at_utc DESC, rowid DESC LIMIT 1",
                [],
                |row| {
                    Ok(ArchiveSession {
                        archive_session_id: row.get(0)?,
                        display_name: row.get(1)?,
                        created_at_utc: row.get(2)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_else(|| ArchiveSession {
                archive_session_id: uuid::Uuid::now_v7().to_string(),
                display_name: DEFAULT_ARCHIVE_SESSION_NAME.to_owned(),
                created_at_utc: Utc::now().to_rfc3339(),
            });
        transaction.execute(
            "INSERT OR IGNORE INTO archive_sessions(
                archive_session_id, display_name, normalized_name, created_at_utc
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                next.archive_session_id,
                next.display_name,
                normalize_archive_name(&next.display_name),
                next.created_at_utc,
            ],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ACTIVE_ARCHIVE_SESSION_KEY, next.archive_session_id],
        )?;
        transaction.commit()?;
        Ok(next)
    }

    pub fn preferences(&self) -> Result<Preferences> {
        if let Some(raw) = self.get_setting(PREFERENCES_KEY)? {
            return serde_json::from_str(&raw).context("stored preferences are invalid");
        }
        let mut defaults = Preferences::defaults_for(&self.root);
        if let Some(days) = self
            .get_setting("retention_days")?
            .and_then(|value| value.parse::<u32>().ok())
        {
            defaults.flight_retention.enabled = true;
            defaults.flight_retention.days = days;
            defaults.flight_retention.applies_after_utc = Some(Utc::now().to_rfc3339());
        }
        self.set_setting(PREFERENCES_KEY, &serde_json::to_string(&defaults)?)?;
        Ok(defaults)
    }

    pub fn save_preferences(&self, mut requested: Preferences) -> Result<Preferences> {
        if requested.flight_retention.days == 0 || requested.snapshot_retention.days == 0 {
            bail!("retention days must be at least one");
        }
        if requested.cutoff_seconds == Some(0) {
            bail!("automatic cutoff must be at least one second");
        }
        let flight_root = validate_storage_root(&requested.flight_root)?;
        let snapshot_root = validate_storage_root(&requested.snapshot_root)?;
        if flight_root == snapshot_root
            || flight_root.starts_with(&snapshot_root)
            || snapshot_root.starts_with(&flight_root)
        {
            bail!("flight and snapshot storage roots must not overlap");
        }
        requested.flight_root = flight_root;
        requested.snapshot_root = snapshot_root;
        let current = self.preferences()?;
        update_retention_activation(&current.flight_retention, &mut requested.flight_retention);
        update_retention_activation(
            &current.snapshot_retention,
            &mut requested.snapshot_retention,
        );
        self.set_setting(PREFERENCES_KEY, &serde_json::to_string(&requested)?)?;
        Ok(requested)
    }

    pub fn recover_stale_sessions(&self) -> Result<usize> {
        let connection = self.index.lock();
        let changed = connection.execute(
            "UPDATE sessions SET state = 'error', ended_at_utc = ?1 WHERE state = 'recording'",
            [Utc::now().to_rfc3339()],
        )?;
        Ok(changed)
    }

    pub fn purge_expired(&self, days: Option<u32>) -> Result<usize> {
        let Some(days) = days else { return Ok(0) };
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(days));
        let candidates = {
            let connection = self.index.lock();
            let mut statement = connection.prepare(
                "SELECT session_id FROM sessions WHERE pinned = 0 AND started_at_utc < ?1",
            )?;
            let rows = statement.query_map([cutoff.to_rfc3339()], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut deleted = 0;
        for session_id in candidates {
            if self
                .session_dir(&session_id)
                .join("exports")
                .read_dir()
                .map(|mut it| it.next().is_some())
                .unwrap_or(false)
            {
                continue;
            }
            if self.delete_session(&session_id).is_ok() {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    pub fn purge_expired_preferences(&self) -> Result<PurgeResult> {
        self.purge_expired_preferences_at(Utc::now())
    }

    fn purge_expired_preferences_at(&self, now: DateTime<Utc>) -> Result<PurgeResult> {
        let preferences = self.preferences()?;
        let mut result = PurgeResult {
            flights_deleted: 0,
            snapshots_deleted: 0,
        };
        if let Some(activation) = active_retention_boundary(&preferences.flight_retention)? {
            let cutoff = now - chrono::Duration::days(i64::from(preferences.flight_retention.days));
            let candidates = {
                let connection = self.index.lock();
                let mut statement = connection.prepare(
                    "SELECT session_id FROM sessions
                     WHERE pinned = 0 AND state NOT IN ('recording', 'finalizing')
                       AND started_at_utc >= ?1 AND started_at_utc < ?2",
                )?;
                statement
                    .query_map(
                        params![activation.to_rfc3339(), cutoff.to_rfc3339()],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for session_id in candidates {
                if self.delete_session(&session_id).is_ok() {
                    result.flights_deleted += 1;
                }
            }
        }
        if let Some(activation) = active_retention_boundary(&preferences.snapshot_retention)? {
            let cutoff =
                now - chrono::Duration::days(i64::from(preferences.snapshot_retention.days));
            let candidates = {
                let connection = self.index.lock();
                let mut statement = connection.prepare(
                    "SELECT snapshot_id, image_path FROM snapshot_exports
                     WHERE created_at_utc >= ?1 AND created_at_utc < ?2",
                )?;
                statement
                    .query_map(
                        params![activation.to_rfc3339(), cutoff.to_rfc3339()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (snapshot_id, image_path) in candidates {
                let path = PathBuf::from(image_path);
                if path.exists() {
                    fs::remove_file(&path)
                        .with_context(|| format!("failed to delete snapshot {}", path.display()))?;
                }
                let mut connection = self.index.lock();
                let transaction = connection.transaction()?;
                transaction.execute(
                    "DELETE FROM shared_frames WHERE snapshot_id = ?1",
                    [&snapshot_id],
                )?;
                transaction.execute(
                    "DELETE FROM snapshot_exports WHERE snapshot_id = ?1",
                    [&snapshot_id],
                )?;
                transaction.commit()?;
                result.snapshots_deleted += 1;
            }
        }
        Ok(result)
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.index
            .lock()
            .query_row(
                "SELECT session_path FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.join("sessions").join(session_id))
    }

    fn archive_session_id_for_flight(&self, session_id: &str) -> Result<String> {
        self.index
            .lock()
            .query_row(
                "SELECT archive_session_id FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .with_context(|| format!("recording session {session_id} was not found"))
    }

    pub fn create_session(
        &self,
        session_id: &str,
        started_at_utc: &str,
        origin_100ns: i64,
        qpc_frequency: i64,
        monitor_name: &str,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Result<Arc<SessionWriter>> {
        let preferences = self.preferences()?;
        let archive = self.active_archive_session()?;
        let dir = preferences
            .flight_root
            .join(&archive.archive_session_id)
            .join(session_id);
        fs::create_dir_all(dir.join("thumbnails"))?;
        fs::create_dir_all(dir.join("exports"))?;
        let writer = Arc::new(SessionWriter::create(
            dir.clone(),
            session_id.to_owned(),
            origin_100ns,
            qpc_frequency,
            monitor_name,
            source_width,
            source_height,
            output_width,
            output_height,
        )?);
        let media_path = dir.join(MEDIA_FILE);
        self.index.lock().execute(
            "INSERT INTO sessions(session_id, started_at_utc, state, monitor_name, output_width, output_height, media_path, storage_root, session_path, archive_session_id)
             VALUES (?1, ?2, 'recording', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                started_at_utc,
                monitor_name,
                output_width,
                output_height,
                media_path.to_string_lossy(),
                preferences.flight_root.to_string_lossy(),
                dir.to_string_lossy(),
                archive.archive_session_id,
            ],
        )?;
        Ok(writer)
    }

    pub fn finalize_session(
        &self,
        session_id: &str,
        duration_ms: i64,
        frame_count: i64,
        event_count: i64,
    ) -> Result<()> {
        self.index.lock().execute(
            "UPDATE sessions SET ended_at_utc = ?2, state = 'ready', duration_ms = ?3, frame_count = ?4, event_count = ?5 WHERE session_id = ?1",
            params![session_id, Utc::now().to_rfc3339(), duration_ms, frame_count, event_count],
        )?;
        Ok(())
    }

    pub fn mark_session_error(&self, session_id: &str) -> Result<()> {
        self.index.lock().execute(
            "UPDATE sessions SET ended_at_utc = ?2, state = 'error' WHERE session_id = ?1",
            params![session_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn list_sessions(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<SessionSummary>, Option<String>)> {
        self.list_sessions_in_archive(None, cursor, limit)
    }

    pub fn list_sessions_for_archive(
        &self,
        archive_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<SessionSummary>, Option<String>)> {
        self.get_archive_session(archive_id)?;
        self.list_sessions_in_archive(Some(archive_id), cursor, limit)
    }

    fn list_sessions_in_archive(
        &self,
        archive_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<SessionSummary>, Option<String>)> {
        let offset = decode_cursor(cursor).unwrap_or(0);
        let limit = limit.clamp(1, 100);
        let connection = self.index.lock();
        let mut statement = connection.prepare(
            "SELECT session_id, started_at_utc, ended_at_utc, state, duration_ms, monitor_name,
                    output_width, output_height, frame_count, event_count, pinned, media_path, display_name
             FROM sessions
             WHERE (?1 IS NULL OR archive_session_id = ?1)
             ORDER BY started_at_utc DESC LIMIT ?2 OFFSET ?3",
        )?;
        let mut rows = statement.query(params![archive_id, (limit + 1) as i64, offset])?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next()? {
            sessions.push(SessionSummary {
                session_id: row.get(0)?,
                started_at_utc: row.get(1)?,
                ended_at_utc: row.get(2)?,
                state: row.get(3)?,
                duration_ms: row.get(4)?,
                monitor_name: row.get(5)?,
                output_width: row.get::<_, i64>(6)? as u32,
                output_height: row.get::<_, i64>(7)? as u32,
                frame_count: row.get(8)?,
                event_count: row.get(9)?,
                pinned: row.get::<_, i64>(10)? != 0,
                media_path: row.get(11)?,
                display_name: row.get(12)?,
            });
        }
        let has_more = sessions.len() > limit;
        sessions.truncate(limit);
        let next = has_more.then(|| encode_cursor(offset + limit as i64));
        Ok((sessions, next))
    }

    pub fn get_session(&self, session_id: &str) -> Result<SessionSummary> {
        self.index
            .lock()
            .query_row(
                "SELECT session_id, started_at_utc, ended_at_utc, state, duration_ms, monitor_name,
                        output_width, output_height, frame_count, event_count, pinned, media_path, display_name
                 FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| {
                    Ok(SessionSummary {
                        session_id: row.get(0)?,
                        started_at_utc: row.get(1)?,
                        ended_at_utc: row.get(2)?,
                        state: row.get(3)?,
                        duration_ms: row.get(4)?,
                        monitor_name: row.get(5)?,
                        output_width: row.get::<_, i64>(6)? as u32,
                        output_height: row.get::<_, i64>(7)? as u32,
                        frame_count: row.get(8)?,
                        event_count: row.get(9)?,
                        pinned: row.get::<_, i64>(10)? != 0,
                        media_path: row.get(11)?,
                        display_name: row.get(12)?,
                    })
                },
            )
            .with_context(|| format!("recording session {session_id} was not found"))
    }

    pub fn timeline(
        &self,
        session_id: &str,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<TimelinePage> {
        let dir = self.session_dir(session_id);
        let connection = Connection::open(dir.join(SESSION_DB))?;
        let after_id = decode_cursor(cursor).unwrap_or(0);
        let start_100ns = start_ms.unwrap_or(0).saturating_mul(10_000);
        let end_100ns = end_ms.unwrap_or(i64::MAX / 10_000).saturating_mul(10_000);
        let limit = limit.clamp(1, 200);
        let mut statement = connection.prepare(
            "SELECT event_id, offset_100ns, source, kind, summary, confidence, tool_use_id,
                    public_payload, sensitive_payload IS NOT NULL
             FROM events
             WHERE event_id > ?1 AND offset_100ns BETWEEN ?2 AND ?3
             ORDER BY event_id ASC LIMIT ?4",
        )?;
        let mut rows = statement.query(params![
            after_id,
            start_100ns,
            end_100ns,
            (limit + 1) as i64
        ])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            let payload: String = row.get(7)?;
            events.push(TimelineEvent {
                event_id: row.get(0)?,
                offset_100ns: row.get(1)?,
                source: row.get(2)?,
                kind: row.get(3)?,
                summary: row.get(4)?,
                confidence: row.get(5)?,
                tool_use_id: row.get(6)?,
                public_payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                has_encrypted_payload: row.get::<_, i64>(8)? != 0,
            });
        }
        let has_more = events.len() > limit;
        events.truncate(limit);
        let next_cursor = has_more
            .then(|| events.last().map(|event| encode_cursor(event.event_id)))
            .flatten();
        Ok(TimelinePage {
            session_id: session_id.to_owned(),
            events,
            next_cursor,
        })
    }

    pub fn presented_timeline(&self, session_id: &str) -> Result<PresentedTimeline> {
        self.get_session(session_id)?;
        let connection = Connection::open(self.session_dir(session_id).join(SESSION_DB))?;
        let events = read_all_events(&connection)?;
        let mut presented = present_observed_events(&events);
        let mut statement =
            connection.prepare("SELECT offset_100ns FROM frames ORDER BY offset_100ns")?;
        let frame_offsets = statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for event in &mut presented {
            if let Some(offset) = nearest_offset(&frame_offsets, event.end_offset_100ns) {
                event.seek_offset_ms = offset / 10_000;
            }
        }
        Ok(group_presented_events(session_id, presented))
    }

    pub fn presented_event_detail(&self, session_id: &str, event_key: &str) -> Result<Value> {
        let timeline = self.presented_timeline(session_id)?;
        let event = timeline
            .categories
            .into_iter()
            .flat_map(|category| category.events)
            .find(|event| event.event_key == event_key)
            .with_context(|| format!("friendly event {event_key} was not found"))?;
        let decrypted = event
            .sensitive_event_id
            .map(|event_id| self.decrypt_event(session_id, event_id))
            .transpose()?;
        Ok(json!({ "event": event, "decrypted": decrypted }))
    }

    pub fn pin_session(&self, session_id: &str, pinned: bool) -> Result<()> {
        let changed = self.index.lock().execute(
            "UPDATE sessions SET pinned = ?2 WHERE session_id = ?1",
            params![session_id, if pinned { 1_i64 } else { 0_i64 }],
        )?;
        if changed == 0 {
            bail!("recording session {session_id} was not found");
        }
        Ok(())
    }

    pub fn rename_session(&self, session_id: &str, display_name: Option<&str>) -> Result<()> {
        let normalized = display_name.map(str::trim);
        if let Some(value) = normalized {
            let length = value.chars().count();
            if !(1..=80).contains(&length) {
                bail!("recording title must contain between 1 and 80 characters");
            }
        }
        let changed = self.index.lock().execute(
            "UPDATE sessions SET display_name = ?2 WHERE session_id = ?1",
            params![session_id, normalized],
        )?;
        if changed == 0 {
            bail!("recording session {session_id} was not found");
        }
        Ok(())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.delete_session_confirmed(session_id, false)
    }

    pub fn delete_session_confirmed(&self, session_id: &str, delete_pinned: bool) -> Result<()> {
        if !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            bail!("invalid session id");
        }
        let session = self.get_session(session_id)?;
        if session.pinned && !delete_pinned {
            bail!("session {session_id} is pinned; unpin it before deletion");
        }
        let storage_root = self.index.lock().query_row(
            "SELECT storage_root FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )?;
        let sessions_root = PathBuf::from(storage_root).canonicalize()?;
        let dir = self.session_dir(session_id).canonicalize()?;
        if !dir.starts_with(&sessions_root) || dir == sessions_root {
            bail!("refusing to delete a path outside the session store");
        }
        self.relocate_session_snapshots(session_id, &dir)?;
        fs::remove_dir_all(&dir).with_context(|| format!("failed to delete {}", dir.display()))?;
        let mut connection = self.index.lock();
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM sessions WHERE session_id = ?1", [session_id])?;
        transaction.commit()?;
        Ok(())
    }

    fn relocate_session_snapshots(&self, session_id: &str, session_dir: &Path) -> Result<()> {
        let snapshots = {
            let connection = self.index.lock();
            let mut statement = connection.prepare(
                "SELECT snapshot_id, image_path FROM snapshot_exports WHERE session_id = ?1",
            )?;
            statement
                .query_map([session_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if snapshots.is_empty() {
            return Ok(());
        }
        let snapshot_root = self.preferences()?.snapshot_root;
        let archive_session_id = self.archive_session_id_for_flight(session_id)?;
        let destination_dir = snapshot_root.join(archive_session_id).join(session_id);
        fs::create_dir_all(&destination_dir)?;
        for (snapshot_id, image_path) in snapshots {
            let source = PathBuf::from(&image_path);
            if !source.exists() || !source.canonicalize()?.starts_with(session_dir) {
                continue;
            }
            let file_name = source
                .file_name()
                .map(|value| value.to_owned())
                .unwrap_or_else(|| format!("snapshot-{snapshot_id}.png").into());
            let mut destination = destination_dir.join(file_name);
            if destination.exists() && destination.canonicalize()? != source.canonicalize()? {
                destination = destination_dir.join(format!("snapshot-{snapshot_id}.png"));
            }
            if destination != source {
                if fs::rename(&source, &destination).is_err() {
                    fs::copy(&source, &destination)?;
                    fs::remove_file(&source)?;
                }
            }
            let destination = destination.to_string_lossy().into_owned();
            let mut connection = self.index.lock();
            let transaction = connection.transaction()?;
            transaction.execute(
                "UPDATE snapshot_exports
                 SET image_path = ?2, storage_root = ?3 WHERE snapshot_id = ?1",
                params![snapshot_id, destination, snapshot_root.to_string_lossy()],
            )?;
            transaction.execute(
                "UPDATE shared_frames SET image_path = ?2 WHERE snapshot_id = ?1",
                params![snapshot_id, destination],
            )?;
            transaction.commit()?;
        }
        Ok(())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.index.lock().execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .index
            .lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn remove_setting(&self, key: &str) -> Result<()> {
        self.index
            .lock()
            .execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn share_frame(&self, session_id: &str, offset_ms: i64) -> Result<SharedFrame> {
        let evidence = self.extract_frame(session_id, offset_ms)?;
        let snapshot_id = self.snapshot_id_for(session_id, offset_ms)?;
        let archive_session_id = self.archive_session_id_for_flight(session_id)?;
        let shared = SharedFrame {
            share_id: uuid::Uuid::now_v7().to_string(),
            session_id: evidence.session_id,
            requested_offset_ms: offset_ms,
            frame_number: evidence.frame_number,
            offset_100ns: evidence.offset_100ns,
            offset_ms: evidence.offset_ms,
            image_path: evidence.image_path,
            mime_type: evidence.mime_type,
            created_at_utc: Utc::now().to_rfc3339(),
            nearest_event: evidence.nearest_event,
        };
        self.index.lock().execute(
            "INSERT INTO shared_frames(
                share_id, snapshot_id, session_id, requested_offset_ms, frame_number, offset_100ns,
                offset_ms, image_path, mime_type, created_at_utc, nearest_event_json,
                archive_session_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                shared.share_id,
                snapshot_id,
                shared.session_id,
                shared.requested_offset_ms,
                shared.frame_number,
                shared.offset_100ns,
                shared.offset_ms,
                shared.image_path,
                shared.mime_type,
                shared.created_at_utc,
                serde_json::to_string(&shared.nearest_event)?,
                archive_session_id,
            ],
        )?;
        Ok(shared)
    }

    pub fn list_shared_frames(&self) -> Result<Vec<SharedFrame>> {
        self.list_shared_frames_in_archive(None)
    }

    pub fn list_shared_frames_for_archive(&self, archive_id: &str) -> Result<Vec<SharedFrame>> {
        self.get_archive_session(archive_id)?;
        self.list_shared_frames_in_archive(Some(archive_id))
    }

    fn list_shared_frames_in_archive(&self, archive_id: Option<&str>) -> Result<Vec<SharedFrame>> {
        let records = {
            let connection = self.index.lock();
            let mut statement = connection.prepare(
                "SELECT share_id, session_id, requested_offset_ms, frame_number, offset_100ns,
                        offset_ms, image_path, mime_type, created_at_utc, nearest_event_json
                 FROM shared_frames
                 WHERE (?1 IS NULL OR archive_session_id = ?1)
                 ORDER BY created_at_utc ASC, rowid ASC",
            )?;
            let rows = statement.query_map([archive_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        records
            .into_iter()
            .map(
                |(
                    share_id,
                    session_id,
                    requested_offset_ms,
                    frame_number,
                    offset_100ns,
                    offset_ms,
                    image_path,
                    mime_type,
                    created_at_utc,
                    nearest_event_json,
                )| {
                    let nearest_event = nearest_event_json
                        .as_deref()
                        .and_then(|value| serde_json::from_str(value).ok())
                        .flatten()
                        .or_else(|| self.nearest_event(&session_id, offset_100ns).ok().flatten());
                    Ok(SharedFrame {
                        share_id,
                        session_id,
                        requested_offset_ms,
                        frame_number,
                        offset_100ns,
                        offset_ms,
                        image_path,
                        mime_type,
                        created_at_utc,
                        nearest_event,
                    })
                },
            )
            .collect()
    }

    pub fn get_shared_frame(&self, share_id: &str) -> Result<SharedFrame> {
        self.list_shared_frames()?
            .into_iter()
            .find(|frame| frame.share_id == share_id)
            .with_context(|| format!("shared frame {share_id} was not found"))
    }

    pub fn latest_shared_frame(&self) -> Result<Option<SharedFrame>> {
        Ok(self.list_shared_frames()?.pop())
    }

    pub fn remove_shared_frame(&self, share_id: &str) -> Result<bool> {
        Ok(self
            .index
            .lock()
            .execute("DELETE FROM shared_frames WHERE share_id = ?1", [share_id])?
            > 0)
    }

    pub fn clear_shared_frames(&self) -> Result<usize> {
        Ok(self.index.lock().execute("DELETE FROM shared_frames", [])?)
    }

    pub fn clear_shared_frames_for_archive(&self, archive_id: &str) -> Result<usize> {
        self.get_archive_session(archive_id)?;
        Ok(self.index.lock().execute(
            "DELETE FROM shared_frames WHERE archive_session_id = ?1",
            [archive_id],
        )?)
    }

    pub fn extract_frame(&self, session_id: &str, offset_ms: i64) -> Result<FrameEvidence> {
        let session = self.get_session(session_id)?;
        let registered = self
            .index
            .lock()
            .query_row(
                "SELECT frame_number, offset_100ns, offset_ms, image_path, mime_type,
                        nearest_event_json
                 FROM snapshot_exports
                 WHERE session_id = ?1 AND requested_offset_ms = ?2",
                params![session_id, offset_ms],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()?;
        if let Some((
            frame_number,
            offset_100ns,
            snapped_offset_ms,
            image_path,
            mime_type,
            nearest_event_json,
        )) = registered
        {
            if Path::new(&image_path).is_file() {
                let nearest_event = nearest_event_json
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .flatten()
                    .or_else(|| self.nearest_event(session_id, offset_100ns).ok().flatten());
                return Ok(FrameEvidence {
                    session_id: session_id.to_owned(),
                    frame_number,
                    offset_100ns,
                    offset_ms: snapped_offset_ms,
                    image_path,
                    mime_type,
                    nearest_event,
                });
            }
        }
        let dir = self.session_dir(session_id);
        let preferences = self.preferences()?;
        let archive_session_id = self.archive_session_id_for_flight(session_id)?;
        let snapshot_dir = preferences
            .snapshot_root
            .join(&archive_session_id)
            .join(session_id);
        fs::create_dir_all(&snapshot_dir)?;
        let image_path = snapshot_dir.join(format!("frame-{offset_ms}.png"));
        if !image_path.exists() {
            let seconds = format!("{:.3}", offset_ms.max(0) as f64 / 1000.0);
            let status = ffmpeg_command()
                .args(["-hide_banner", "-loglevel", "error", "-ss", &seconds, "-i"])
                .arg(&session.media_path)
                .args(["-frames:v", "1", "-y"])
                .arg(&image_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .status()
                .context("failed to launch ffmpeg for frame extraction")?;
            if !status.success() {
                bail!("ffmpeg could not extract a frame at {offset_ms} ms");
            }
        }
        let connection = Connection::open(dir.join(SESSION_DB))?;
        let target = offset_ms.saturating_mul(10_000);
        let (frame_number, offset_100ns): (i64, i64) = connection
            .query_row(
                "SELECT frame_number, offset_100ns FROM frames ORDER BY ABS(offset_100ns - ?1) LIMIT 1",
                [target],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, target));
        let nearest_event = self.nearest_event(session_id, offset_100ns)?;
        let snapshot_id = uuid::Uuid::now_v7().to_string();
        self.index.lock().execute(
            "INSERT INTO snapshot_exports(
                snapshot_id, session_id, requested_offset_ms, frame_number, offset_100ns,
                offset_ms, image_path, mime_type, created_at_utc, nearest_event_json,
                archive_session_id, storage_root
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'image/png', ?8, ?9, ?10, ?11)
             ON CONFLICT(session_id, requested_offset_ms) DO UPDATE SET
                frame_number = excluded.frame_number,
                offset_100ns = excluded.offset_100ns,
                offset_ms = excluded.offset_ms,
                image_path = excluded.image_path,
                nearest_event_json = excluded.nearest_event_json,
                archive_session_id = excluded.archive_session_id,
                storage_root = excluded.storage_root",
            params![
                snapshot_id,
                session_id,
                offset_ms,
                frame_number,
                offset_100ns,
                offset_100ns as f64 / 10_000.0,
                image_path.to_string_lossy(),
                Utc::now().to_rfc3339(),
                serde_json::to_string(&nearest_event)?,
                archive_session_id,
                preferences.snapshot_root.to_string_lossy(),
            ],
        )?;
        let snapshot_id = self.snapshot_id_for(session_id, offset_ms)?;
        self.index.lock().execute(
            "UPDATE shared_frames
             SET frame_number = ?2, offset_100ns = ?3, offset_ms = ?4,
                 image_path = ?5, mime_type = 'image/png', nearest_event_json = ?6
             WHERE snapshot_id = ?1",
            params![
                snapshot_id,
                frame_number,
                offset_100ns,
                offset_100ns as f64 / 10_000.0,
                image_path.to_string_lossy(),
                serde_json::to_string(&nearest_event)?,
            ],
        )?;
        Ok(FrameEvidence {
            session_id: session_id.to_owned(),
            frame_number,
            offset_100ns,
            offset_ms: offset_100ns as f64 / 10_000.0,
            image_path: image_path.to_string_lossy().into_owned(),
            mime_type: "image/png".to_owned(),
            nearest_event,
        })
    }

    fn snapshot_id_for(&self, session_id: &str, offset_ms: i64) -> Result<String> {
        self.index
            .lock()
            .query_row(
                "SELECT snapshot_id FROM snapshot_exports
                 WHERE session_id = ?1 AND requested_offset_ms = ?2",
                params![session_id, offset_ms],
                |row| row.get(0),
            )
            .context("snapshot export was not registered")
    }

    fn nearest_event(&self, session_id: &str, offset_100ns: i64) -> Result<Option<TimelineEvent>> {
        Ok(self
            .timeline(session_id, None, None, None, 200)?
            .events
            .into_iter()
            .min_by_key(|event| (event.offset_100ns - offset_100ns).abs()))
    }

    pub fn generate_thumbnail(&self, session_id: &str, duration_ms: i64) -> Result<PathBuf> {
        let session = self.get_session(session_id)?;
        let output = self
            .session_dir(session_id)
            .join("thumbnails")
            .join("poster.jpg");
        let seconds = format!("{:.3}", duration_ms.max(0) as f64 / 2000.0);
        let status = ffmpeg_command()
            .args(["-hide_banner", "-loglevel", "error", "-ss", &seconds, "-i"])
            .arg(&session.media_path)
            .args(["-frames:v", "1", "-vf", "scale=480:-2", "-y"])
            .arg(&output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("FFmpeg thumbnail generation failed");
        }
        Ok(output)
    }

    pub fn decrypt_event(&self, session_id: &str, event_id: i64) -> Result<Value> {
        let dir = self.session_dir(session_id);
        let connection = Connection::open(dir.join(SESSION_DB))?;
        let encrypted: Vec<u8> = connection.query_row(
            "SELECT sensitive_payload FROM events WHERE event_id = ?1 AND sensitive_payload IS NOT NULL",
            [event_id], |row| row.get(0),
        ).with_context(|| format!("event {event_id} has no encrypted payload"))?;
        let clear = SessionCrypto::open(&dir)?.decrypt(&encrypted)?;
        Ok(serde_json::from_slice(&clear)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&clear).into_owned())))
    }

    pub fn verification_report(&self, session_id: &str) -> Result<Value> {
        let session = self.get_session(session_id)?;
        let connection = Connection::open(self.session_dir(session_id).join(SESSION_DB))?;
        let (frames, duplicated, dropped, max_offset): (i64, i64, i64, i64) = connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(duplicated),0), COALESCE(SUM(dropped_before),0), COALESCE(MAX(offset_100ns),0) FROM frames",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let automatic_cutoff_events: i64 = connection.query_row(
            "SELECT COUNT(*) FROM events WHERE source = 'recorder' AND kind = 'automatic_cutoff'",
            [],
            |row| row.get(0),
        )?;
        let profile_columns = {
            let mut statement = connection.prepare("PRAGMA table_info(session)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let (encoder_name, quality, resolution_mode): (
            Option<String>,
            Option<String>,
            Option<String>,
        ) = if ["encoder_name", "quality", "resolution_mode"]
            .iter()
            .all(|column| profile_columns.iter().any(|candidate| candidate == column))
        {
            connection.query_row(
                "SELECT encoder_name, quality, resolution_mode FROM session LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
        } else {
            (None, None, None)
        };
        let duration_ms = session.duration_ms.unwrap_or(max_offset / 10_000);
        let expected_frames = ((duration_ms.max(0) as f64 / 1000.0) * 30.0).round() as i64;
        // A CFR schedule may legitimately include the slot at t=0 and one
        // boundary slot at finalization.  Extra boundary frames are not
        // "negative misses"; only an actual shortfall is reportable.
        let missing_slots = (expected_frames - frames).max(0);
        let missed_percent = if expected_frames > 0 {
            missing_slots as f64 * 100.0 / expected_frames as f64
        } else {
            0.0
        };
        Ok(json!({
            "session_id": session_id, "duration_ms": duration_ms, "indexed_duration_ms": max_offset / 10_000,
            "frame_count": frames, "expected_frames": expected_frames, "duplicated_frames": duplicated,
            "dropped_source_frames": dropped, "missing_encoder_slots": missing_slots, "missed_percent": missed_percent,
            "duration_delta_frames": ((duration_ms - max_offset / 10_000).abs() as f64 / 33.333).round() as i64,
            "media_path": session.media_path, "output_width": session.output_width,
            "output_height": session.output_height, "encoder_name": encoder_name,
            "quality": quality, "resolution_mode": resolution_mode,
            "automatic_cutoff_events": automatic_cutoff_events
        }))
    }
}

pub struct SessionWriter {
    pub session_id: String,
    pub dir: PathBuf,
    pub origin_100ns: i64,
    pub qpc_frequency: i64,
    connection: Mutex<Connection>,
    crypto: SessionCrypto,
}

impl SessionWriter {
    #[allow(clippy::too_many_arguments)]
    fn create(
        dir: PathBuf,
        session_id: String,
        origin_100ns: i64,
        qpc_frequency: i64,
        monitor_name: &str,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Result<Self> {
        let crypto = SessionCrypto::create(&dir)?;
        let connection = Connection::open(dir.join(SESSION_DB))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "
            CREATE TABLE session (
                session_id TEXT PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                origin_100ns INTEGER NOT NULL,
                qpc_frequency INTEGER NOT NULL,
                monitor_name TEXT NOT NULL,
                source_width INTEGER NOT NULL,
                source_height INTEGER NOT NULL,
                output_width INTEGER NOT NULL,
                output_height INTEGER NOT NULL,
                encoder_name TEXT,
                quality TEXT,
                resolution_mode TEXT
            );
            CREATE TABLE turns (
                turn_id TEXT PRIMARY KEY,
                started_offset_100ns INTEGER NOT NULL,
                ended_offset_100ns INTEGER,
                prompt_length INTEGER,
                prompt_sha256 TEXT
            );
            CREATE TABLE frames (
                frame_number INTEGER PRIMARY KEY,
                offset_100ns INTEGER NOT NULL,
                source_timestamp_100ns INTEGER NOT NULL,
                duplicated INTEGER NOT NULL DEFAULT 0,
                dropped_before INTEGER NOT NULL DEFAULT 0,
                visual_change REAL
            );
            CREATE TABLE events (
                event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                offset_100ns INTEGER NOT NULL,
                source TEXT NOT NULL,
                kind TEXT NOT NULL,
                summary TEXT NOT NULL,
                confidence REAL,
                tool_use_id TEXT,
                public_payload TEXT NOT NULL,
                sensitive_payload BLOB
            );
            CREATE INDEX idx_events_time ON events(offset_100ns);
            CREATE TABLE tool_calls (
                tool_use_id TEXT PRIMARY KEY,
                tool_name TEXT NOT NULL,
                started_offset_100ns INTEGER,
                ended_offset_100ns INTEGER,
                status TEXT
            );
            CREATE TABLE markers (
                marker_id INTEGER PRIMARY KEY AUTOINCREMENT,
                offset_100ns INTEGER NOT NULL,
                label TEXT NOT NULL,
                created_at_utc TEXT NOT NULL
            );
            CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            ",
        )?;
        connection.execute(
            "INSERT INTO session(
                session_id, schema_version, origin_100ns, qpc_frequency, monitor_name,
                source_width, source_height, output_width, output_height
             ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                origin_100ns,
                qpc_frequency,
                monitor_name,
                source_width,
                source_height,
                output_width,
                output_height
            ],
        )?;
        Ok(Self {
            session_id,
            dir,
            origin_100ns,
            qpc_frequency,
            connection: Mutex::new(connection),
            crypto,
        })
    }

    pub fn set_capture_profile(
        &self,
        encoder_name: &str,
        quality: CaptureQuality,
        resolution: CaptureResolution,
    ) -> Result<()> {
        let quality = match quality {
            CaptureQuality::Low => "low",
            CaptureQuality::Medium => "medium",
            CaptureQuality::High => "high",
        };
        let resolution = match resolution {
            CaptureResolution::Hd1080 => "hd1080",
            CaptureResolution::Qhd2k => "qhd2k",
            CaptureResolution::Native => "native",
        };
        self.connection.lock().execute(
            "UPDATE session SET encoder_name = ?1, quality = ?2, resolution_mode = ?3",
            params![encoder_name, quality, resolution],
        )?;
        Ok(())
    }

    pub fn add_turn(
        &self,
        turn_id: &str,
        offset_100ns: i64,
        prompt_length: Option<usize>,
        prompt_sha256: Option<&str>,
    ) -> Result<()> {
        self.connection.lock().execute(
            "INSERT INTO turns(turn_id, started_offset_100ns, prompt_length, prompt_sha256)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(turn_id) DO NOTHING",
            params![
                turn_id,
                offset_100ns,
                prompt_length.map(|v| v as i64),
                prompt_sha256
            ],
        )?;
        Ok(())
    }

    pub fn end_turn(&self, turn_id: &str, offset_100ns: i64) -> Result<()> {
        self.connection.lock().execute(
            "UPDATE turns SET ended_offset_100ns = ?2 WHERE turn_id = ?1",
            params![turn_id, offset_100ns],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_event(
        &self,
        offset_100ns: i64,
        source: &str,
        kind: &str,
        summary: &str,
        confidence: Option<f64>,
        tool_use_id: Option<&str>,
        public_payload: &Value,
        sensitive_payload: Option<&[u8]>,
    ) -> Result<i64> {
        let encrypted = sensitive_payload
            .map(|payload| self.crypto.encrypt(payload))
            .transpose()?;
        let connection = self.connection.lock();
        connection.execute(
            "INSERT INTO events(offset_100ns, source, kind, summary, confidence, tool_use_id, public_payload, sensitive_payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                offset_100ns,
                source,
                kind,
                summary,
                confidence,
                tool_use_id,
                public_payload.to_string(),
                encrypted
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn add_frame(
        &self,
        frame_number: i64,
        offset_100ns: i64,
        source_timestamp_100ns: i64,
        duplicated: bool,
        dropped_before: i64,
        visual_change: Option<f64>,
    ) -> Result<()> {
        self.connection.lock().execute(
            "INSERT OR REPLACE INTO frames(frame_number, offset_100ns, source_timestamp_100ns, duplicated, dropped_before, visual_change)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                frame_number,
                offset_100ns,
                source_timestamp_100ns,
                if duplicated { 1_i64 } else { 0_i64 },
                dropped_before,
                visual_change
            ],
        )?;
        Ok(())
    }

    pub fn upsert_tool_start(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        offset_100ns: i64,
    ) -> Result<()> {
        self.connection.lock().execute(
            "INSERT INTO tool_calls(tool_use_id, tool_name, started_offset_100ns, status)
             VALUES (?1, ?2, ?3, 'running')
             ON CONFLICT(tool_use_id) DO NOTHING",
            params![tool_use_id, tool_name, offset_100ns],
        )?;
        Ok(())
    }

    pub fn upsert_tool_end(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        offset_100ns: i64,
    ) -> Result<()> {
        self.connection.lock().execute(
            "INSERT INTO tool_calls(tool_use_id, tool_name, ended_offset_100ns, status)
             VALUES (?1, ?2, ?3, 'complete')
             ON CONFLICT(tool_use_id) DO UPDATE SET ended_offset_100ns = excluded.ended_offset_100ns, status = 'complete'",
            params![tool_use_id, tool_name, offset_100ns],
        )?;
        Ok(())
    }

    pub fn counts(&self) -> Result<(i64, i64)> {
        let connection = self.connection.lock();
        let frames = connection.query_row("SELECT COUNT(*) FROM frames", [], |row| row.get(0))?;
        let events = connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok((frames, events))
    }

    pub fn correlate_requested_action(
        &self,
        offset_100ns: i64,
    ) -> Result<(Option<String>, Option<f64>)> {
        let connection = self.connection.lock();
        let nearest: Option<(Option<String>, i64)> = connection
            .query_row(
                "SELECT tool_use_id, ABS(offset_100ns - ?1) FROM events
             WHERE source = 'requested_action' AND ABS(offset_100ns - ?1) <= 5000000
             ORDER BY ABS(offset_100ns - ?1) LIMIT 1",
                [offset_100ns],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match nearest {
            Some((tool_use_id, delta)) => (
                tool_use_id,
                Some((1.0 - delta as f64 / 5_000_000.0).clamp(0.0, 1.0)),
            ),
            None => (None, None),
        })
    }
}

fn encode_cursor(value: i64) -> String {
    URL_SAFE_NO_PAD.encode(value.to_le_bytes())
}

fn decode_cursor(cursor: Option<&str>) -> Option<i64> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor?).ok()?;
    let array: [u8; 8] = bytes.try_into().ok()?;
    Some(i64::from_le_bytes(array).max(0))
}

fn read_all_events(connection: &Connection) -> Result<Vec<TimelineEvent>> {
    let mut statement = connection.prepare(
        "SELECT event_id, offset_100ns, source, kind, summary, confidence, tool_use_id,
                public_payload, sensitive_payload IS NOT NULL
         FROM events ORDER BY event_id ASC",
    )?;
    let rows = statement.query_map([], |row| {
        let payload: String = row.get(7)?;
        Ok(TimelineEvent {
            event_id: row.get(0)?,
            offset_100ns: row.get(1)?,
            source: row.get(2)?,
            kind: row.get(3)?,
            summary: row.get(4)?,
            confidence: row.get(5)?,
            tool_use_id: row.get(6)?,
            public_payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            has_encrypted_payload: row.get::<_, i64>(8)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn nearest_offset(offsets: &[i64], target: i64) -> Option<i64> {
    match offsets.binary_search(&target) {
        Ok(index) => offsets.get(index).copied(),
        Err(0) => offsets.first().copied(),
        Err(index) if index >= offsets.len() => offsets.last().copied(),
        Err(index) => {
            let before = offsets[index - 1];
            let after = offsets[index];
            Some(if target - before <= after - target {
                before
            } else {
                after
            })
        }
    }
}

pub fn public_input_event(kind: &str, details: Value) -> Value {
    json!({ "input_kind": kind, "details": details })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CaptureQuality, CaptureResolution, Preferences, RetentionPolicy};

    #[test]
    fn preferences_round_trip_through_real_sqlite_and_directories() {
        let root =
            std::env::temp_dir().join(format!("cdxvidext-preferences-{}", uuid::Uuid::now_v7()));
        let flight_root = root.join("recorded-flights");
        let snapshot_root = root.join("snapshot-images");
        let store = Store::open(root.clone()).unwrap();
        let requested = Preferences {
            flight_root: flight_root.clone(),
            snapshot_root: snapshot_root.clone(),
            flight_retention: RetentionPolicy {
                enabled: true,
                days: 14,
                applies_after_utc: None,
            },
            snapshot_retention: RetentionPolicy {
                enabled: false,
                days: 30,
                applies_after_utc: None,
            },
            cutoff_seconds: Some(125),
            quality: CaptureQuality::High,
            resolution: CaptureResolution::Qhd2k,
        };

        let saved = store.save_preferences(requested).unwrap();
        let loaded = store.preferences().unwrap();

        assert_eq!(loaded, saved);
        assert!(loaded.flight_retention.applies_after_utc.is_some());
        assert!(flight_root.is_dir());
        assert!(snapshot_root.is_dir());
        assert_eq!(loaded.cutoff_seconds, Some(125));
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preferences_reject_overlapping_real_storage_roots() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-preferences-overlap-{}",
            uuid::Uuid::now_v7()
        ));
        let store = Store::open(root.clone()).unwrap();
        let shared_root = root.join("evidence");
        let requested = Preferences {
            flight_root: shared_root.clone(),
            snapshot_root: shared_root.join("snapshots"),
            ..Preferences::defaults_for(&root)
        };

        let error = store.save_preferences(requested).unwrap_err().to_string();

        assert!(error.contains("must not overlap"));
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_flight_uses_the_saved_real_storage_root() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-external-flight-{}",
            uuid::Uuid::now_v7()
        ));
        let flight_root = root.join("external-flights");
        let store = Store::open(root.clone()).unwrap();
        store
            .save_preferences(Preferences {
                flight_root: flight_root.clone(),
                snapshot_root: root.join("external-snapshots"),
                ..Preferences::defaults_for(&root)
            })
            .unwrap();
        let session_id = "018f0000-0000-7000-8000-000000000099";

        let writer = store
            .create_session(
                session_id,
                &Utc::now().to_rfc3339(),
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        drop(writer);

        let archive_id = store.active_archive_session().unwrap().archive_session_id;
        assert!(
            flight_root
                .join(&archive_id)
                .join(session_id)
                .join(SESSION_DB)
                .is_file()
        );
        assert_eq!(
            store.session_dir(session_id),
            flight_root
                .canonicalize()
                .unwrap()
                .join(archive_id)
                .join(session_id)
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_session_paths_migrate_from_real_media_paths() {
        let root =
            std::env::temp_dir().join(format!("cdxvidext-legacy-path-{}", uuid::Uuid::now_v7()));
        let session_id = "legacy-session-p";
        let session_dir = root.join("sessions").join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        let connection = Connection::open(root.join("index.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY, started_at_utc TEXT NOT NULL, ended_at_utc TEXT,
                state TEXT NOT NULL, duration_ms INTEGER, monitor_name TEXT NOT NULL,
                output_width INTEGER NOT NULL, output_height INTEGER NOT NULL,
                frame_count INTEGER NOT NULL DEFAULT 0, event_count INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0, media_path TEXT NOT NULL, display_name TEXT
             );",
            )
            .unwrap();
        connection.execute(
            "INSERT INTO sessions(session_id, started_at_utc, state, monitor_name, output_width, output_height, media_path)
             VALUES (?1, '2026-01-01T00:00:00Z', 'ready', 'Display', 100, 100, ?2)",
            params![session_id, session_dir.join(MEDIA_FILE).to_string_lossy()],
        ).unwrap();
        drop(connection);

        let store = Store::open(root.clone()).unwrap();

        assert_eq!(store.session_dir(session_id), session_dir);
        let default = store.active_archive_session().unwrap();
        assert_eq!(default.display_name, "Default Session");
        assert_eq!(
            store
                .list_sessions_for_archive(&default.archive_session_id, None, 100)
                .unwrap()
                .0[0]
                .session_id,
            session_id
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_snapshot_survives_source_flight_deletion() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-snapshot-survival-{}",
            uuid::Uuid::now_v7()
        ));
        let snapshot_root = root.join("saved-snapshots");
        let store = Store::open(root.clone()).unwrap();
        store
            .save_preferences(Preferences {
                flight_root: root.join("recorded-flights"),
                snapshot_root: snapshot_root.clone(),
                ..Preferences::defaults_for(&root)
            })
            .unwrap();
        let session_id = "018f0000-0000-7000-8000-000000000098";
        let writer = store
            .create_session(
                session_id,
                &Utc::now().to_rfc3339(),
                0,
                10_000_000,
                "Display",
                96,
                64,
                96,
                64,
            )
            .unwrap();
        writer.add_frame(0, 0, 0, false, 0, Some(1.0)).unwrap();
        drop(writer);
        let media_path = store.session_dir(session_id).join(MEDIA_FILE);
        let status = ffmpeg_command()
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=green:s=96x64:d=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&media_path)
            .status()
            .unwrap();
        assert!(status.success());

        let shared = store.share_frame(session_id, 0).unwrap();
        let archive_id = store.active_archive_session().unwrap().archive_session_id;
        let snapshot_path = PathBuf::from(&shared.image_path);
        assert!(snapshot_path.starts_with(snapshot_root.canonicalize().unwrap()));
        assert!(snapshot_path.is_file());

        store.delete_session(session_id).unwrap();

        let remaining = store.list_shared_frames().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].share_id, shared.share_id);
        assert!(PathBuf::from(&remaining[0].image_path).is_file());
        assert_eq!(
            store
                .list_shared_frames_for_archive(&archive_id)
                .unwrap()
                .len(),
            1
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_grandfathers_pre_activation_flights_and_deletes_later_expired_flights() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-retention-cohort-{}",
            uuid::Uuid::now_v7()
        ));
        let store = Store::open(root.clone()).unwrap();
        let grandfathered_id = "018f0000-0000-7000-8000-000000000096";
        let eligible_id = "018f0000-0000-7000-8000-000000000097";
        drop(
            store
                .create_session(
                    grandfathered_id,
                    "2020-01-01T00:00:00Z",
                    0,
                    10_000_000,
                    "Display",
                    100,
                    100,
                    100,
                    100,
                )
                .unwrap(),
        );
        drop(
            store
                .create_session(
                    eligible_id,
                    "2026-01-01T00:00:00Z",
                    0,
                    10_000_000,
                    "Display",
                    100,
                    100,
                    100,
                    100,
                )
                .unwrap(),
        );
        store.finalize_session(grandfathered_id, 0, 0, 0).unwrap();
        store.finalize_session(eligible_id, 0, 0, 0).unwrap();
        let preferences = Preferences {
            flight_retention: RetentionPolicy {
                enabled: true,
                days: 30,
                applies_after_utc: Some("2025-01-01T00:00:00Z".into()),
            },
            ..Preferences::defaults_for(&root)
        };
        store
            .set_setting(
                PREFERENCES_KEY,
                &serde_json::to_string(&preferences).unwrap(),
            )
            .unwrap();

        let result = store
            .purge_expired_preferences_at(
                DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )
            .unwrap();

        assert_eq!(result.flights_deleted, 1);
        assert!(store.session_dir(grandfathered_id).is_dir());
        assert!(store.get_session(eligible_id).is_err());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_retention_removes_real_png_registry_and_shared_row_together() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-snapshot-retention-{}",
            uuid::Uuid::now_v7()
        ));
        let store = Store::open(root.clone()).unwrap();
        let image_path = root.join("exports").join("expired.png");
        let status = ffmpeg_command()
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=96x64",
                "-frames:v",
                "1",
                "-y",
            ])
            .arg(&image_path)
            .status()
            .unwrap();
        assert!(status.success());
        let snapshot_id = uuid::Uuid::now_v7().to_string();
        let share_id = uuid::Uuid::now_v7().to_string();
        store
            .index
            .lock()
            .execute(
                "INSERT INTO snapshot_exports(snapshot_id, session_id, requested_offset_ms, frame_number, offset_100ns, offset_ms, image_path, mime_type, created_at_utc)
                 VALUES (?1, 'expired-session', 0, 0, 0, 0, ?2, 'image/png', '2026-01-01T00:00:00Z')",
                params![snapshot_id, image_path.to_string_lossy()],
            )
            .unwrap();
        store
            .index
            .lock()
            .execute(
                "INSERT INTO shared_frames(share_id, snapshot_id, session_id, requested_offset_ms, frame_number, offset_100ns, offset_ms, image_path, mime_type, created_at_utc)
                 VALUES (?1, ?2, 'expired-session', 0, 0, 0, 0, ?3, 'image/png', '2026-01-01T00:00:00Z')",
                params![share_id, snapshot_id, image_path.to_string_lossy()],
            )
            .unwrap();
        let preferences = Preferences {
            snapshot_retention: RetentionPolicy {
                enabled: true,
                days: 30,
                applies_after_utc: Some("2025-01-01T00:00:00Z".into()),
            },
            ..Preferences::defaults_for(&root)
        };
        store
            .set_setting(
                PREFERENCES_KEY,
                &serde_json::to_string(&preferences).unwrap(),
            )
            .unwrap();

        let result = store
            .purge_expired_preferences_at(
                DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )
            .unwrap();

        assert_eq!(result.snapshots_deleted, 1);
        assert!(!image_path.exists());
        assert!(store.list_shared_frames().unwrap().is_empty());
        let registry_count: i64 = store
            .index
            .lock()
            .query_row("SELECT COUNT(*) FROM snapshot_exports", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(registry_count, 0);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_database_records_requested_profile_and_actual_encoder() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-capture-profile-{}",
            uuid::Uuid::now_v7()
        ));
        let store = Store::open(root.clone()).unwrap();
        let session_id = "018f0000-0000-7000-8000-000000000095";
        let writer = store
            .create_session(
                session_id,
                &Utc::now().to_rfc3339(),
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();

        writer
            .set_capture_profile("libx264", CaptureQuality::High, CaptureResolution::Native)
            .unwrap();

        let connection = Connection::open(store.session_dir(session_id).join(SESSION_DB)).unwrap();
        let values = connection
            .query_row(
                "SELECT encoder_name, quality, resolution_mode FROM session LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(values, ("libx264".into(), "high".into(), "native".into()));
        drop(connection);
        drop(writer);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleting_a_legacy_flight_relocates_its_real_registered_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-legacy-snapshot-{}",
            uuid::Uuid::now_v7()
        ));
        let store = Store::open(root.clone()).unwrap();
        let session_id = "018f0000-0000-7000-8000-000000000094";
        drop(
            store
                .create_session(
                    session_id,
                    &Utc::now().to_rfc3339(),
                    0,
                    10_000_000,
                    "Display",
                    96,
                    64,
                    96,
                    64,
                )
                .unwrap(),
        );
        let legacy_path = store
            .session_dir(session_id)
            .join("exports")
            .join("frame-0.png");
        let status = ffmpeg_command()
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=96x64",
                "-frames:v",
                "1",
                "-y",
            ])
            .arg(&legacy_path)
            .status()
            .unwrap();
        assert!(status.success());
        let snapshot_id = uuid::Uuid::now_v7().to_string();
        let share_id = uuid::Uuid::now_v7().to_string();
        let created = Utc::now().to_rfc3339();
        store.index.lock().execute(
            "INSERT INTO snapshot_exports(snapshot_id, session_id, requested_offset_ms, frame_number, offset_100ns, offset_ms, image_path, mime_type, created_at_utc, nearest_event_json)
             VALUES (?1, ?2, 0, 0, 0, 0, ?3, 'image/png', ?4, 'null')",
            params![snapshot_id, session_id, legacy_path.to_string_lossy(), created],
        ).unwrap();
        store.index.lock().execute(
            "INSERT INTO shared_frames(share_id, snapshot_id, session_id, requested_offset_ms, frame_number, offset_100ns, offset_ms, image_path, mime_type, created_at_utc, nearest_event_json)
             VALUES (?1, ?2, ?3, 0, 0, 0, 0, ?4, 'image/png', ?5, 'null')",
            params![share_id, snapshot_id, session_id, legacy_path.to_string_lossy(), created],
        ).unwrap();

        let selected_snapshot_root = root.join("selected-snapshots");
        store
            .save_preferences(Preferences {
                snapshot_root: selected_snapshot_root.clone(),
                ..Preferences::defaults_for(&root)
            })
            .unwrap();
        let reused = store.extract_frame(session_id, 0).unwrap();
        assert_eq!(PathBuf::from(reused.image_path), legacy_path);

        store.delete_session(session_id).unwrap();

        let shared = store.get_shared_frame(&share_id).unwrap();
        let relocated = PathBuf::from(shared.image_path);
        assert!(relocated.is_file());
        assert!(
            relocated
                .canonicalize()
                .unwrap()
                .starts_with(selected_snapshot_root.canonicalize().unwrap())
        );
        assert!(!legacy_path.exists());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_legacy_index_registers_existing_shared_snapshots() {
        let root =
            std::env::temp_dir().join(format!("cdxvidext-legacy-shared-{}", uuid::Uuid::now_v7()));
        let session_id = "legacy-shared-session";
        let session_dir = root.join("sessions").join(session_id);
        fs::create_dir_all(session_dir.join("exports")).unwrap();
        let image_path = session_dir.join("exports").join("frame-100.png");
        let status = ffmpeg_command()
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=96x64",
                "-frames:v",
                "1",
                "-y",
            ])
            .arg(&image_path)
            .status()
            .unwrap();
        assert!(status.success());
        let connection = Connection::open(root.join("index.sqlite3")).unwrap();
        connection.execute_batch(
            "CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY, started_at_utc TEXT NOT NULL, ended_at_utc TEXT,
                state TEXT NOT NULL, duration_ms INTEGER, monitor_name TEXT NOT NULL,
                output_width INTEGER NOT NULL, output_height INTEGER NOT NULL,
                frame_count INTEGER NOT NULL DEFAULT 0, event_count INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0, media_path TEXT NOT NULL, display_name TEXT
             );
             CREATE TABLE shared_frames (
                share_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, requested_offset_ms INTEGER NOT NULL,
                frame_number INTEGER NOT NULL, offset_100ns INTEGER NOT NULL, offset_ms REAL NOT NULL,
                image_path TEXT NOT NULL, mime_type TEXT NOT NULL, created_at_utc TEXT NOT NULL
             );"
        ).unwrap();
        connection.execute(
            "INSERT INTO sessions(session_id, started_at_utc, state, monitor_name, output_width, output_height, media_path)
             VALUES (?1, '2026-01-01T00:00:00Z', 'ready', 'Display', 96, 64, ?2)",
            params![session_id, session_dir.join(MEDIA_FILE).to_string_lossy()],
        ).unwrap();
        connection.execute(
            "INSERT INTO shared_frames VALUES ('share-legacy', ?1, 100, 3, 1000000, 100.0, ?2, 'image/png', '2026-01-01T00:00:00Z')",
            params![session_id, image_path.to_string_lossy()],
        ).unwrap();
        drop(connection);

        let store = Store::open(root.clone()).unwrap();
        let registered: i64 = store
            .index
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM snapshot_exports WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        let snapshot_id: Option<String> = store
            .index
            .lock()
            .query_row(
                "SELECT snapshot_id FROM shared_frames WHERE share_id = 'share-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(registered, 1);
        assert!(snapshot_id.is_some());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cursor_is_opaque_and_round_trips() {
        let cursor = encode_cursor(42);
        assert_ne!(cursor, "42");
        assert_eq!(decode_cursor(Some(&cursor)), Some(42));
    }

    #[test]
    fn session_store_persists_encrypted_sensitive_events() {
        let root = std::env::temp_dir().join(format!("cdxvidext-store-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let writer = store
            .create_session(
                "018f0000-0000-7000-8000-000000000000",
                &Utc::now().to_rfc3339(),
                10,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        writer
            .add_event(
                5,
                "os_input",
                "key_down",
                "Encrypted key",
                None,
                None,
                &json!({ "redacted": false }),
                Some(b"secret phrase"),
            )
            .unwrap();
        drop(writer);
        let bytes = fs::read(
            store
                .session_dir("018f0000-0000-7000-8000-000000000000")
                .join(SESSION_DB),
        )
        .unwrap();
        assert!(
            !bytes
                .windows(b"secret phrase".len())
                .any(|w| w == b"secret phrase")
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn turns_and_tool_pairing_are_idempotent_in_real_sqlite() {
        let root = std::env::temp_dir().join(format!("cdxvidext-pairing-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let writer = store
            .create_session(
                "018f0000-0000-7000-8000-000000000001",
                &Utc::now().to_rfc3339(),
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        writer.add_turn("turn-1", 0, Some(5), Some("hash")).unwrap();
        writer.add_turn("turn-1", 0, Some(5), Some("hash")).unwrap();
        writer
            .upsert_tool_start("tool-1", "mcp__node_repl__js", 10)
            .unwrap();
        writer
            .upsert_tool_start("tool-1", "mcp__node_repl__js", 10)
            .unwrap();
        writer
            .upsert_tool_end("tool-1", "mcp__node_repl__js", 20)
            .unwrap();
        let connection = Connection::open(
            store
                .session_dir("018f0000-0000-7000-8000-000000000001")
                .join(SESSION_DB),
        )
        .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM turns", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT status FROM tool_calls WHERE tool_use_id='tool-1'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "complete"
        );
        drop(connection);
        drop(writer);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_preserves_pinned_real_session_directory() {
        let root =
            std::env::temp_dir().join(format!("cdxvidext-retention-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let old = "2000-01-01T00:00:00Z";
        let writer = store
            .create_session(
                "018f0000-0000-7000-8000-000000000002",
                old,
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        drop(writer);
        store
            .pin_session("018f0000-0000-7000-8000-000000000002", true)
            .unwrap();
        assert_eq!(store.purge_expired(Some(1)).unwrap(), 0);
        assert!(
            store
                .session_dir("018f0000-0000-7000-8000-000000000002")
                .exists()
        );
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_display_name_can_be_set_and_cleared_in_real_sqlite() {
        let root = std::env::temp_dir().join(format!("cdxvidext-rename-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let session_id = "018f0000-0000-7000-8000-000000000003";
        let writer = store
            .create_session(
                session_id,
                "2026-07-18T16:20:00Z",
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        drop(writer);

        store
            .rename_session(session_id, Some("  Evidence pass  "))
            .unwrap();
        assert_eq!(
            store
                .get_session(session_id)
                .unwrap()
                .display_name
                .as_deref(),
            Some("Evidence pass")
        );
        store.rename_session(session_id, None).unwrap();
        assert_eq!(store.get_session(session_id).unwrap().display_name, None);

        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirmed_delete_removes_a_pinned_real_session() {
        let root = std::env::temp_dir().join(format!("cdxvidext-delete-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let session_id = "018f0000-0000-7000-8000-000000000004";
        let writer = store
            .create_session(
                session_id,
                "2026-07-18T16:20:00Z",
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        drop(writer);
        store.pin_session(session_id, true).unwrap();

        assert!(store.delete_session_confirmed(session_id, false).is_err());
        store.delete_session_confirmed(session_id, true).unwrap();
        assert!(store.get_session(session_id).is_err());

        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_sessions_persist_selection_filter_flights_and_namespace_real_paths() {
        let root = std::env::temp_dir().join(format!(
            "cdxvidext-archive-session-{}",
            uuid::Uuid::now_v7()
        ));
        let store = Store::open(root.clone()).unwrap();
        let default = store.active_archive_session().unwrap();
        assert_eq!(default.display_name, "Default Session");

        let archive = store.create_archive_session("  Project Atlas  ").unwrap();
        assert_eq!(archive.display_name, "Project Atlas");
        assert_eq!(
            store.active_archive_session().unwrap().archive_session_id,
            archive.archive_session_id
        );
        assert!(store.create_archive_session("project atlas").is_err());
        let archive = store
            .rename_archive_session(&archive.archive_session_id, "Mission Control")
            .unwrap();

        let flight_id = "018f0000-0000-7000-8000-000000000006";
        let writer = store
            .create_session(
                flight_id,
                "2026-07-18T16:20:00Z",
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        drop(writer);
        assert_eq!(
            store
                .list_sessions_for_archive(&archive.archive_session_id, None, 100)
                .unwrap()
                .0
                .len(),
            1
        );
        assert!(
            store
                .list_sessions_for_archive(&default.archive_session_id, None, 100)
                .unwrap()
                .0
                .is_empty()
        );
        assert!(
            store
                .session_dir(flight_id)
                .starts_with(root.join("sessions").join(&archive.archive_session_id))
        );

        drop(store);
        let reopened = Store::open(root.clone()).unwrap();
        assert_eq!(
            reopened.active_archive_session().unwrap().display_name,
            "Mission Control"
        );
        assert_eq!(reopened.list_sessions(None, 100).unwrap().0.len(), 1);
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleting_archive_cascades_real_evidence_and_replaces_the_last_session() {
        let root =
            std::env::temp_dir().join(format!("cdxvidext-archive-delete-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let default = store.active_archive_session().unwrap();
        let archive = store.create_archive_session("Disposable Session").unwrap();
        let flight_id = "018f0000-0000-7000-8000-000000000007";
        let writer = store
            .create_session(
                flight_id,
                "2026-07-18T16:20:00Z",
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        drop(writer);
        store.pin_session(flight_id, true).unwrap();
        let flight_dir = store.session_dir(flight_id);
        let snapshot_root = root.join("exports");
        let flight_archive_dir = root.join("sessions").join(&archive.archive_session_id);
        let snapshot_archive_dir = snapshot_root.join(&archive.archive_session_id);
        let snapshot_path = snapshot_root
            .join(&archive.archive_session_id)
            .join(flight_id)
            .join("frame-0.png");
        fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        fs::write(&snapshot_path, b"real registered png bytes").unwrap();
        store
            .index
            .lock()
            .execute(
                "INSERT INTO snapshot_exports(
                    snapshot_id, session_id, requested_offset_ms, frame_number, offset_100ns,
                    offset_ms, image_path, mime_type, created_at_utc, nearest_event_json,
                    archive_session_id, storage_root
                 ) VALUES ('snapshot-real', ?1, 0, 0, 0, 0.0, ?2, 'image/png', ?3, NULL, ?4, ?5)",
                params![
                    flight_id,
                    snapshot_path.to_string_lossy(),
                    Utc::now().to_rfc3339(),
                    archive.archive_session_id,
                    snapshot_root.to_string_lossy(),
                ],
            )
            .unwrap();
        store
            .index
            .lock()
            .execute(
                "INSERT INTO shared_frames(
                    share_id, snapshot_id, session_id, requested_offset_ms, frame_number,
                    offset_100ns, offset_ms, image_path, mime_type, created_at_utc,
                    nearest_event_json, archive_session_id
                 ) VALUES ('share-real', 'snapshot-real', ?1, 0, 0, 0, 0.0, ?2,
                           'image/png', ?3, NULL, ?4)",
                params![
                    flight_id,
                    snapshot_path.to_string_lossy(),
                    Utc::now().to_rfc3339(),
                    archive.archive_session_id,
                ],
            )
            .unwrap();

        let preview = store
            .archive_delete_preview(&archive.archive_session_id)
            .unwrap();
        assert_eq!((preview.flights, preview.pinned_flights), (1, 1));
        assert_eq!((preview.snapshots, preview.shared_frames), (1, 1));
        let selected = store
            .delete_archive_session(&archive.archive_session_id, &preview)
            .unwrap();
        assert_eq!(selected.archive_session_id, default.archive_session_id);
        assert!(!flight_dir.exists());
        assert!(!snapshot_path.exists());
        assert!(!flight_archive_dir.exists());
        assert!(!snapshot_archive_dir.exists());
        assert!(store.get_session(flight_id).is_err());
        assert!(store.list_shared_frames().unwrap().is_empty());

        let last_preview = store
            .archive_delete_preview(&default.archive_session_id)
            .unwrap();
        let replacement = store
            .delete_archive_session(&default.archive_session_id, &last_preview)
            .unwrap();
        assert_ne!(replacement.archive_session_id, default.archive_session_id);
        assert_eq!(replacement.display_name, "Default Session");
        assert_eq!(store.list_archive_sessions().unwrap().len(), 1);

        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_delete_refuses_a_registered_flight_root_as_the_target() {
        let root =
            std::env::temp_dir().join(format!("cdxvidext-archive-guard-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let archive = store.active_archive_session().unwrap();
        let flight_id = "018f0000-0000-7000-8000-000000000008";
        let writer = store
            .create_session(
                flight_id,
                "2026-07-18T16:20:00Z",
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        drop(writer);
        let flight_root = root.join("sessions").canonicalize().unwrap();
        store
            .index
            .lock()
            .execute(
                "UPDATE sessions SET session_path = ?2 WHERE session_id = ?1",
                params![flight_id, flight_root.to_string_lossy()],
            )
            .unwrap();
        let preview = store
            .archive_delete_preview(&archive.archive_session_id)
            .unwrap();
        assert!(
            store
                .delete_archive_session(&archive.archive_session_id, &preview)
                .is_err()
        );
        assert!(
            store
                .get_archive_session(&archive.archive_session_id)
                .is_ok()
        );

        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presented_timeline_snaps_navigation_to_the_nearest_real_frame() {
        let root = std::env::temp_dir().join(format!("cdxvidext-present-{}", uuid::Uuid::now_v7()));
        let store = Store::open(root.clone()).unwrap();
        let session_id = "018f0000-0000-7000-8000-000000000005";
        let writer = store
            .create_session(
                session_id,
                "2026-07-18T16:20:00Z",
                0,
                10_000_000,
                "Display",
                100,
                100,
                100,
                100,
            )
            .unwrap();
        writer.add_frame(0, 0, 0, false, 0, None).unwrap();
        writer
            .add_frame(1, 333_333, 333_333, false, 0, None)
            .unwrap();
        writer
            .add_event(
                350_000,
                "os_input",
                "pointer_move",
                "raw",
                None,
                None,
                &json!({ "details": { "x": 5, "y": 6, "button_state": 0 } }),
                None,
            )
            .unwrap();
        drop(writer);

        let timeline = store.presented_timeline(session_id).unwrap();

        assert_eq!(timeline.total_events, 1);
        assert_eq!(timeline.categories[0].events[0].seek_offset_ms, 33);

        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
