#![windows_subsystem = "windows"]
// Night mode daemon — single .exe, lives in system tray.
//
// First launch: creates tray icon, registers Ctrl+Alt+N, waits.
// Second launch while running: signals the first instance to toggle, exits.
//
// Left-click tray icon  → toggle
// Right-click tray icon → menu (Toggle / Exit)
// Ctrl+Alt+N            → toggle from anywhere

use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIconBuilder, TrayIconEvent,
};
use winapi::um::handleapi::CloseHandle;
use winapi::um::synchapi::{CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent,
                            WaitForSingleObject};

const ALIVE_MUTEX:  &str = "Local\\ResurfacerNightAlive";
const TOGGLE_EVENT: &str = "Local\\ResurfacerNightToggle";
const HOTKEY_ID:     i32 = 9001;

fn main() {
    // If another instance is running, signal it to toggle and exit.
    if signal_existing() { return; }
    run_tray();
}

// ── second-instance path ────────────────────────────────────────────────────

fn signal_existing() -> bool {
    unsafe {
        let mh = OpenMutexW(0x0010_0000 /* SYNCHRONIZE */, 0, wide(ALIVE_MUTEX).as_ptr());
        if mh.is_null() { return false; }
        CloseHandle(mh);

        let eh = OpenEventW(0x0000_0002 /* EVENT_MODIFY_STATE */, 0, wide(TOGGLE_EVENT).as_ptr());
        if !eh.is_null() { SetEvent(eh); CloseHandle(eh); }
        true
    }
}

// ── primary-instance tray loop ──────────────────────────────────────────────

fn run_tray() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, RegisterHotKey, UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_HOTKEY, WM_QUIT,
    };

    // Claim alive mutex so second instances can detect us.
    let alive = unsafe { CreateMutexW(std::ptr::null_mut(), 1, wide(ALIVE_MUTEX).as_ptr()) };
    // Auto-reset event: second instance sets it → we toggle.
    let toggle_ev = unsafe {
        CreateEventW(std::ptr::null_mut(), 0, 0, wide(TOGGLE_EVENT).as_ptr())
    };

    // Global hotkey: Ctrl+Alt+N
    let hotkey_ok = unsafe {
        RegisterHotKey(None, HOTKEY_ID, MOD_CONTROL | MOD_ALT, 0x4E /* N */).is_ok()
    };
    if !hotkey_ok {
        eprintln!("Warning: could not register Ctrl+Alt+N (already in use?)");
    }

    // Tray menu
    let item_toggle = MenuItem::new("Нічний режим: ВИМК", true, None);
    let item_exit   = MenuItem::new("Вийти", true, None);
    let id_toggle   = item_toggle.id().clone();
    let id_exit     = item_exit.id().clone();

    let menu = Menu::new();
    let _ = menu.append(&item_toggle);
    let _ = menu.append(&item_exit);

    let mut tray = TrayIconBuilder::new()
        .with_tooltip("Night Mode: OFF  (Ctrl+Alt+N)")
        .with_icon(circle_icon(0xC8, 0xC8, 0xC8))
        .with_menu(Box::new(menu))
        .build()
        .expect("failed to create tray icon");

    let mut night = false;
    let mut mag: Option<MagContext> = None;

    loop {
        // --- Win32 message pump ---
        let mut msg = MSG::default();
        if unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT { break; }

            if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_ID {
                do_toggle(&mut night, &mut mag, &mut tray, &item_toggle);
            }

            unsafe { TranslateMessage(&msg); DispatchMessageW(&msg); }
        }

        // --- second-instance toggle signal ---
        if unsafe { WaitForSingleObject(toggle_ev, 0) } == 0 /* WAIT_OBJECT_0 */ {
            do_toggle(&mut night, &mut mag, &mut tray, &item_toggle);
        }

        // --- tray left-click ---
        if let Ok(e) = TrayIconEvent::receiver().try_recv() {
            if matches!(e, TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left, ..
            }) {
                do_toggle(&mut night, &mut mag, &mut tray, &item_toggle);
            }
        }

        // --- menu click ---
        if let Ok(e) = MenuEvent::receiver().try_recv() {
            if e.id == id_toggle {
                do_toggle(&mut night, &mut mag, &mut tray, &item_toggle);
            } else if e.id == id_exit {
                break;
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Restore display before exit
    if let Some(ctx) = mag.take() {
        ctx.apply(identity());
    }

    unsafe {
        if hotkey_ok { let _ = UnregisterHotKey(None, HOTKEY_ID); }
        CloseHandle(toggle_ev);
        CloseHandle(alive);
    }
}

fn do_toggle(
    night: &mut bool,
    mag: &mut Option<MagContext>,
    tray: &mut tray_icon::TrayIcon,
    item: &MenuItem,
) {
    *night = !*night;
    if *night {
        if let Some(ctx) = MagContext::init() {
            ctx.apply(warm_amber());
            *mag = Some(ctx);
        }
        let _ = tray.set_icon(Some(circle_icon(0xFF, 0x8C, 0x00)));
        let _ = tray.set_tooltip(Some("Night Mode: ON  (Ctrl+Alt+N)"));
        item.set_text("Нічний режим: УВІМК");
    } else {
        if let Some(ctx) = mag.take() {
            ctx.apply(identity());
        }
        let _ = tray.set_icon(Some(circle_icon(0xC8, 0xC8, 0xC8)));
        let _ = tray.set_tooltip(Some("Night Mode: OFF  (Ctrl+Alt+N)"));
        item.set_text("Нічний режим: ВИМК");
    }
}

// ── icons ───────────────────────────────────────────────────────────────────

fn circle_icon(r: u8, g: u8, b: u8) -> tray_icon::Icon {
    const S: u32 = 32;
    let rgba: Vec<u8> = (0..S * S)
        .flat_map(|i| {
            let x = (i % S) as f32 / S as f32 - 0.5;
            let y = (i / S) as f32 / S as f32 - 0.5;
            if x * x + y * y < 0.42 * 0.42 { [r, g, b, 0xFF] } else { [0, 0, 0, 0] }
        })
        .collect();
    tray_icon::Icon::from_rgba(rgba, S, S).expect("icon")
}

// ── Magnification API ───────────────────────────────────────────────────────

#[repr(C)]
struct ColorEffect { m: [[f32; 5]; 5] }

fn identity() -> ColorEffect {
    ColorEffect { m: [
        [1.0, 0.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 0.0, 1.0],
    ]}
}

fn warm_amber() -> ColorEffect {
    // ~3000 K: R=100%, G=78%, B=38%
    // Cooler → raise G/B; warmer → lower them.
    ColorEffect { m: [
        [1.00, 0.0,  0.0,  0.0, 0.0],
        [0.0,  0.78, 0.0,  0.0, 0.0],
        [0.0,  0.0,  0.38, 0.0, 0.0],
        [0.0,  0.0,  0.0,  1.0, 0.0],
        [0.0,  0.0,  0.0,  0.0, 1.0],
    ]}
}

type BoolFn = unsafe extern "system" fn() -> i32;
type SetFn  = unsafe extern "system" fn(*const ColorEffect) -> i32;

struct MagContext {
    hlib:   winapi::shared::minwindef::HMODULE,
    uninit: BoolFn,
    set:    SetFn,
}

impl MagContext {
    fn init() -> Option<Self> {
        use winapi::um::libloaderapi::{FreeLibrary, GetProcAddress, LoadLibraryW};
        unsafe {
            let hlib = LoadLibraryW(wide("Magnification.dll").as_ptr());
            if hlib.is_null() { return None; }

            macro_rules! proc {
                ($name:expr) => {{
                    let p = GetProcAddress(hlib, concat!($name, "\0").as_ptr() as _);
                    if p.is_null() { FreeLibrary(hlib); return None; }
                    p
                }};
            }

            let init:   BoolFn = std::mem::transmute(proc!("MagInitialize"));
            let uninit: BoolFn = std::mem::transmute(proc!("MagUninitialize"));
            let set:    SetFn  = std::mem::transmute(proc!("MagSetFullscreenColorEffect"));

            if init() == 0 { FreeLibrary(hlib); return None; }
            Some(MagContext { hlib, uninit, set })
        }
    }

    fn apply(&self, effect: ColorEffect) {
        unsafe { (self.set)(&effect); }
    }
}

impl Drop for MagContext {
    fn drop(&mut self) {
        use winapi::um::libloaderapi::FreeLibrary;
        unsafe { (self.uninit)(); FreeLibrary(self.hlib); }
    }
}

// ── utils ────────────────────────────────────────────────────────────────────

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
