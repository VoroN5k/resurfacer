/// Extension simulator for Phase 1 integration testing.
///
/// Spawns resurfacer-core as a child process, sends synthetic tab events via
/// its stdin (native messaging format), and reads commands from its stdout.
/// After the grace period elapses, verifies that:
///   - close_tab was sent for the unfocused YouTube tab
///   - close_tab was NOT sent for a non-video tab
///   - close_tab was NOT sent for a YouTube tab that received focus
///   - The archived_tabs SQLite table has the expected row
///
/// Run with:  cargo run --bin sim
/// (from the workspace root; builds both binaries automatically)

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::{json, Value};

// ── Native Messaging framing ─────────────────────────────────────────────────

fn write_frame(w: &mut impl Write, msg: Value) {
    let bytes = serde_json::to_vec(&msg).unwrap();
    w.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
    w.write_all(&bytes).unwrap();
    w.flush().unwrap();
}

fn read_frame(r: &mut impl Read) -> Option<Value> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn daemon_exe() -> PathBuf {
    // sim lives in target/{profile}/sim.exe; daemon is in the same directory.
    let mut exe = std::env::current_exe().unwrap();
    exe.set_file_name(if cfg!(windows) {
        "resurfacer-core.exe"
    } else {
        "resurfacer-core"
    });
    exe
}

fn pass(label: &str) { println!("  [PASS] {label}"); }
fn fail(label: &str) { println!("  [FAIL] {label}"); }

fn check(label: &str, ok: bool) -> bool {
    if ok { pass(label) } else { fail(label) }
    ok
}

fn has_archived_row(db: &Path, tab_id: i64) -> bool {
    Connection::open(db)
        .and_then(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM archived_tabs WHERE tab_id = ?1",
                rusqlite::params![tab_id],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(|n| n > 0)
        .unwrap_or(false)
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let daemon = daemon_exe();
    println!("resurfacer sim — Phase 1 integration test");
    println!("daemon: {}", daemon.display());

    if !daemon.exists() {
        eprintln!("ERROR: daemon not found. Run `cargo build` first.");
        std::process::exit(2);
    }

    // The daemon writes resurfacer.db next to its own exe.
    let db_path = daemon.parent().unwrap().join("resurfacer.db");

    // Remove any leftover DB from a previous run so counts are fresh.
    let _ = std::fs::remove_file(&db_path);

    let mut child = spawn_daemon(&daemon);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // Read daemon commands in a background thread.
    let (tx, rx) = mpsc::channel::<Value>();
    thread::spawn(move || {
        let mut out = stdout;
        while let Some(frame) = read_frame(&mut out) {
            println!("  <- {frame}");
            if tx.send(frame).is_err() {
                break;
            }
        }
    });

    // ── Send tab events ──────────────────────────────────────────────────────

    println!("\n[1] YouTube tab opened via middle-click (no focus) — SHOULD be archived");
    write_frame(&mut stdin, json!({
        "type": "tab_created",
        "tab_id": 1001,
        "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "title": "Never Gonna Give You Up - YouTube",
        "opener_tab_id": 1000,
        "created_at": now_ms()
    }));

    println!("[2] Non-video tab (no opener) — should NOT be archived");
    write_frame(&mut stdin, json!({
        "type": "tab_created",
        "tab_id": 1002,
        "url": "https://news.ycombinator.com/item?id=12345",
        "title": "Ask HN: something interesting",
        "opener_tab_id": null,
        "created_at": now_ms()
    }));

    println!("[3] YouTube tab with focus — should NOT be archived");
    write_frame(&mut stdin, json!({
        "type": "tab_created",
        "tab_id": 1003,
        "url": "https://www.youtube.com/watch?v=abc123",
        "title": "Some Video - YouTube",
        "opener_tab_id": 999,
        "created_at": now_ms()
    }));
    write_frame(&mut stdin, json!({
        "type": "tab_activated",
        "tab_id": 1003
    }));

    // ── Wait for grace period + sweep ────────────────────────────────────────

    // Default grace = 20 s, sweep every 5 s → expect action within ~27 s.
    let wait_secs = 27u64;
    println!("\nWaiting {wait_secs} s (grace period 20 s + sweep 5 s + buffer)…");
    for remaining in (1..=wait_secs).rev() {
        thread::sleep(Duration::from_secs(1));
        if remaining % 5 == 0 {
            print!("  {remaining} s remaining…\r");
            let _ = std::io::stdout().flush();
        }
    }
    println!();

    // ── Collect commands received ─────────────────────────────────────────────

    let mut received: Vec<Value> = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        received.push(cmd);
    }

    // ── Assertions ───────────────────────────────────────────────────────────

    println!("\nResults:");

    let close_1001 = received.iter().any(|c| c["type"] == "close_tab" && c["tab_id"] == 1001);
    let close_1002 = received.iter().any(|c| c["type"] == "close_tab" && c["tab_id"] == 1002);
    let close_1003 = received.iter().any(|c| c["type"] == "close_tab" && c["tab_id"] == 1003);

    let r1 = check("close_tab received for unfocused YouTube tab (1001)", close_1001);
    let r2 = check("close_tab NOT received for non-video tab (1002)", !close_1002);
    let r3 = check("close_tab NOT received for focused YouTube tab (1003)", !close_1003);
    let r4 = check("archived_tabs row exists for tab 1001", has_archived_row(&db_path, 1001));
    let r5 = check("archived_tabs has NO row for tab 1002", !has_archived_row(&db_path, 1002));
    let r6 = check("archived_tabs has NO row for tab 1003", !has_archived_row(&db_path, 1003));

    child.kill().ok();

    let all_ok = r1 && r2 && r3 && r4 && r5 && r6;
    println!();
    if all_ok {
        println!("All checks passed.");
        std::process::exit(0);
    } else {
        println!("Some checks FAILED — see above.");
        std::process::exit(1);
    }
}

fn spawn_daemon(exe: &Path) -> Child {
    Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit()) // daemon logs flow through to our terminal
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn daemon: {e}"))
}
