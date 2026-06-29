#![allow(dead_code)]

mod bridge;
mod config;
mod idle_detector;
mod messages;
mod presenter;
mod summarizer;
mod tab_store;
mod tab_tracker;

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc;

use config::Config;
use idle_detector::IdleDetector;
use messages::DaemonCommand;
use presenter::{PresenterAction, RecapEntry, RecapPayload};
use tab_store::{ArchivalReason, ArchivedTab, TabStore};
use tab_tracker::TabTracker;

fn main() {
    let log_path = config::exe_dir().join("resurfacer.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&log_path)
        .expect("log file");

    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    tracing::info!("resurfacer-core starting");

    let config = Config::load().unwrap_or_else(|e| {
        tracing::warn!("Could not load config.toml ({e}), using defaults");
        Config::default()
    });

    let (recap_tx, recap_rx) = std::sync::mpsc::channel::<RecapPayload>();
    let (action_tx, action_rx) = std::sync::mpsc::channel::<PresenterAction>();

    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(daemon_main(config, recap_tx, action_rx))
            .unwrap_or_else(|e| tracing::error!("daemon exited with error: {e}"));
    });

    presenter::run(recap_rx, action_tx);
}

async fn daemon_main(
    config: Config,
    recap_tx: std::sync::mpsc::Sender<RecapPayload>,
    action_rx: std::sync::mpsc::Receiver<PresenterAction>,
) -> anyhow::Result<()> {
    let store = TabStore::new()?;
    let mut tracker = TabTracker::new(config.clone());
    let mut detector = IdleDetector::new(config.idle_detection.clone());

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    bridge::spawn_reader(event_tx);
    bridge::spawn_writer(cmd_rx);

    // Archive any pending tabs left over from a previous session (older than 8 h)
    do_startup_cleanup(&store, &config, &recap_tx);

    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut idle_poll = tokio::time::interval(Duration::from_secs(1));
    idle_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut action_poll = tokio::time::interval(Duration::from_secs(1));
    action_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    tracing::info!("Event loop started");

    loop {
        tokio::select! {
            msg = event_rx.recv() => {
                let Some(msg) = msg else {
                    tracing::info!("Event channel closed - shutting down");
                    break;
                };
                match tracker.handle_message(msg, &store) {
                    Ok(cmds) => send_all(&cmd_tx, cmds).await,
                    Err(e)   => tracing::error!("handle_message: {e}"),
                }
            }

            _ = sweep.tick() => {
                match tracker.check_candidates(&store) {
                    Ok(cmds) => send_all(&cmd_tx, cmds).await,
                    Err(e)   => tracing::error!("check_candidates: {e}"),
                }
                if detector.is_free_moment_active() {
                    let pending = store.pending_count().unwrap_or(0);
                    if pending > 0 {
                        tracing::info!(pending, "Already free - flushing newly detected pending tabs");
                        do_flush(&mut tracker, &store, &cmd_tx, &config, &recap_tx).await;
                    }
                }
            }

            _ = idle_poll.tick() => {
                let fired = detector.poll();
                if fired {
                    tracing::info!("Free moment detected - flushing pending tabs");
                    do_flush(&mut tracker, &store, &cmd_tx, &config, &recap_tx).await;
                }
            }

            _ = action_poll.tick() => {
                while let Ok(action) = action_rx.try_recv() {
                    match action {
                        PresenterAction::ReopenUrls(urls) => {
                            tracing::info!(count = urls.len(), "Reopening URLs from presenter");
                            send_all(&cmd_tx, vec![DaemonCommand::ReopenUrls { urls }]).await;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// Archive pending tabs from previous sessions and show a startup recap.
fn do_startup_cleanup(
    store: &TabStore,
    config: &Config,
    recap_tx: &std::sync::mpsc::Sender<RecapPayload>,
) {
    let tabs = match store.take_leftover_pending(8) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("startup_cleanup take: {e}");
            return;
        }
    };

    if tabs.is_empty() {
        tracing::info!("Startup cleanup: no leftover pending tabs");
        return;
    }

    tracing::info!(count = tabs.len(), "Startup cleanup: archiving leftover tabs from previous session");

    for tab in &tabs {
        let _ = store.archive_tab(&ArchivedTab {
            tab_id:         tab.tab_id,
            url:            tab.url.clone(),
            title:          tab.title.clone(),
            opener_tab_id:  tab.opener_tab_id,
            created_at:     tab.created_at,
            closed_at:      Utc::now(),
            reason:         tab.reason,
            cluster_id:     tab.cluster_id.clone(),
            extracted_text: None,
            is_video:       tab.is_video,
        });
    }

    let mut groups: HashMap<String, Vec<tab_store::PendingTab>> = HashMap::new();
    for tab in tabs {
        groups.entry(reason_str(tab.reason)).or_default().push(tab);
    }

    for (reason, group_tabs) in groups {
        let entries = group_tabs.iter()
            .map(|t| RecapEntry {
                url:   t.url.clone(),
                title: t.title.clone().unwrap_or_default(),
            })
            .collect();

        let payload = RecapPayload {
            reason:        reason.clone(),
            tab_count:     group_tabs.len(),
            entries,
            ollama_url:    config.llm.ollama_url.clone(),
            ollama_model:  config.llm.ollama_model.clone(),
        };

        tracing::info!(reason = %payload.reason, tab_count = payload.tab_count, "Startup recap sent");
        let _ = recap_tx.send(payload);
    }
}

async fn do_flush(
    tracker: &mut TabTracker,
    store: &TabStore,
    cmd_tx: &mpsc::Sender<DaemonCommand>,
    config: &Config,
    recap_tx: &std::sync::mpsc::Sender<RecapPayload>,
) {
    match tracker.flush_pending(store) {
        Err(e) => tracing::error!("flush_pending: {e}"),
        Ok((cmds, flushed)) => {
            send_all(cmd_tx, cmds).await;

            if flushed.is_empty() {
                return;
            }

            let mut groups: HashMap<String, Vec<tab_store::PendingTab>> = HashMap::new();
            for tab in flushed {
                groups.entry(reason_str(tab.reason)).or_default().push(tab);
            }

            for (reason, tabs) in groups {
                let entries = tabs.iter()
                    .map(|t| RecapEntry {
                        url:   t.url.clone(),
                        title: t.title.clone().unwrap_or_default(),
                    })
                    .collect();

                let payload = RecapPayload {
                    reason:       reason.clone(),
                    tab_count:    tabs.len(),
                    entries,
                    ollama_url:   config.llm.ollama_url.clone(),
                    ollama_model: config.llm.ollama_model.clone(),
                };

                tracing::info!(
                    reason = %payload.reason,
                    tab_count = payload.tab_count,
                    "Sending recap to presenter"
                );
                let _ = recap_tx.send(payload);
            }
        }
    }
}

fn reason_str(r: ArchivalReason) -> String {
    r.as_str().to_string()
}

async fn send_all(tx: &mpsc::Sender<DaemonCommand>, cmds: Vec<DaemonCommand>) {
    for cmd in cmds {
        if tx.send(cmd).await.is_err() {
            tracing::error!("Command channel closed");
            return;
        }
    }
}
