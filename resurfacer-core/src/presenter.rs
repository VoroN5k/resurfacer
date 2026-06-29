use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;

#[derive(Clone)]
pub struct RecapPayload {
    pub reason: String,
    pub tab_count: usize,
    pub entries: Vec<RecapEntry>,
    pub ollama_url: String,
    pub ollama_model: String,
}

#[derive(Clone)]
pub struct RecapEntry {
    pub title: String,
    pub url: String,
}

pub enum PresenterAction {
    ReopenUrls(Vec<String>),
}

pub fn run(recap_rx: mpsc::Receiver<RecapPayload>, action_tx: mpsc::Sender<PresenterAction>) {
    use windows::Win32::Foundation::*;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::*;

    const WM_RECAP_READY:    u32 = WM_APP + 1;
    const WM_SUMMARIZE_DONE: u32 = WM_APP + 2;

    let recap_queue: Arc<Mutex<VecDeque<RecapPayload>>> = Arc::new(Mutex::new(VecDeque::new()));
    let summary_queue: Arc<Mutex<VecDeque<(RecapPayload, String)>>> =
        Arc::new(Mutex::new(VecDeque::new()));

    let rq = Arc::clone(&recap_queue);
    let main_tid = unsafe { GetCurrentThreadId() };

    // Relay thread: receives recaps from daemon, wakes Win32 loop
    std::thread::spawn(move || {
        while let Ok(payload) = recap_rx.recv() {
            tracing::info!(reason = %payload.reason, tab_count = payload.tab_count, "Relay: recap ready");
            rq.lock().unwrap().push_back(payload);
            unsafe { let _ = PostThreadMessageW(main_tid, WM_RECAP_READY, WPARAM(0), LPARAM(0)); }
        }
    });

    let _tray = build_tray_icon();
    tracing::info!("Presenter Win32 message loop started");

    loop {
        let mut msg = MSG::default();
        let has_msg = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };

        if has_msg.as_bool() {
            if msg.message == WM_QUIT { break; }

            if msg.message == WM_RECAP_READY {
                let recaps: Vec<RecapPayload> = recap_queue.lock().unwrap().drain(..).collect();
                for recap in recaps {
                    show_recap_dialog(recap, &action_tx, main_tid, &summary_queue);
                }
                continue;
            }

            if msg.message == WM_SUMMARIZE_DONE {
                let results: Vec<(RecapPayload, String)> =
                    summary_queue.lock().unwrap().drain(..).collect();
                for (recap, summary) in results {
                    show_summary_dialog(&recap, &summary, &action_tx);
                }
                continue;
            }

            unsafe { TranslateMessage(&msg); DispatchMessageW(&msg); }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
}

fn show_recap_dialog(
    recap: RecapPayload,
    action_tx: &mpsc::Sender<PresenterAction>,
    main_tid: u32,
    summary_queue: &Arc<Mutex<VecDeque<(RecapPayload, String)>>>,
) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Controls::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    const WM_SUMMARIZE_DONE: u32 = WM_APP + 2;

    let badge = badge_name(&recap.reason);
    let title   = wstr("Resurfacer");
    let heading = wstr(&format!("{} вкладок збережено — {badge}", recap.tab_count));

    let mut body = String::new();
    for e in &recap.entries {
        let t = if e.title.is_empty() { &e.url } else { &e.title };
        body.push_str(&format!("• {t}\n"));
    }
    let content = wstr(body.trim_end());

    let lbl_reopen  = wstr("Відкрити всі");
    let lbl_ai      = wstr("AI підсумок");
    let lbl_save    = wstr("Зберегти в нотатки");
    let lbl_dismiss = wstr("Закрити");

    const ID_REOPEN:  i32 = 101;
    const ID_AI:      i32 = 102;
    const ID_SAVE:    i32 = 103;
    const ID_DISMISS: i32 = 104;

    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: ID_REOPEN,  pszButtonText: PCWSTR(lbl_reopen.as_ptr())  },
        TASKDIALOG_BUTTON { nButtonID: ID_AI,      pszButtonText: PCWSTR(lbl_ai.as_ptr())      },
        TASKDIALOG_BUTTON { nButtonID: ID_SAVE,    pszButtonText: PCWSTR(lbl_save.as_ptr())    },
        TASKDIALOG_BUTTON { nButtonID: ID_DISMISS, pszButtonText: PCWSTR(lbl_dismiss.as_ptr()) },
    ];

    let mut cfg: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
    cfg.cbSize             = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
    cfg.pszWindowTitle     = PCWSTR(title.as_ptr());
    cfg.pszMainInstruction = PCWSTR(heading.as_ptr());
    cfg.pszContent         = PCWSTR(content.as_ptr());
    cfg.pButtons           = buttons.as_ptr();
    cfg.cButtons           = buttons.len() as u32;
    cfg.dwFlags            = TASKDIALOG_FLAGS(TDF_ALLOW_DIALOG_CANCELLATION.0 | TDF_SIZE_TO_CONTENT.0);

    let mut pressed = 0i32;
    if let Err(e) = unsafe { TaskDialogIndirect(&cfg, Some(&mut pressed), None, None) } {
        tracing::error!("TaskDialogIndirect: {e}");
        return;
    }

    tracing::info!(pressed, "Recap dialog closed");

    match pressed {
        ID_REOPEN => {
            let urls: Vec<String> = recap.entries.iter().map(|e| e.url.clone()).collect();
            let _ = action_tx.send(PresenterAction::ReopenUrls(urls));
        }
        ID_AI => {
            // Run Ollama on a background thread; post WM_SUMMARIZE_DONE when done
            let sq = Arc::clone(summary_queue);
            let recap_clone = recap.clone();
            std::thread::spawn(move || {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
                tracing::info!("AI summary thread started");
                let summary = generate_ai_summary(&recap_clone);
                sq.lock().unwrap().push_back((recap_clone, summary));
                unsafe { let _ = PostThreadMessageW(main_tid, WM_SUMMARIZE_DONE, WPARAM(0), LPARAM(0)); }
            });
            // Show a brief "generating" notice while waiting
            show_generating_notice();
        }
        ID_SAVE => save_to_notes(&recap, None),
        _ => {}
    }
}

fn show_generating_notice() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Controls::*;

    let title   = wstr("Resurfacer");
    let heading = wstr("AI підсумок генерується");
    let content = wstr("Ollama обробляє запит. Результат з'явиться автоматично.");

    let lbl_ok = wstr("OK");
    const ID_OK: i32 = 1;

    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: ID_OK, pszButtonText: PCWSTR(lbl_ok.as_ptr()) },
    ];

    let mut cfg: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
    cfg.cbSize             = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
    cfg.pszWindowTitle     = PCWSTR(title.as_ptr());
    cfg.pszMainInstruction = PCWSTR(heading.as_ptr());
    cfg.pszContent         = PCWSTR(content.as_ptr());
    cfg.pButtons           = buttons.as_ptr();
    cfg.cButtons           = buttons.len() as u32;
    cfg.dwFlags            = TASKDIALOG_FLAGS(TDF_ALLOW_DIALOG_CANCELLATION.0 | TDF_SIZE_TO_CONTENT.0);

    let mut pressed = 0i32;
    let _ = unsafe { TaskDialogIndirect(&cfg, Some(&mut pressed), None, None) };
}

fn show_summary_dialog(
    recap: &RecapPayload,
    summary: &str,
    action_tx: &mpsc::Sender<PresenterAction>,
) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Controls::*;

    let badge = badge_name(&recap.reason);
    let title   = wstr("Resurfacer — AI підсумок");
    let heading = wstr(&format!("{} вкладок — {badge}", recap.tab_count));

    let mut body = format!("{}\n\n", summary);
    for e in &recap.entries {
        let t = if e.title.is_empty() { &e.url } else { &e.title };
        body.push_str(&format!("• {t}\n"));
    }
    let content = wstr(body.trim_end());

    let lbl_reopen  = wstr("Відкрити всі");
    let lbl_save    = wstr("Зберегти в нотатки");
    let lbl_dismiss = wstr("Закрити");

    const ID_REOPEN:  i32 = 201;
    const ID_SAVE:    i32 = 202;
    const ID_DISMISS: i32 = 203;

    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: ID_REOPEN,  pszButtonText: PCWSTR(lbl_reopen.as_ptr())  },
        TASKDIALOG_BUTTON { nButtonID: ID_SAVE,    pszButtonText: PCWSTR(lbl_save.as_ptr())    },
        TASKDIALOG_BUTTON { nButtonID: ID_DISMISS, pszButtonText: PCWSTR(lbl_dismiss.as_ptr()) },
    ];

    let mut cfg: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
    cfg.cbSize             = std::mem::size_of::<TASKDIALOGCONFIG>() as u32;
    cfg.pszWindowTitle     = PCWSTR(title.as_ptr());
    cfg.pszMainInstruction = PCWSTR(heading.as_ptr());
    cfg.pszContent         = PCWSTR(content.as_ptr());
    cfg.pButtons           = buttons.as_ptr();
    cfg.cButtons           = buttons.len() as u32;
    cfg.dwFlags            = TASKDIALOG_FLAGS(TDF_ALLOW_DIALOG_CANCELLATION.0 | TDF_SIZE_TO_CONTENT.0);

    let mut pressed = 0i32;
    let _ = unsafe { TaskDialogIndirect(&cfg, Some(&mut pressed), None, None) };

    tracing::info!(pressed, "Summary dialog closed");

    match pressed {
        ID_REOPEN => {
            let urls: Vec<String> = recap.entries.iter().map(|e| e.url.clone()).collect();
            let _ = action_tx.send(PresenterAction::ReopenUrls(urls));
        }
        ID_SAVE => save_to_notes(recap, Some(summary)),
        _ => {}
    }
}

fn generate_ai_summary(recap: &RecapPayload) -> String {
    let list = recap.entries.iter()
        .map(|e| {
            let title = if e.title.is_empty() { "без назви" } else { e.title.as_str() };
            let host = e.url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or(e.url.as_str());
            format!("- {title} ({host})")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "<|im_start|>system\n\
         Ти підсумовуєш сесії браузерних вкладок одним реченням. Будь стислим і конкретним. Відповідай лише українською мовою.<|im_end|>\n\
         <|im_start|>user\n\
         Користувач мав відкриті ці вкладки браузера, але так і не переглянув їх:\n\
         {list}\n\
         Напиши рівно одне речення українською мовою, яке підсумовує, що він досліджував.<|im_end|>\n\
         <|im_start|>assistant\n"
    );

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("reqwest build: {e}");
            return "Не вдалося підключитися до Ollama.".to_string();
        }
    };

    let body = serde_json::json!({
        "model": recap.ollama_model,
        "prompt": prompt,
        "stream": false,
        "options": { "temperature": 0.7, "num_predict": 120, "stop": ["<|im_end|>", "\n\n"] }
    });

    let url = format!("{}/api/generate", recap.ollama_url.trim_end_matches('/'));
    tracing::info!(url = %url, model = %recap.ollama_model, "Calling Ollama");

    match client.post(&url).json(&body).send() {
        Err(e) => {
            tracing::warn!("Ollama error: {e}");
            format!("Ollama недоступний: {e}")
        }
        Ok(resp) => resp
            .json::<serde_json::Value>()
            .ok()
            .and_then(|v| v["response"].as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Ollama не повернув результат.".to_string()),
    }
}

fn save_to_notes(recap: &RecapPayload, ai_summary: Option<&str>) {
    let path = crate::config::exe_dir().join("resurfacer-notes.txt");
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let mut text = format!("\n[{timestamp}] {} вкладок — {}\n", recap.tab_count, recap.reason);
    if let Some(s) = ai_summary {
        text.push_str(&format!("AI підсумок: {s}\n"));
    }
    for e in &recap.entries {
        text += &format!("  {} — {}\n", e.title, e.url);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = f.write_all(text.as_bytes());
    }
}

fn badge_name(reason: &str) -> &'static str {
    if reason == "watch_later" { "Watch Later" } else { "Rabbit Hole" }
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn build_tray_icon() -> Option<tray_icon::TrayIcon> {
    let icon = make_icon()?;
    tray_icon::TrayIconBuilder::new()
        .with_tooltip("Resurfacer — tab hygiene daemon")
        .with_icon(icon)
        .build()
        .ok()
}

fn make_icon() -> Option<tray_icon::Icon> {
    const SIZE: u32 = 32;
    let rgba: Vec<u8> = (0..SIZE * SIZE)
        .flat_map(|i| {
            let x = (i % SIZE) as f32 / SIZE as f32 - 0.5;
            let y = (i / SIZE) as f32 / SIZE as f32 - 0.5;
            if (x * x + y * y).sqrt() < 0.42 { [0x4A_u8, 0x90, 0xD9, 0xFF] } else { [0, 0, 0, 0] }
        })
        .collect();
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).ok()
}
