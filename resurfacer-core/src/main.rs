#![allow(dead_code)] // Phase 1: many fields/variants are wired up for Phase 2/3

mod bridge;
mod config;
mod messages;
mod tab_store;
mod tab_tracker;

use std::time::Duration;

use tokio::sync::mpsc;

use config::Config;
use messages::ExtensionMessage;
use tab_store::TabStore;
use tab_tracker::TabTracker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // All logging must go to stderr; stdout is reserved for native messaging frames.
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

    // Bridge channels
    let (event_tx, mut event_rx) = mpsc::channel::<ExtensionMessage>(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    bridge::spawn_reader(event_tx);
    bridge::spawn_writer(cmd_rx);

    let mut tracker = TabTracker::new(config);
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            msg = event_rx.recv() => {
                let Some(msg) = msg else {
                    tracing::info!("Event channel closed — shutting down");
                    break;
                };
                match tracker.handle_message(msg, &store) {
                    Ok(cmds) => send_commands(&cmd_tx, cmds).await,
                    Err(e)   => tracing::error!("handle_message error: {e}"),
                }
            }
            _ = sweep.tick() => {
                match tracker.check_candidates(&store) {
                    Ok(cmds) => send_commands(&cmd_tx, cmds).await,
                    Err(e)   => tracing::error!("check_candidates error: {e}"),
                }
            }
        }
    }
    Ok(())
}

async fn send_commands(tx: &mpsc::Sender<crate::messages::DaemonCommand>, cmds: Vec<crate::messages::DaemonCommand>) {
    for cmd in cmds {
        if tx.send(cmd).await.is_err() {
            tracing::error!("Command channel closed");
            return;
        }
    }
}
