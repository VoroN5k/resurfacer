#![allow(dead_code)] // Phase 1-2: some fields/variants wired up for Phase 3

mod bridge;
mod config;
mod idle_detector;
mod messages;
mod tab_store;
mod tab_tracker;

use std::time::Duration;

use tokio::sync::mpsc;

use config::Config;
use idle_detector::IdleDetector;
use messages::DaemonCommand;
use tab_store::TabStore;
use tab_tracker::TabTracker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // All logging to stderr; stdout is reserved for native messaging frames
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("resurfacer-core starting");

    let config = Config::load().unwrap_or_else(|e| {
        tracing::warn!("Could not load config.toml ({e}), using defaults");
        Config::default()
    });

    let store = TabStore::new()?;
    let mut tracker = TabTracker::new(config.clone());
    let mut detector = IdleDetector::new(config.idle_detection.clone());

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    bridge::spawn_reader(event_tx);
    bridge::spawn_writer(cmd_rx);

    // Detection sweep: checks for new candidates every 5 s
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Idle poll: checks system idle state every 1 s
    let mut idle_poll = tokio::time::interval(Duration::from_secs(1));
    idle_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                // If the system is already in a confirmed free state, flush any
                // tabs that were just detected - don't wait for the next transition
                if detector.is_free_moment_active() {
                    let pending = store.pending_count().unwrap_or(0);
                    if pending > 0 {
                        tracing::info!(pending, "Already free - flushing newly detected pending tabs");
                        match tracker.flush_pending(&store) {
                            Ok(cmds) => send_all(&cmd_tx, cmds).await,
                            Err(e)   => tracing::error!("flush_pending: {e}"),
                        }
                    }
                }
            }

            _ = idle_poll.tick() => {
                // poll() does a few Windows API calls - takes < 1 ms, safe to
                // call directly from the async context without spawn_blocking
                let fired = detector.poll();

                if fired {
                    tracing::info!("Free moment detected - flushing pending tabs");
                    match tracker.flush_pending(&store) {
                        Ok(cmds) => send_all(&cmd_tx, cmds).await,
                        Err(e)   => tracing::error!("flush_pending: {e}"),
                    }
                }
            }
        }
    }
    Ok(())
}

async fn send_all(tx: &mpsc::Sender<DaemonCommand>, cmds: Vec<DaemonCommand>) {
    for cmd in cmds {
        if tx.send(cmd).await.is_err() {
            tracing::error!("Command channel closed");
            return;
        }
    }
}
