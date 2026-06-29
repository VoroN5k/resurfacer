use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::Utc;
use uuid::Uuid;

use crate::config::Config;
use crate::messages::{DaemonCommand, ExtensionMessage};
use crate::tab_store::{ArchivalReason, ArchivedTab, TabStore};

#[derive(Debug, Clone, PartialEq)]
enum TabStatus {
    Active,
    /// Content has been requested from the extension; waiting for TabContent reply.
    AwaitingContent { reason: ArchivalReason, cluster_id: Option<String> },
    Closed,
}

struct TabInfo {
    tab_id: i64,
    url: String,
    title: Option<String>,
    opener_tab_id: Option<i64>,
    /// Wall-clock instant when the TabCreated event arrived (used for timing checks).
    created_at: Instant,
    /// UTC timestamp stored in the archive row.
    created_at_utc: chrono::DateTime<chrono::Utc>,
    last_focused_at: Option<Instant>,
    is_video: bool,
    status: TabStatus,
}

pub struct TabTracker {
    tabs: HashMap<i64, TabInfo>,
    config: Config,
}

impl TabTracker {
    pub fn new(config: Config) -> Self {
        Self {
            tabs: HashMap::new(),
            config,
        }
    }

    fn is_video_url(&self, url: &str) -> bool {
        self.config
            .watch_later
            .video_domains
            .iter()
            .any(|d| url.contains(d.as_str()))
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Process one inbound extension message; returns any commands to send back.
    pub fn handle_message(
        &mut self,
        msg: ExtensionMessage,
        store: &TabStore,
    ) -> anyhow::Result<Vec<DaemonCommand>> {
        match msg {
            ExtensionMessage::TabCreated {
                tab_id,
                url,
                title,
                opener_tab_id,
                created_at: _,
            } => {
                let is_video = self.is_video_url(&url);
                self.tabs.insert(
                    tab_id,
                    TabInfo {
                        tab_id,
                        url,
                        title,
                        opener_tab_id,
                        created_at: Instant::now(),
                        created_at_utc: Utc::now(),
                        last_focused_at: None,
                        is_video,
                        status: TabStatus::Active,
                    },
                );
                tracing::debug!(tab_id, "tab_created");
            }

            ExtensionMessage::TabActivated { tab_id } => {
                if let Some(tab) = self.tabs.get_mut(&tab_id) {
                    tab.last_focused_at = Some(Instant::now());
                    tracing::debug!(tab_id, "tab_activated");
                }
            }

            ExtensionMessage::TabRemoved { tab_id } => {
                self.tabs.remove(&tab_id);
                tracing::debug!(tab_id, "tab_removed");
            }

            ExtensionMessage::TabUpdated { tab_id, url, title, .. } => {
                // Compute is_video before the mutable borrow of self.tabs.
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

    /// Periodic sweep: detect watch-later candidates and rabbit-hole clusters.
    /// Returns commands to send to the extension.
    pub fn check_candidates(&mut self, store: &TabStore) -> anyhow::Result<Vec<DaemonCommand>> {
        let mut commands = Vec::new();
        commands.extend(self.check_watch_later(store)?);
        commands.extend(self.check_rabbit_holes(store)?);
        // Purge closed tabs so the map stays small.
        self.tabs.retain(|_, t| t.status != TabStatus::Closed);
        Ok(commands)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Called when the extension replies with extracted page content.
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
            url: tab.url.clone(),
            title: tab.title.clone(),
            opener_tab_id: tab.opener_tab_id,
            created_at: tab.created_at_utc,
            closed_at: Utc::now(),
            reason,
            cluster_id,
            extracted_text: Some(text).filter(|t| !t.is_empty()),
            is_video: false,
        })?;

        tab.status = TabStatus::Closed;
        Ok(vec![DaemonCommand::CloseTab { tab_id }])
    }

    /// Detect tabs matching the "watch later" pattern and initiate archival.
    ///
    /// Detection criteria (per spec §4.1):
    ///   - opened via middle/ctrl-click (has opener_tab_id)
    ///   - URL on a video-hosting domain
    ///   - never received focus within the grace period
    fn check_watch_later(&mut self, store: &TabStore) -> anyhow::Result<Vec<DaemonCommand>> {
        let grace = Duration::from_secs(self.config.watch_later.grace_period_seconds);
        let mut commands = Vec::new();

        for tab in self.tabs.values_mut() {
            if tab.status != TabStatus::Active { continue; }
            if !tab.is_video { continue; }
            if tab.opener_tab_id.is_none() { continue; }
            if tab.last_focused_at.is_some() { continue; }
            if tab.created_at.elapsed() < grace { continue; }

            tracing::info!(tab_id = tab.tab_id, url = %tab.url, "Watch-later: archiving");

            // Video pages: metadata only, no text extraction needed.
            store.archive_tab(&ArchivedTab {
                tab_id: tab.tab_id,
                url: tab.url.clone(),
                title: tab.title.clone(),
                opener_tab_id: tab.opener_tab_id,
                created_at: tab.created_at_utc,
                closed_at: Utc::now(),
                reason: ArchivalReason::WatchLater,
                cluster_id: None,
                extracted_text: None,
                is_video: true,
            })?;

            tab.status = TabStatus::Closed;
            commands.push(DaemonCommand::CloseTab { tab_id: tab.tab_id });
        }
        Ok(commands)
    }

    /// Detect "rabbit hole" clusters and archive them.
    ///
    /// Detection criteria (per spec §4.2):
    ///   - grouped by opener chain OR temporal proximity (10-min window)
    ///   - cluster size >= min_cluster_size
    ///   - no tab in the cluster has had focus in the last N minutes
    fn check_rabbit_holes(&mut self, store: &TabStore) -> anyhow::Result<Vec<DaemonCommand>> {
        let no_focus_threshold =
            Duration::from_secs(self.config.rabbit_hole.no_focus_threshold_minutes * 60);
        let cluster_window =
            Duration::from_secs(self.config.rabbit_hole.cluster_window_minutes * 60);
        let min_size = self.config.rabbit_hole.min_cluster_size;

        // Collect IDs of Active tabs only (immutable borrow ends before mutation).
        let active_ids: Vec<i64> = self
            .tabs
            .iter()
            .filter(|(_, t)| t.status == TabStatus::Active)
            .map(|(id, _)| *id)
            .collect();

        if active_ids.len() < min_size {
            return Ok(vec![]);
        }

        let opener_clusters = self.build_opener_clusters(&active_ids);
        let temporal_clusters = self.build_temporal_clusters(&active_ids, cluster_window);

        // Deduplicate: collect qualifying cluster tab sets into a single list.
        let mut seen: HashSet<i64> = HashSet::new();
        let mut clusters_to_archive: Vec<(Vec<i64>, String)> = Vec::new();

        for cluster in opener_clusters.into_iter().chain(temporal_clusters) {
            if cluster.len() < min_size { continue; }

            // Skip if any tab has had recent focus.
            let all_stale = cluster.iter().all(|&id| {
                self.tabs.get(&id).map_or(false, |t| {
                    let since_last_focus = t
                        .last_focused_at
                        .map(|f| f.elapsed())
                        .unwrap_or_else(|| t.created_at.elapsed());
                    since_last_focus >= no_focus_threshold
                })
            });
            if !all_stale { continue; }

            // Skip tabs already queued in a previous cluster match.
            if cluster.iter().any(|id| seen.contains(id)) { continue; }

            let cluster_id = Uuid::new_v4().to_string();
            cluster.iter().for_each(|id| { seen.insert(*id); });
            clusters_to_archive.push((cluster, cluster_id));
        }

        let mut commands = Vec::new();

        for (cluster, cluster_id) in clusters_to_archive {
            tracing::info!(
                size = cluster.len(),
                cluster_id = %cluster_id,
                "Rabbit-hole cluster detected"
            );

            for tab_id in cluster {
                let Some(tab) = self.tabs.get_mut(&tab_id) else { continue };
                if tab.status != TabStatus::Active { continue; }

                if tab.is_video {
                    // Video: archive metadata immediately.
                    store.archive_tab(&ArchivedTab {
                        tab_id,
                        url: tab.url.clone(),
                        title: tab.title.clone(),
                        opener_tab_id: tab.opener_tab_id,
                        created_at: tab.created_at_utc,
                        closed_at: Utc::now(),
                        reason: ArchivalReason::RabbitHole,
                        cluster_id: Some(cluster_id.clone()),
                        extracted_text: None,
                        is_video: true,
                    })?;
                    tab.status = TabStatus::Closed;
                    commands.push(DaemonCommand::CloseTab { tab_id });
                } else {
                    // Non-video: request page content before closing.
                    tab.status = TabStatus::AwaitingContent {
                        reason: ArchivalReason::RabbitHole,
                        cluster_id: Some(cluster_id.clone()),
                    };
                    commands.push(DaemonCommand::RequestContent { tab_id });
                }
            }
        }

        Ok(commands)
    }

    /// Groups active tabs into clusters connected by opener_tab_id chains.
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
            if cluster.len() >= 2 {
                clusters.push(cluster);
            }
        }
        clusters
    }

    /// Groups active tabs that were all opened within `window` of the earliest tab.
    /// Uses a simple fixed-window sweep: sorts by creation time, then groups into
    /// non-overlapping windows anchored at the first tab in each group.
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
            // current[0].1 is the oldest in this window (sort ascending → safe)
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

impl PartialEq for ArchivalReason {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
