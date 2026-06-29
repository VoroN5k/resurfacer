use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::Utc;
use uuid::Uuid;

use crate::config::Config;
use crate::messages::{DaemonCommand, ExtensionMessage};
use crate::tab_store::{ArchivalReason, ArchivedTab, PendingTab, TabStore};

// Internal tab state

#[derive(Debug, Clone, PartialEq)]
enum TabStatus {
    Active,
    // Detected as a candidate, stored in pending_tabs, waiting for a free moment
    Pending { reason: ArchivalReason, cluster_id: Option<String> },
    // Content extraction requested, waiting for TabContent reply before archiving
    AwaitingContent { reason: ArchivalReason, cluster_id: Option<String> },
    Closed,
}

struct TabInfo {
    tab_id: i64,
    url: String,
    title: Option<String>,
    opener_tab_id: Option<i64>,
    created_at: Instant,
    created_at_utc: chrono::DateTime<chrono::Utc>,
    last_focused_at: Option<Instant>,
    is_video: bool,
    status: TabStatus,
}

// TabTracker

pub struct TabTracker {
    tabs: HashMap<i64, TabInfo>,
    config: Config,
}

impl TabTracker {
    pub fn new(config: Config) -> Self {
        Self { tabs: HashMap::new(), config }
    }

    fn is_video_url(&self, url: &str) -> bool {
        self.config
            .watch_later
            .video_domains
            .iter()
            .any(|d| url.contains(d.as_str()))
    }

    // Public API

    // Process one inbound message from the extension
    pub fn handle_message(
        &mut self,
        msg: ExtensionMessage,
        store: &TabStore,
    ) -> anyhow::Result<Vec<DaemonCommand>> {
        match msg {
            ExtensionMessage::TabCreated { tab_id, url, title, opener_tab_id, .. } => {
                let is_video = self.is_video_url(&url);
                self.tabs.insert(tab_id, TabInfo {
                    tab_id,
                    url,
                    title,
                    opener_tab_id,
                    created_at: Instant::now(),
                    created_at_utc: Utc::now(),
                    last_focused_at: None,
                    is_video,
                    status: TabStatus::Active,
                });
                tracing::debug!(tab_id, "tab_created");
            }

            ExtensionMessage::TabActivated { tab_id } => {
                if let Some(tab) = self.tabs.get_mut(&tab_id) {
                    tab.last_focused_at = Some(Instant::now());
                    // If the user focuses a pending tab, rescue it from the queue
                    if matches!(tab.status, TabStatus::Pending { .. }) {
                        store.remove_pending(tab_id)?;
                        tab.status = TabStatus::Active;
                        tracing::info!(tab_id, "Pending tab focused - rescued from queue");
                    } else {
                        tracing::debug!(tab_id, "tab_activated");
                    }
                }
            }

            ExtensionMessage::TabRemoved { tab_id } => {
                // Tab is already gone - remove pending entry if it exists
                store.remove_pending(tab_id)?;
                self.tabs.remove(&tab_id);
                tracing::debug!(tab_id, "tab_removed");
            }

            ExtensionMessage::TabUpdated { tab_id, url, title, .. } => {
                let new_is_video = url.as_deref().map(|u| self.is_video_url(u));
                if let Some(tab) = self.tabs.get_mut(&tab_id) {
                    if let Some(u) = url {
                        tab.is_video = new_is_video.unwrap_or(tab.is_video);
                        tab.url = u;
                    }
                    if let Some(t) = title {
                        tab.title = Some(t);
                    }
                }
            }

            ExtensionMessage::TabContent { tab_id, text, title } => {
                return self.complete_archival_with_content(tab_id, text, title, store);
            }
        }
        Ok(vec![])
    }

    // Periodic detection sweep (every ~5 s)
    // Moves newly detected candidates into pending_tabs; returns no commands
    // (closing is deferred to the free-moment flush)
    pub fn check_candidates(&mut self, store: &TabStore) -> anyhow::Result<Vec<DaemonCommand>> {
        self.check_watch_later(store)?;
        self.check_rabbit_holes(store)?;
        self.tabs.retain(|_, t| t.status != TabStatus::Closed);
        Ok(vec![])
    }

    // Called by main when the idle detector fires a free-moment event
    //
    // Drains the pending_tabs table, archives each tab, and returns the
    // close_tab / request_content commands to send to the extension
    pub fn flush_pending(&mut self, store: &TabStore) -> anyhow::Result<Vec<DaemonCommand>> {
        let pending = store.take_all_pending()?;
        if pending.is_empty() {
            return Ok(vec![]);
        }

        tracing::info!(count = pending.len(), "Free moment - flushing pending tabs");
        let mut commands = Vec::new();

        for tab in pending {
            if let Some(t) = self.tabs.get_mut(&tab.tab_id) {
                if tab.is_video {
                    // Video: archive immediately, close right now
                    store.archive_tab(&ArchivedTab {
                        tab_id:         tab.tab_id,
                        url:            tab.url.clone(),
                        title:          tab.title.clone(),
                        opener_tab_id:  tab.opener_tab_id,
                        created_at:     tab.created_at,
                        closed_at:      Utc::now(),
                        reason:         tab.reason,
                        cluster_id:     tab.cluster_id.clone(),
                        extracted_text: None,
                        is_video:       true,
                    })?;
                    t.status = TabStatus::Closed;
                    commands.push(DaemonCommand::CloseTab { tab_id: tab.tab_id });
                } else {
                    // Non-video: request content extraction first; archival
                    // happens when the extension replies with TabContent
                    t.status = TabStatus::AwaitingContent {
                        reason:     tab.reason,
                        cluster_id: tab.cluster_id,
                    };
                    commands.push(DaemonCommand::RequestContent { tab_id: tab.tab_id });
                }
            } else {
                // Tab already closed by the user before the free moment -
                // archive what we have (no close command needed)
                store.archive_tab(&ArchivedTab {
                    tab_id:         tab.tab_id,
                    url:            tab.url,
                    title:          tab.title,
                    opener_tab_id:  tab.opener_tab_id,
                    created_at:     tab.created_at,
                    closed_at:      Utc::now(),
                    reason:         tab.reason,
                    cluster_id:     tab.cluster_id,
                    extracted_text: None,
                    is_video:       tab.is_video,
                })?;
                tracing::debug!(tab_id = tab.tab_id, "Pending tab was already closed - archived");
            }
        }

        self.tabs.retain(|_, t| t.status != TabStatus::Closed);
        Ok(commands)
    }

    // Private: content completion

    fn complete_archival_with_content(
        &mut self,
        tab_id: i64,
        text: String,
        title: Option<String>,
        store: &TabStore,
    ) -> anyhow::Result<Vec<DaemonCommand>> {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
            return Ok(vec![]);
        };
        let TabStatus::AwaitingContent { reason, cluster_id } = tab.status.clone() else {
            return Ok(vec![]);
        };

        if let Some(t) = title {
            tab.title = Some(t);
        }

        store.archive_tab(&ArchivedTab {
            tab_id,
            url:            tab.url.clone(),
            title:          tab.title.clone(),
            opener_tab_id:  tab.opener_tab_id,
            created_at:     tab.created_at_utc,
            closed_at:      Utc::now(),
            reason,
            cluster_id,
            extracted_text: Some(text).filter(|t| !t.is_empty()),
            is_video:       false,
        })?;

        tab.status = TabStatus::Closed;
        Ok(vec![DaemonCommand::CloseTab { tab_id }])
    }

    // Private: detection logic

    fn check_watch_later(&mut self, store: &TabStore) -> anyhow::Result<()> {
        let grace = Duration::from_secs(self.config.watch_later.grace_period_seconds);

        for tab in self.tabs.values_mut() {
            if tab.status != TabStatus::Active { continue; }
            if !tab.is_video { continue; }
            if tab.opener_tab_id.is_none() { continue; }
            if tab.last_focused_at.is_some() { continue; }
            if tab.created_at.elapsed() < grace { continue; }

            tracing::info!(tab_id = tab.tab_id, url = %tab.url, "Watch-later candidate -> pending");

            store.insert_pending(&PendingTab {
                tab_id:        tab.tab_id,
                url:           tab.url.clone(),
                title:         tab.title.clone(),
                opener_tab_id: tab.opener_tab_id,
                created_at:    tab.created_at_utc,
                detected_at:   Utc::now(),
                reason:        ArchivalReason::WatchLater,
                cluster_id:    None,
                is_video:      true,
            })?;

            tab.status = TabStatus::Pending {
                reason:     ArchivalReason::WatchLater,
                cluster_id: None,
            };
        }
        Ok(())
    }

    fn check_rabbit_holes(&mut self, store: &TabStore) -> anyhow::Result<()> {
        let no_focus_threshold =
            Duration::from_secs(self.config.rabbit_hole.no_focus_threshold_minutes * 60);
        let cluster_window =
            Duration::from_secs(self.config.rabbit_hole.cluster_window_minutes * 60);
        let min_size = self.config.rabbit_hole.min_cluster_size;

        let active_ids: Vec<i64> = self
            .tabs
            .iter()
            .filter(|(_, t)| t.status == TabStatus::Active)
            .map(|(id, _)| *id)
            .collect();

        if active_ids.len() < min_size {
            return Ok(());
        }

        let mut seen: HashSet<i64> = HashSet::new();
        let mut clusters_to_queue: Vec<(Vec<i64>, String)> = Vec::new();

        for cluster in self
            .build_opener_clusters(&active_ids)
            .into_iter()
            .chain(self.build_temporal_clusters(&active_ids, cluster_window))
        {
            if cluster.len() < min_size { continue; }

            let all_stale = cluster.iter().all(|&id| {
                self.tabs.get(&id).map_or(false, |t| {
                    t.last_focused_at
                        .map(|f| f.elapsed())
                        .unwrap_or_else(|| t.created_at.elapsed())
                        >= no_focus_threshold
                })
            });
            if !all_stale { continue; }
            if cluster.iter().any(|id| seen.contains(id)) { continue; }

            let cluster_id = Uuid::new_v4().to_string();
            cluster.iter().for_each(|id| { seen.insert(*id); });
            clusters_to_queue.push((cluster, cluster_id));
        }

        for (cluster, cluster_id) in clusters_to_queue {
            tracing::info!(size = cluster.len(), cluster_id = %cluster_id, "Rabbit-hole cluster -> pending");

            for tab_id in cluster {
                let Some(tab) = self.tabs.get_mut(&tab_id) else { continue };
                if tab.status != TabStatus::Active { continue; }

                store.insert_pending(&PendingTab {
                    tab_id,
                    url:           tab.url.clone(),
                    title:         tab.title.clone(),
                    opener_tab_id: tab.opener_tab_id,
                    created_at:    tab.created_at_utc,
                    detected_at:   Utc::now(),
                    reason:        ArchivalReason::RabbitHole,
                    cluster_id:    Some(cluster_id.clone()),
                    is_video:      tab.is_video,
                })?;

                tab.status = TabStatus::Pending {
                    reason:     ArchivalReason::RabbitHole,
                    cluster_id: Some(cluster_id.clone()),
                };
            }
        }
        Ok(())
    }

    // Cluster building (unchanged from Phase 1)

    fn build_opener_clusters(&self, active_ids: &[i64]) -> Vec<Vec<i64>> {
        let mut visited: HashSet<i64> = HashSet::new();
        let mut clusters = Vec::new();

        for &root in active_ids {
            if visited.contains(&root) { continue; }
            let mut cluster = vec![root];
            let mut queue = vec![root];

            while let Some(parent) = queue.pop() {
                for &candidate in active_ids {
                    if visited.contains(&candidate) || cluster.contains(&candidate) { continue; }
                    if self.tabs.get(&candidate).and_then(|t| t.opener_tab_id) == Some(parent) {
                        cluster.push(candidate);
                        queue.push(candidate);
                    }
                }
            }

            cluster.iter().for_each(|id| { visited.insert(*id); });
            if cluster.len() >= 2 { clusters.push(cluster); }
        }
        clusters
    }

    fn build_temporal_clusters(&self, active_ids: &[i64], window: Duration) -> Vec<Vec<i64>> {
        let mut by_time: Vec<(i64, Instant)> = active_ids
            .iter()
            .filter_map(|&id| self.tabs.get(&id).map(|t| (id, t.created_at)))
            .collect();
        by_time.sort_by_key(|(_, t)| *t);

        let mut clusters: Vec<Vec<i64>> = Vec::new();
        let mut current: Vec<(i64, Instant)> = Vec::new();

        for (id, ts) in by_time {
            if current.is_empty() {
                current.push((id, ts));
                continue;
            }
            if ts.duration_since(current[0].1) <= window {
                current.push((id, ts));
            } else {
                if current.len() >= 2 {
                    clusters.push(current.iter().map(|(i, _)| *i).collect());
                }
                current = vec![(id, ts)];
            }
        }
        if current.len() >= 2 {
            clusters.push(current.iter().map(|(i, _)| *i).collect());
        }
        clusters
    }
}

// PartialEq for ArchivalReason (needed for TabStatus comparison)

impl PartialEq for ArchivalReason {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
