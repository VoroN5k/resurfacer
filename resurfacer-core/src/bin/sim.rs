// Extension simulator - Phase 2 integration test
//
// Tests the full detection - pending - free-moment - archive flow without
// requiring a browser
//
// What it does:
//   1 - writes a test config (short grace/debounce) next to the daemon exe
//   2 - spawns resurfacer-core, sends synthetic tab events
//   3 - waits for the idle detector to fire a free moment and flush pending tabs
//   4 - verifies close_tab commands and archived_tabs rows
//   5 - cleans up the test config
//
// Run with: cargo run --bin sim

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::{json, Value};

// Native Messaging framing

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

// Helpers

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn daemon_exe() -> PathBuf {
    let mut exe = std::env::current_exe().unwrap();
    exe.set_file_name(if cfg!(windows) { "resurfacer-core.exe" } else { "resurfacer-core" });
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

fn has_pending_row(db: &Path, tab_id: i64) -> bool {
    Connection::open(db)
        .and_then(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM pending_tabs WHERE tab_id = ?1",
                rusqlite::params![tab_id],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(|n| n > 0)
        .unwrap_or(false)
}

fn spawn_daemon(exe: &Path) -> Child {
    Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn daemon: {e}"))
}

// Test config
//
// Short timings so the test completes in ~20 s:
//   grace = 4 s  -> tab detected at the T=5 sweep
//   debounce = 8 s -> free moment fires at T=8 (after daemon starts idle-free)
//
// Timeline:
//   T=0  daemon starts + sweep/idle ticks fire immediately
//   T=5  second sweep: grace elapsed -> tab goes to pending_tabs
//   T=8  idle debounce elapsed -> free moment fires -> flush -> close_tab

const TEST_CONFIG: &str = r#"
[watch_later]
grace_period_seconds = 4
video_domains = ["youtube.com", "twitch.tv", "vimeo.com"]

[rabbit_hole]
cluster_window_minutes = 10
no_focus_threshold_minutes = 15
min_cluster_size = 3

[idle_detection]
presence_threshold_seconds = 120
debounce_seconds = 8
heavy_process_denylist = []

[llm]
model_path = "./models/test.gguf"
max_tabs_per_summary_batch = 30
excerpt_word_limit = 300
"#;

// Main

fn main() {
    let daemon = daemon_exe();
    println!("resurfacer sim (Phase 2) - integration test");
    println!("daemon: {}", daemon.display());

    if !daemon.exists() {
        eprintln!("ERROR: daemon not found. Run `cargo build` first.");
        std::process::exit(2);
    }

    let dir = daemon.parent().unwrap();
    let config_path = dir.join("config.toml");
    let db_path = dir.join("resurfacer.db");

    // Write test config and clean up previous DB
    std::fs::write(&config_path, TEST_CONFIG).expect("write test config");
    let _ = std::fs::remove_file(&db_path);

    let mut child = spawn_daemon(&daemon);
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // Collect commands from daemon in background
    let (tx, rx) = mpsc::channel::<Value>();
    thread::spawn(move || {
        let mut out = stdout;
        while let Some(frame) = read_frame(&mut out) {
            println!("  <- {frame}");
            if tx.send(frame).is_err() { break; }
        }
    });

    // Send tab events

    println!("\n[1] YouTube tab via middle-click, no focus -> SHOULD be archived after free moment");
    write_frame(&mut stdin, json!({
        "type": "tab_created",
        "tab_id": 2001,
        "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "title": "Never Gonna Give You Up - YouTube",
        "opener_tab_id": 2000,
        "created_at": now_ms()
    }));

    println!("[2] Non-video tab - should NOT be archived");
    write_frame(&mut stdin, json!({
        "type": "tab_created",
        "tab_id": 2002,
        "url": "https://news.ycombinator.com/item?id=12345",
        "title": "Ask HN: something",
        "opener_tab_id": null,
        "created_at": now_ms()
    }));

    println!("[3] YouTube tab that gets focused -> should NOT be archived");
    write_frame(&mut stdin, json!({
        "type": "tab_created",
        "tab_id": 2003,
        "url": "https://www.youtube.com/watch?v=focused",
        "title": "Focused Video - YouTube",
        "opener_tab_id": 2000,
        "created_at": now_ms()
    }));
    write_frame(&mut stdin, json!({ "type": "tab_activated", "tab_id": 2003 }));

    // Verify pending state after grace period
    // Tab 2001 should be in pending_tabs after ~5 s (grace=4 s, sweep at T=5)

    println!("\nWaiting 7 s to verify pending state (grace=4 s, sweep=5 s)…");
    thread::sleep(Duration::from_secs(7));

    let in_pending = has_pending_row(&db_path, 2001);
    check("tab 2001 is in pending_tabs after grace period", in_pending);
    check("tab 2002 is NOT in pending_tabs", !has_pending_row(&db_path, 2002));

    // Wait for free moment + flush
    // Debounce=8 s from daemon start -> free moment fires at T≈8
    // At T=7 we already checked; wait a few more seconds for the flush

    println!("\nWaiting 8 more s for free moment + flush (debounce=8 s)…");
    thread::sleep(Duration::from_secs(8));

    let mut received: Vec<Value> = Vec::new();
    while let Ok(cmd) = rx.try_recv() { received.push(cmd); }

    println!("\nResults:");
    let close_2001 = received.iter().any(|c| c["type"] == "close_tab" && c["tab_id"] == 2001);
    let close_2002 = received.iter().any(|c| c["type"] == "close_tab" && c["tab_id"] == 2002);
    let close_2003 = received.iter().any(|c| c["type"] == "close_tab" && c["tab_id"] == 2003);

    let r1 = check("close_tab received for unfocused YouTube tab (2001)", close_2001);
    let r2 = check("close_tab NOT received for non-video tab (2002)", !close_2002);
    let r3 = check("close_tab NOT received for focused YouTube tab (2003)", !close_2003);
    let r4 = check("archived_tabs row exists for tab 2001", has_archived_row(&db_path, 2001));
    let r5 = check("archived_tabs has NO row for tab 2002", !has_archived_row(&db_path, 2002));
    let r6 = check("archived_tabs has NO row for tab 2003", !has_archived_row(&db_path, 2003));
    let r7 = check("pending_tabs is now empty for tab 2001", !has_pending_row(&db_path, 2001));

    child.kill().ok();

    // Remove test config so normal config.toml (if any) takes over next run
    let _ = std::fs::remove_file(&config_path);

    let all_ok = r1 && r2 && r3 && r4 && r5 && r6 && r7;
    println!();
    if all_ok {
        println!("All checks passed.");
        std::process::exit(0);
    } else {
        println!("Some checks FAILED - see above.");
        std::process::exit(1);
    }
}
