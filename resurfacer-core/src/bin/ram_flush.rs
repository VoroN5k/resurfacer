#![windows_subsystem = "windows"]
// RAM flush via Windows Task Scheduler — no repeated UAC prompts.
//
// First run: one UAC → installs scheduled task "ResurfacerRamFlush"
//            with HIGHEST privilege level.
// Every next run (or hotkey trigger): schtasks /run → elevated, no UAC.
//
// Internal: task calls `ram_flush.exe --run` which does the actual flush.

const TASK_NAME: &str = "ResurfacerRamFlush";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--run")     => do_flush(),      // called by Task Scheduler (already elevated)
        Some("--install") => do_install(),    // called by relaunch_elevated
        _                 => do_launch(),     // normal double-click / hotkey trigger
    }
}

// ── entry points ─────────────────────────────────────────────────────────────

fn do_launch() {
    if task_exists() {
        // Already set up — just trigger the task, no UAC.
        trigger_task();
    } else {
        // First time: ask once, then install elevated.
        let answer = message_box_yesno(
            "RAM Flush — першочергове налаштування",
            "Для роботи без UAC щоразу потрібно встановити заплановане завдання.\n\n\
             Це запитає права адміністратора ОДИН РАЗ.\n\n\
             Встановити?",
        );
        if answer {
            if is_elevated() {
                install_task();
                do_flush();
            } else {
                relaunch_elevated("--install");
            }
        }
    }
}

fn do_install() {
    // Relaunched elevated specifically to install the task.
    if !is_elevated() {
        relaunch_elevated("--install");
        return;
    }
    install_task();
    // After installing, do the first flush right away.
    do_flush_inner(true);
}

fn do_flush() {
    // Called by the scheduled task (already elevated).
    do_flush_inner(false);
}

// ── flush logic ───────────────────────────────────────────────────────────────

fn do_flush_inner(just_installed: bool) {
    let before = available_mb();

    enable_profile_privilege();

    let mut cmd: u32 = 2; // MemoryEmptyWorkingSets
    nt_set_memory_list(&mut cmd);
    cmd = 4;              // MemoryPurgeStandbyList
    nt_set_memory_list(&mut cmd);

    let after  = available_mb();
    let freed  = after.saturating_sub(before);

    let mut msg = String::new();
    if just_installed {
        msg.push_str("Завдання встановлено. Тепер запуск без UAC.\n\n");
    }
    if freed > 0 {
        msg.push_str(&format!(
            "Standby list очищено.\n\nЗвільнено:     {} МБ\nВільно зараз:  {} МБ  ({:.1} ГБ)",
            freed, after, after as f64 / 1024.0,
        ));
    } else {
        msg.push_str(&format!(
            "Standby list очищено.\n\nВільно зараз:  {} МБ  ({:.1} ГБ)",
            after, after as f64 / 1024.0,
        ));
    }

    show_info("RAM Flush", &msg);
}

// ── Task Scheduler ────────────────────────────────────────────────────────────

fn task_exists() -> bool {
    std::process::Command::new("schtasks")
        .args(["/query", "/tn", TASK_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn install_task() {
    let exe = std::env::current_exe().unwrap();
    // Wrap in quotes in case path has spaces.
    let cmd = format!("\"{}\" --run", exe.display());

    std::process::Command::new("schtasks")
        .args([
            "/create", "/tn", TASK_NAME,
            "/tr", &cmd,
            "/sc", "ondemand",   // on-demand only, no schedule
            "/rl", "HIGHEST",    // run with highest privileges (no UAC on trigger)
            "/f",                // overwrite if exists
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok();
}

pub fn trigger_task() {
    std::process::Command::new("schtasks")
        .args(["/run", "/tn", TASK_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok();
}

// ── NtSetSystemInformation ────────────────────────────────────────────────────

fn nt_set_memory_list(cmd: &mut u32) {
    use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryW};
    type NtSetFn = unsafe extern "system" fn(u32, *mut u32, u32) -> i32;
    unsafe {
        let hlib = LoadLibraryW(wide("ntdll.dll").as_ptr());
        if hlib.is_null() { return; }
        let addr = GetProcAddress(hlib, b"NtSetSystemInformation\0".as_ptr() as _);
        if addr.is_null() { return; }
        let f: NtSetFn = std::mem::transmute(addr);
        f(80, cmd as *mut u32, std::mem::size_of::<u32>() as u32);
    }
}

fn enable_profile_privilege() {
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::AdjustTokenPrivileges;
    use winapi::um::winbase::LookupPrivilegeValueW;
    use winapi::um::winnt::{
        LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return;
        }
        let mut luid: winapi::um::winnt::LUID = std::mem::zeroed();
        LookupPrivilegeValueW(std::ptr::null(), wide("SeProfileSingleProcessPrivilege").as_ptr(), &mut luid);
        let mut tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        AdjustTokenPrivileges(token, 0, &mut tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
    }
}

// ── memory stats ──────────────────────────────────────────────────────────────

fn available_mb() -> u64 {
    use winapi::um::sysinfoapi::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut ms: MEMORYSTATUSEX = std::mem::zeroed();
        ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut ms);
        ms.ullAvailPhys / (1024 * 1024)
    }
}

// ── elevation ─────────────────────────────────────────────────────────────────

fn is_elevated() -> bool {
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::GetTokenInformation;
    use winapi::um::winnt::{TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 { return false; }
        let mut elev: TOKEN_ELEVATION = std::mem::zeroed();
        let mut sz = 0u32;
        GetTokenInformation(token, TokenElevation, &mut elev as *mut _ as _, std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut sz);
        elev.TokenIsElevated != 0
    }
}

fn relaunch_elevated(arg: &str) {
    use winapi::um::shellapi::ShellExecuteW;
    let exe = std::env::current_exe().unwrap();
    let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let arg_w = wide(arg);
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), wide("runas").as_ptr(), exe_w.as_ptr(), arg_w.as_ptr(), std::ptr::null(), 1);
    }
}

// ── dialogs ───────────────────────────────────────────────────────────────────

fn show_info(title: &str, text: &str) {
    use winapi::um::winuser::MessageBoxW;
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), 0x40); }
}

fn message_box_yesno(title: &str, text: &str) -> bool {
    use winapi::um::winuser::MessageBoxW;
    // MB_YESNO | MB_ICONQUESTION = 0x24
    let result = unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), 0x24) };
    result == 6 // IDYES
}

// ── utils ─────────────────────────────────────────────────────────────────────

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
