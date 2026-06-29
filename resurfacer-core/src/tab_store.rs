use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::config::exe_dir;

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
}

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
                ON archived_tabs (closed_at);",
        )?;
        Ok(())
    }

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
}
