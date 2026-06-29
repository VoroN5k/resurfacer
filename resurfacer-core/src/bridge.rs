/// Native Messaging bridge: length-prefixed JSON over stdin/stdout.
///
/// Chrome sends each message as a 4-byte little-endian length followed by that
/// many bytes of UTF-8 JSON.  We reply in the same format.
/// stdout must ONLY ever contain native-messaging frames; tracing output goes to stderr.
use std::io::{self, Read, Write};

use tokio::sync::mpsc;

use crate::messages::{DaemonCommand, ExtensionMessage};

/// Spawns a blocking thread that reads messages from stdin and pushes them into `tx`.
/// Exits cleanly when stdin is closed (Chrome disconnected or the extension port dropped).
pub fn spawn_reader(tx: mpsc::Sender<ExtensionMessage>) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        loop {
            let mut len_buf = [0u8; 4];
            if stdin.read_exact(&mut len_buf).is_err() {
                tracing::info!("stdin closed — bridge reader exiting");
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;

            let mut payload = vec![0u8; len];
            if stdin.read_exact(&mut payload).is_err() {
                tracing::error!("Truncated native-messaging payload");
                break;
            }

            match serde_json::from_slice::<ExtensionMessage>(&payload) {
                Ok(msg) => {
                    if tx.blocking_send(msg).is_err() {
                        break; // receiver dropped — shutting down
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        raw = %String::from_utf8_lossy(&payload),
                        "Unrecognised message from extension"
                    );
                }
            }
        }
    })
}

/// Spawns a blocking thread that drains `rx` and writes each command to stdout.
pub fn spawn_writer(mut rx: mpsc::Receiver<DaemonCommand>) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        while let Some(cmd) = rx.blocking_recv() {
            let payload = match serde_json::to_vec(&cmd) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("Failed to serialize command: {e}");
                    continue;
                }
            };
            let len = (payload.len() as u32).to_le_bytes();
            if stdout.write_all(&len).is_err() || stdout.write_all(&payload).is_err() {
                tracing::error!("stdout write failed — bridge writer exiting");
                break;
            }
            stdout.flush().ok();
        }
    })
}
