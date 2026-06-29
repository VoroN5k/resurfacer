use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::config::exe_dir;

// Public types

pub struct TabStore {
    conn: Connection,
}

pub struct ArchivedTab {
    pub tab_id: i64,
    pub url: String,
    pub title: Option<String>,
    pub opener_tab_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub closed_at: DateTime<Utc>,
    pub reason: ArchivalReason,
    pub cluster_id: Option<String>,
    pub extracted_text: Option<String>,
    pub is_video: bool,
}

// A tab detected as a candidate but not yet archived - waiting for a free moment
#[derive(Clone)]
pub struct PendingTab {
    pub tab_id: i64,
    pub url: String,
    pub title: Option<String>,
    pub opener_tab_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub detected_at: DateTime<Utc>,
    pub reason: ArchivalReason,
    pub cluster_id: Option<String>,
    pub is_video: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum ArchivalReason {
    WatchLater,
    RabbitHole,
}

impl ArchivalReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WatchLater => "watch_later",
            Self::RabbitHole => "rabbit_hole",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "rabbit_hole" => Self::RabbitHole,
            _ => Self::WatchLater,
        }
    }
}

// TabStore impl

impl TabStore {
    pub fn new() -> Result<Self> {
        let db_path = exe_dir().join("resurfacer.db");
        let conn = Connection::open(&db_path)?;
        let store = Self { conn };
        store.init_schema()?;
        tracing::info!(path = %db_path.display(), "SQLite database opened");
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS archived_tabs (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                tab_id         INTEGER NOT NULL,
                url            TEXT NOT NULL,
                title          TEXT,
                opener_tab_id  INTEGER,
                created_at     TEXT NOT NULL,
                closed_at      TEXT NOT NULL,
                reason         TEXT NOT NULL,
                cluster_id     TEXT,
                extracted_text TEXT,
                is_video       INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_archived_tabs_closed_at
                ON archived_tabs (closed_at);

            CREATE TABLE IF NOT EXISTS pending_tabs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                tab_id        INTEGER NOT NULL UNIQUE,
                url           TEXT NOT NULL,
                title         TEXT,
                opener_tab_id INTEGER,
                created_at    TEXT NOT NULL,
                detected_at   TEXT NOT NULL,
                reason        TEXT NOT NULL,
                cluster_id    TEXT,
                is_video      INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(())
    }

    // archived_tabs

    pub fn archive_tab(&self, tab: &ArchivedTab) -> Result<()> {
        self.conn.execute(
            "INSERT INTO archived_tabs
                (tab_id, url, title, opener_tab_id, created_at, closed_at,
                 reason, cluster_id, extracted_text, is_video)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                tab.tab_id,
                tab.url,
                tab.title,
                tab.opener_tab_id,
                tab.created_at.to_rfc3339(),
                tab.closed_at.to_rfc3339(),
                tab.reason.as_str(),
                tab.cluster_id,
                tab.extracted_text,
                tab.is_video as i64,
            ],
        )?;
        tracing::info!(
            tab_id = tab.tab_id,
            url = %tab.url,
            reason = tab.reason.as_str(),
            "Tab archived"
        );
        Ok(())
    }

    // pending_tabs

    // Insert a newly detected candidate into the pending queue
    // Uses INSERT OR IGNORE so re-detecting the same tab is harmless
    pub fn insert_pending(&self, tab: &PendingTab) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO pending_tabs
                (tab_id, url, title, opener_tab_id, created_at, detected_at,
                 reason, cluster_id, is_video)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                tab.tab_id,
                tab.url,
                tab.title,
                tab.opener_tab_id,
                tab.created_at.to_rfc3339(),
                tab.detected_at.to_rfc3339(),
                tab.reason.as_str(),
                tab.cluster_id,
                tab.is_video as i64,
            ],
        )?;
        tracing::debug!(tab_id = tab.tab_id, reason = tab.reason.as_str(), "Tab queued as pending");
        Ok(())
    }

    // Remove a single pending tab - e.g. user focused it before the free moment
    pub fn remove_pending(&self, tab_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_tabs WHERE tab_id = ?1", params![tab_id])?;
        Ok(())
    }

    // Atomically fetch all pending tabs and clear the table
    // Called when a free-moment event fires
    pub fn take_all_pending(&self) -> Result<Vec<PendingTab>> {
        let mut stmt = self.conn.prepare(
            "SELECT tab_id, url, title, opener_tab_id, created_at, detected_at,
                    reason, cluster_id, is_video
             FROM pending_tabs",
        )?;

        let rows: Vec<PendingTab> = stmt
            .query_map([], |row| {
                Ok(PendingTab {
                    tab_id:        row.get(0)?,
                    url:           row.get(1)?,
                    title:         row.get(2)?,
                    opener_tab_id: row.get(3)?,
                    created_at:    parse_dt(row.get::<_, String>(4)?),
                    detected_at:   parse_dt(row.get::<_, String>(5)?),
                    reason:        ArchivalReason::from_str(&row.get::<_, String>(6)?),
                    cluster_id:    row.get(7)?,
                    is_video:      row.get::<_, i64>(8)? != 0,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        if !rows.is_empty() {
            self.conn.execute("DELETE FROM pending_tabs", [])?;
            tracing::info!(count = rows.len(), "Pending tabs taken for archival");
        }
        Ok(rows)
    }

    // Fetch and delete pending tabs detected more than `hours` hours ago.
    // Called at startup to surface leftovers from previous sessions.
    pub fn take_leftover_pending(&self, hours: u64) -> Result<Vec<PendingTab>> {
        let threshold = (Utc::now() - chrono::Duration::hours(hours as i64)).to_rfc3339();

        let mut stmt = self.conn.prepare(
            "SELECT tab_id, url, title, opener_tab_id, created_at, detected_at,
                    reason, cluster_id, is_video
             FROM pending_tabs WHERE detected_at < ?1",
        )?;

        let rows: Vec<PendingTab> = stmt
            .query_map(params![threshold], |row| {
                Ok(PendingTab {
                    tab_id:        row.get(0)?,
                    url:           row.get(1)?,
                    title:         row.get(2)?,
                    opener_tab_id: row.get(3)?,
                    created_at:    parse_dt(row.get::<_, String>(4)?),
                    detected_at:   parse_dt(row.get::<_, String>(5)?),
                    reason:        ArchivalReason::from_str(&row.get::<_, String>(6)?),
                    cluster_id:    row.get(7)?,
                    is_video:      row.get::<_, i64>(8)? != 0,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        if !rows.is_empty() {
            self.conn.execute(
                "DELETE FROM pending_tabs WHERE detected_at < ?1",
                params![threshold],
            )?;
            tracing::info!(count = rows.len(), "Startup: took leftover pending tabs");
        }
        Ok(rows)
    }

    pub fn pending_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pending_tabs", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

fn parse_dt(s: String) -> DateTime<Utc> {
    s.parse::<DateTime<Utc>>().unwrap_or_else(|_| Utc::now())
}
