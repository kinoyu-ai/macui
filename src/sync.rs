use super::*;
pub const SYNC: u32 = WM_APP + 23;
static LATEST: std::sync::Mutex<Option<tasks::Snapshot>> = std::sync::Mutex::new(None);
thread_local! {
    pub static SHELL_MSG: Cell<u32> = const { Cell::new(0) };
    static REQUEST: RefCell<Option<std::sync::mpsc::SyncSender<()>>> = const { RefCell::new(None) };
    static STARTUP_TRIMMED: Cell<bool> = const { Cell::new(false) };
}
pub fn request() {
    REQUEST.with_borrow(|s| {
        if let Some(tx) = s {
            let _ = tx.try_send(());
        }
    });
}
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};

unsafe fn find_overflow_window() -> HWND {
    let h = FindWindowW(w("TopLevelWindowForOverflowXamlIsland").as_ptr(), null());
    if !h.is_null() {
        return h;
    }
    FindWindowW(w("NotifyIconOverflowWindow").as_ptr(), null())
}

unsafe fn is_overflow_window_visible(h: HWND) -> bool {
    if h.is_null() || IsWindowVisible(h) == 0 {
        return false;
    }
    let mut cloaked = 0u32;
    DwmGetWindowAttribute(
        h,
        DWMWA_CLOAKED as u32,
        (&mut cloaked as *mut u32).cast(),
        4,
    );
    if cloaked != 0 {
        return false;
    }
    let mut r: RECT = zeroed();
    GetWindowRect(h, &mut r) != 0 && (r.right - r.left) > 20 && (r.bottom - r.top) > 20
}

pub unsafe fn is_overflow_open() -> bool {
    is_overflow_window_visible(find_overflow_window())
}

unsafe fn send_key_tap(vk: u16) {
    let ev = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let keys = [ev(false), ev(true)];
    SendInput(keys.len() as u32, keys.as_ptr(), size_of::<INPUT>() as i32);
}

unsafe fn click_legacy_tray_chevron() -> bool {
    let tray = FindWindowW(w("Shell_TrayWnd").as_ptr(), null());
    if tray.is_null() {
        return false;
    }
    let notify = FindWindowExW(tray, null_mut(), w("TrayNotifyWnd").as_ptr(), null());
    if notify.is_null() {
        return false;
    }
    let btn = FindWindowExW(notify, null_mut(), w("Button").as_ptr(), null());
    if btn.is_null() {
        return false;
    }
    PostMessageW(btn, 0x00F5 /* BM_CLICK */, 0, 0) != 0
}

unsafe fn position_overflow_under_bar(ov: HWND, anchor_x: i32, anchor_y: i32) {
    let mut r: RECT = zeroed();
    if GetWindowRect(ov, &mut r) == 0 {
        return;
    }
    let w_px = r.right - r.left;
    let h_px = r.bottom - r.top;
    if w_px <= 20 || h_px <= 20 {
        return;
    }
    let screen_w = GetSystemMetrics(SM_CXSCREEN).max(640);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);
    let tx = (anchor_x - w_px / 2).clamp(8, (screen_w - w_px - 8).max(8));
    let ty = anchor_y.clamp(8, (screen_h - h_px - 12).max(8));
    SetWindowPos(ov, HWND_TOPMOST, tx, ty, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
}

pub fn overflow(owner: HWND, anchor_x: i32, anchor_y: i32) {
    let owner_u = owner as usize;
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::Com::*;
        let com_ok = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let was_open = is_overflow_open();

        if was_open {
            // Toggle closed cleanly via UIA chevron invoke/toggle (never WM_CLOSE Explorer's XAML island!)
            let _ = tasks::automation().and_then(|u| tasks::overflow(&u));
            std::thread::sleep(std::time::Duration::from_millis(100));
            if is_overflow_open() {
                // Fallback: dismiss flyout via Escape / focusing owner
                let ov = find_overflow_window();
                if !ov.is_null() {
                    SetForegroundWindow(ov);
                    send_key_tap(VK_ESCAPE);
                }
                if owner_u != 0 {
                    SetForegroundWindow(owner_u as HWND);
                }
            }
            if com_ok {
                CoUninitialize();
            }
            bar::post_repaint();
            return;
        }

        // Tier 1: Native Windows 11/10 UIA Chevron Invoke / Toggle / LegacyIAccessible
        let uia_ok = tasks::automation()
            .and_then(|u| tasks::overflow(&u))
            .is_ok();

        let mut ov = null_mut();
        if uia_ok {
            for _ in 0..8 {
                let cand = find_overflow_window();
                if is_overflow_window_visible(cand) {
                    ov = cand;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        // Tier 2: Legacy Win32 TrayNotifyWnd -> Button BM_CLICK
        if ov.is_null() && click_legacy_tray_chevron() {
            for _ in 0..6 {
                let cand = find_overflow_window();
                if is_overflow_window_visible(cand) {
                    ov = cand;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        // Tier 3: Guaranteed Win+B -> Enter fallback (actually opens the flyout!)
        if ov.is_null() {
            bar::shortcut(0x42); // Win+B focuses the tray chevron
            std::thread::sleep(std::time::Duration::from_millis(110));
            send_key_tap(VK_RETURN);
            for _ in 0..14 {
                let cand = find_overflow_window();
                if is_overflow_window_visible(cand) {
                    ov = cand;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }

        if !ov.is_null() && is_overflow_window_visible(ov) {
            position_overflow_under_bar(ov, anchor_x, anchor_y);
            SetForegroundWindow(ov);
            // Re-align once after Windows XAML flyout entrance layout settles
            std::thread::sleep(std::time::Duration::from_millis(90));
            if is_overflow_window_visible(ov) {
                let mut r: RECT = zeroed();
                let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);
                if GetWindowRect(ov, &mut r) != 0 && r.top > screen_h / 2 {
                    position_overflow_under_bar(ov, anchor_x, anchor_y);
                }
            }
        }

        if com_ok {
            CoUninitialize();
        }
        bar::post_repaint();
    });
}
pub unsafe fn apply(h: HWND) {
    if let Some(data) = LATEST.lock().ok().and_then(|mut s| s.take()) {
        let apps_changed = MODEL.with_borrow_mut(|s| s.sync(data.apps));
        let uia_changed = system::merge_uia_status(&data.status);
        let net_changed = system::refresh_network_periodic();
        let fg_changed = system::refresh_foreground();
        let status_changed = bar::status(data.status);
        if apps_changed {
            surface::layout(h);
        } else {
            surface::ensure_topmost();
            if fg_changed && !surface::is_hidden() {
                surface::repaint_now();
            }
        }
        if status_changed || uia_changed || net_changed || fg_changed {
            bar::repaint();
        }
        if !STARTUP_TRIMMED.replace(true) {
            system::trim_working_set();
        }
    }
}
pub unsafe fn start(h: HWND) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    REQUEST.with_borrow_mut(|s| *s = Some(tx));
    SHELL_MSG.set(RegisterWindowMessageW(w("SHELLHOOK").as_ptr()));
    RegisterShellHookWindow(h);
    let handle = h as usize;
    std::thread::spawn(move || {
        use windows::Win32::System::Com::*;
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return;
        }
        if let Ok(uia) = tasks::automation() {
            loop {
                if let Ok(snapshot) = tasks::read(&uia) {
                    *LATEST.lock().unwrap() = Some(snapshot);
                    PostMessageW(handle as HWND, SYNC, 0, 0);
                }
                if matches!(
                    rx.recv_timeout(std::time::Duration::from_secs(30)),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                ) {
                    break;
                }
            }
        }
        CoUninitialize();
    });
}
pub unsafe fn switch(target: HWND, toggle: bool) -> bool {
    if IsWindow(target) == 0 {
        return false;
    }
    if toggle && GetForegroundWindow() == target && IsIconic(target) == 0 {
        return ShowWindowAsync(target, SW_MINIMIZE) != 0;
    }
    AllowSetForegroundWindow(ASFW_ANY);
    if IsIconic(target) != 0 {
        ShowWindowAsync(target, SW_RESTORE);
    } else if IsWindowVisible(target) == 0 {
        ShowWindow(target, SW_SHOW);
    }
    // Bridge input queues so our WS_EX_NOACTIVATE Dock can reliably raise & focus ANY app window!
    let fg = GetForegroundWindow();
    let my_tid = GetCurrentThreadId();
    let fg_tid = if !fg.is_null() {
        GetWindowThreadProcessId(fg, null_mut())
    } else {
        0
    };
    let target_tid = GetWindowThreadProcessId(target, null_mut());
    let att_fg = fg_tid != 0 && fg_tid != my_tid && AttachThreadInput(my_tid, fg_tid, 1) != 0;
    let att_tgt = target_tid != 0
        && target_tid != my_tid
        && target_tid != fg_tid
        && AttachThreadInput(my_tid, target_tid, 1) != 0;
    BringWindowToTop(target);
    let ok = SetForegroundWindow(target) != 0 || GetForegroundWindow() == target;
    if att_tgt {
        AttachThreadInput(my_tid, target_tid, 0);
    }
    if att_fg {
        AttachThreadInput(my_tid, fg_tid, 0);
    }
    ok
}

unsafe fn shell_exec_with_dir(target: &str, params: Option<&str>) -> bool {
    AllowSetForegroundWindow(ASFW_ANY);
    let target_w = w(target);
    let params_w = params.map(w);
    let dir_w = if !target.starts_with("shell:")
        && !target.contains("://")
        && !target.starts_with("ms-settings:")
    {
        std::path::Path::new(target)
            .parent()
            .filter(|p| p.is_dir())
            .map(|p| w(&p.to_string_lossy()))
    } else {
        None
    };
    ShellExecuteW(
        null_mut(),
        null(),
        target_w.as_ptr(),
        params_w.as_ref().map_or(null(), |v| v.as_ptr()),
        dir_w.as_ref().map_or(null(), |v| v.as_ptr()),
        SW_SHOWNORMAL,
    ) as isize
        > 32
}

pub unsafe fn launch(h: HWND, path: &str) {
    let resolved = tasks::resolve_helper_to_launcher(path);

    // Steam often runs with `-silent` in the system tray; `steam://open/main` wakes and focuses
    // the running client reliably AND starts Steam if not yet running!
    if tasks::is_steam_target(&resolved) {
        if shell_exec_with_dir("steam://open/main", None) {
            return;
        }
    }

    if shell_exec_with_dir(&resolved, None) {
        return;
    }

    // Fallback: if a shell:AppsFolder\<id> or stale path failed, try matching Start Menu shortcut!
    if let Some(fallback_lnk) = tasks::find_start_menu_shortcut(&resolved) {
        if shell_exec_with_dir(&fallback_lnk, None) {
            return;
        }
    }

    note(h, "この項目を開けませんでした。");
}

fn invoke_taskbar_async_with_fallback(owner: HWND, app_id: String, fallback_path: String) {
    let owner_u = owner as usize;
    std::thread::spawn(move || unsafe {
        use windows::Win32::System::Com::*;
        let com_ok = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let invoked = tasks::automation()
            .map(|uia| tasks::invoke_taskbar_app(&uia, &app_id))
            .unwrap_or(false);
        if com_ok {
            CoUninitialize();
        }
        if !invoked {
            launch(owner_u as HWND, &fallback_path);
        }
    });
}

pub unsafe fn activate(h: HWND, i: i32) {
    let (n_items, entry) = MODEL.with_borrow(|s| {
        (
            s.items.len(),
            s.items
                .get(i as usize)
                .map(|v| (v.path.clone(), v.app.clone())),
        )
    });
    if let Some((path, app)) = entry {
        // Clicking an app icon on the Dock cleanly dismisses any open Launcher or Panel
        panel::close();
        launcher::close_animated();
        let (wins, app_id, running) = match app {
            Some(a) => (a.windows, Some(a.id), a.running),
            None => (Vec::new(), None, false),
        };
        match wins.as_slice() {
            [] => {
                surface::trigger_bounce(h, &path, !running);
                // If Windows Taskbar reports the app IS running (e.g. Steam CEF/tray window,
                // elevated app, or custom AUMID whose window didn't match EnumWindows),
                // invoke the real Windows Taskbar button natively first, falling back to launch()!
                if running {
                    if let Some(id) = app_id {
                        invoke_taskbar_async_with_fallback(h, id, path);
                        return;
                    }
                }
                // For Steam or any app whose launch path is unresolved shell:AppsFolder\...,
                // try our upgraded launch() first; if custom/unmatched, taskbar UIA handles fallback.
                if let Some(id) = app_id {
                    let resolved = tasks::resolve_helper_to_launcher(&path);
                    if tasks::is_steam_target(&resolved) || tasks::is_steam_target(&id) {
                        if shell_exec_with_dir("steam://open/main", None) {
                            return;
                        }
                    }
                    if shell_exec_with_dir(&resolved, None) {
                        return;
                    }
                    invoke_taskbar_async_with_fallback(h, id, resolved);
                } else {
                    launch(h, &path);
                }
            }
            [(target, _)] => {
                surface::trigger_bounce(h, &path, false);
                if !switch(*target as HWND, true) {
                    if let Some(id) = app_id {
                        invoke_taskbar_async_with_fallback(h, id, path);
                    }
                }
            }
            _ => {
                let (ax, ay) = surface::item_anchor_screen(h, i);
                panel::open_dock_item(h, i, true, ax, ay);
            }
        }
    } else if i >= 0 && i as usize == n_items {
        // Built-in App Launcher Dock icon
        launcher::toggle(h);
    } else {
        let (ax, ay) = surface::item_anchor_screen(h, i);
        panel::open_settings(h, ax, ay);
    }
}
