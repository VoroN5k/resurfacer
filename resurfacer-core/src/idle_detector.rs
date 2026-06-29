use std::time::{Duration, Instant};

use crate::config::IdleDetectionConfig;

// State machine

#[derive(Debug)]
enum State {
    Blocked,
    Debouncing(Instant),
    Fired,
}

// Public struct

pub struct IdleDetector {
    config: IdleDetectionConfig,
    state: State,
    sys: sysinfo::System,
}

impl IdleDetector {
    pub fn new(config: IdleDetectionConfig) -> Self {
        Self {
            config,
            state: State::Blocked,
            sys: sysinfo::System::new(),
        }
    }

    // Call once per second
    // Returns `true` exactly once per Blocked->Free transition after the
    // debounce window elapses
    pub fn poll(&mut self) -> bool {
        let free = self.is_free();
        let debounce = Duration::from_secs(self.config.debounce_seconds);
        let old = std::mem::replace(&mut self.state, State::Blocked);

        let (new_state, fired) = match old {
            State::Blocked => {
                let next = if free { State::Debouncing(Instant::now()) } else { State::Blocked };
                (next, false)
            }
            State::Debouncing(since) => {
                if !free {
                    (State::Blocked, false)
                } else if since.elapsed() >= debounce {
                    (State::Fired, true)
                } else {
                    (State::Debouncing(since), false)
                }
            }
            State::Fired => {
                let next = if free { State::Fired } else { State::Blocked };
                (next, false)
            }
        };

        self.state = new_state;
        fired
    }

    // Public helpers (used by idle_check binary and main loop)

    pub fn is_user_present(&self) -> bool {
        self.last_input_elapsed_ms() < self.config.presence_threshold_seconds * 1000
    }

    pub fn is_heavy_app_foreground(&mut self) -> bool {
        if self.foreground_is_fullscreen() {
            return true;
        }
        match self.foreground_pid() {
            Some(pid) => {
                let name = self.process_name_for_pid(pid);
                self.config
                    .heavy_process_denylist
                    .iter()
                    .any(|d| name.eq_ignore_ascii_case(d))
            }
            None => false,
        }
    }

    // `(last_input_ms, foreground_process_name, is_fullscreen)`
    pub fn diagnostics(&mut self) -> (u64, String, bool) {
        let ms = self.last_input_elapsed_ms();
        let fullscreen = self.foreground_is_fullscreen();
        let name = self.foreground_pid()
            .map(|pid| self.process_name_for_pid(pid))
            .unwrap_or_default();
        (ms, name, fullscreen)
    }

    // Returns true when the system has already confirmed a free moment and
    // remains in that free state. Used to flush tabs that arrive after the
    // initial free-moment event
    pub fn is_free_moment_active(&self) -> bool {
        matches!(self.state, State::Fired)
    }

    // Time the detector has been continuously in the debounce window, if any
    pub fn debounce_elapsed(&self) -> Option<Duration> {
        if let State::Debouncing(since) = &self.state {
            Some(since.elapsed())
        } else {
            None
        }
    }

    // Private

    fn is_free(&mut self) -> bool {
        self.is_user_present() && !self.is_heavy_app_foreground()
    }

    // Look up a process name by PID using sysinfo (cross-platform)
    fn process_name_for_pid(&mut self, pid: u32) -> String {
        // sysinfo 0.30: Pid wraps usize internally; use From<usize>
        let spid = sysinfo::Pid::from(pid as usize);
        self.sys.refresh_process(spid);
        self.sys
            .process(spid)
            .map(|p| p.name().to_string())
            .unwrap_or_default()
    }

    // Windows-specific system calls

    #[cfg(windows)]
    fn last_input_elapsed_ms(&self) -> u64 {
        use std::mem::size_of;
        use windows::Win32::System::SystemInformation::GetTickCount64;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        unsafe {
            let mut info = LASTINPUTINFO {
                cbSize: size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            if !GetLastInputInfo(&mut info).as_bool() {
                return 0; // fail-open: treat as just having had input
            }
            let tick_now = GetTickCount64();
            tick_now.saturating_sub(info.dwTime as u64)
        }
    }

    #[cfg(windows)]
    fn foreground_is_fullscreen(&self) -> bool {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetSystemMetrics, GetWindowRect, SM_CXSCREEN, SM_CYSCREEN,
        };
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0 == 0 { return false; }
            let mut rect = RECT::default();
            // windows 0.52: GetWindowRect returns Result<()>
            if GetWindowRect(hwnd, &mut rect).is_err() { return false; }
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            (rect.right - rect.left) >= sw && (rect.bottom - rect.top) >= sh
        }
    }

    #[cfg(windows)]
    fn foreground_pid(&self) -> Option<u32> {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0 == 0 { return None; }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            // On Windows, sysinfo::Pid is a type alias for u32
            if pid == 0 { None } else { Some(pid) }
        }
    }

    // Non-Windows stubs (always "free")

    #[cfg(not(windows))]
    fn last_input_elapsed_ms(&self) -> u64 { 0 }

    #[cfg(not(windows))]
    fn foreground_is_fullscreen(&self) -> bool { false }

    #[cfg(not(windows))]
    fn foreground_pid(&self) -> Option<u32> { None }
}
