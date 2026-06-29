#![allow(dead_code)]
// Standalone idle-state diagnostic (Phase 2)
//
// Polls system state every second and prints a status line
// Prints "*** FREE MOMENT ***" when the debounce threshold is crossed
//
// Run with:  cargo run --bin idle_check

// Pull shared modules into this binary's crate root so that
// `use crate::config::...` inside idle_detector.rs resolves here
#[path = "../config.rs"]
mod config;

#[path = "../idle_detector.rs"]
mod idle_detector;

use config::Config;
use idle_detector::IdleDetector;

fn main() {
    let cfg = Config::default();
    let debounce = cfg.idle_detection.debounce_seconds;
    let presence_ms = cfg.idle_detection.presence_threshold_seconds * 1000;
    let denylist = cfg.idle_detection.heavy_process_denylist.clone();

    let mut detector = IdleDetector::new(cfg.idle_detection);

    println!("idle_check - 1 s poll  |  debounce: {debounce} s  |  Ctrl+C to exit");
    println!(
        "{:<10}  {:<7}  {:<6}  {:<22}  {:<11}  {}",
        "time", "present", "fullsc", "foreground", "debounce", "note"
    );

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));

        let fired = detector.poll();
        let (input_ms, proc_name, fullscreen) = detector.diagnostics();

        let present = input_ms < presence_ms;
        let on_denylist = denylist.iter().any(|d| proc_name.eq_ignore_ascii_case(d));
        let heavy = fullscreen || on_denylist;

        let debounce_col = detector
            .debounce_elapsed()
            .map(|d| format!("{:.0}/{debounce}s", d.as_secs_f32()))
            .unwrap_or_else(|| "-".into());

        let note = if fired {
            "*** FREE MOMENT ***"
        } else if !present {
            "user away"
        } else if fullscreen {
            "fullscreen"
        } else if on_denylist {
            "denylist"
        } else if heavy {
            "heavy"
        } else {
            ""
        };

        let now = chrono::Local::now().format("%H:%M:%S");
        let proc_col = if proc_name.is_empty() { "-".to_string() } else { proc_name };

        println!(
            "[{now}]  {present:<7}  {fullscreen:<6}  {proc_col:<22}  {debounce_col:<11}  {note}"
        );
    }
}
