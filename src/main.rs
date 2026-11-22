#![windows_subsystem = "windows"]
mod bar;
mod glass;
mod guard;
mod launcher;
mod panel;
mod settings;
mod surface;
mod sync;
mod system;
mod tasks;

use glass::GlassStyle;
use settings::Settings;
use std::{
    cell::{Cell, RefCell},
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{
        Controls::Dialogs::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*,
        WindowsAndMessaging::*,
    },
};

thread_local! {
    static MODEL: RefCell<Settings> = RefCell::new(unsafe { Settings::load() });
    static TRAY_CREATED: Cell<u32> = const { Cell::new(0) };
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn note(h: HWND, s: &str) {
    MessageBoxW(
        h,
        w(s).as_ptr(),
        w("MacUI").as_ptr(),
        MB_OK | MB_ICONINFORMATION,
    );
}

const TRAY: u32 = WM_APP + 7;

unsafe fn tray(h: HWND, operation: u32) {
    let mut d: NOTIFYICONDATAW = zeroed();
    d.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = h;
    d.uID = 1;
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    d.uCallbackMessage = TRAY;
    d.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    let tip = w("MacUI • クリックで終了してタスクバーを復元");
    d.szTip[..tip.len()].copy_from_slice(&tip);
    Shell_NotifyIconW(operation, &d);
}

unsafe fn menu(h: HWND, index: i32) {
    if index >= 0 && MODEL.with_borrow(|s| (index as usize) < s.items.len()) {
        let (ax, ay) = surface::item_anchor_screen(h, index);
        panel::open_dock_item(h, index, false, ax, ay);
    } else {
        let mut p = zeroed();
        GetCursorPos(&mut p);
        let (ax, ay) = if index >= 0 {
            surface::item_anchor_screen(h, index)
        } else {
            (p.x, p.y)
        };
        panel::open_settings(h, ax, ay);
    }
}

unsafe fn command(h: HWND, index: i32, id: i32) {
    match id {
        1 => {
            let mut buf = [0u16; 32768];
            let mut dlg: OPENFILENAMEW = zeroed();
            dlg.lStructSize = size_of::<OPENFILENAMEW>() as u32;
            dlg.hwndOwner = h;
            dlg.lpstrFile = buf.as_mut_ptr();
            dlg.nMaxFile = buf.len() as u32;
            dlg.Flags =
                OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT | OFN_NODEREFERENCELINKS;
            if GetOpenFileNameW(&mut dlg) != 0 {
                let path = String::from_utf16_lossy(
                    &buf[..buf.iter().position(|x| *x == 0).unwrap_or(buf.len())],
                );
                MODEL.with_borrow_mut(|s| s.pin(&path));
                changed(h);
            }
        }
        2 => {
            MODEL.with_borrow_mut(|s| {
                if s.items.get(index as usize).is_some_and(|i| i.custom) {
                    s.items.remove(index as usize);
                }
            });
            changed(h);
        }
        10..=12 => {
            MODEL.with_borrow_mut(|s| s.size = [46, 54, 64][(id - 10) as usize]);
            changed(h);
        }
        20..=22 => {
            MODEL.with_borrow_mut(|s| s.glass = GlassStyle::from_index((id - 20) as usize));
            changed(h);
        }
        30..=33 => {
            MODEL.with_borrow_mut(|s| s.magnify = (id - 30) as u8);
            changed(h);
        }
        40 => {
            MODEL.with_borrow_mut(|s| s.autohide = !s.autohide);
            changed(h);
        }
        99 => {
            DestroyWindow(h);
        }
        _ => {}
    }
}

unsafe fn changed(h: HWND) {
    if !MODEL.with_borrow(|s| s.save()) {
        note(h, "設定を保存できませんでした。");
    }
    surface::layout(h);
    bar::apply_style();
    panel::apply_style();
    launcher::apply_style();
}

unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps = zeroed();
            let dc = BeginPaint(h, &mut ps);
            let mut r = zeroed();
            GetClientRect(h, &mut r);
            FillRect(dc, &r, GetStockObject(BLACK_BRUSH));
            EndPaint(h, &ps);
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            system::refresh_ambient();
            surface::layout(h);
            bar::reposition();
        }
        WM_SETTINGCHANGE => {
            // Refresh Liquid Glass ambient color/luminance adaptation when wallpaper or OS theme changes
            system::refresh_ambient();
            bar::repaint();
            surface::repaint_now();
            panel::repaint_if_open();
        }
        WM_MOUSEMOVE => {
            let x = lp as i16 as i32;
            let y = (lp >> 16) as i16 as i32;
            surface::on_mouse_move(h, x, y);
        }
        0x02a3 => {
            // WM_MOUSELEAVE
            surface::on_mouse_leave(h);
        }
        WM_LBUTTONDOWN => {
            let x = lp as i16 as i32;
            let y = (lp >> 16) as i16 as i32;
            surface::on_mouse_down(h, x, y);
        }
        WM_LBUTTONUP => {
            let i = surface::on_mouse_up(h, lp);
            if i >= 0 {
                sync::activate(h, i);
            } else {
                panel::close();
                launcher::close_animated();
            }
        }
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
        WM_RBUTTONUP => menu(h, surface::hit(h, lp)),
        WM_DROPFILES => {
            let drop = wp as HDROP;
            let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
            for index in 0..count {
                let n = DragQueryFileW(drop, index, null_mut(), 0);
                let mut buf = vec![0; n as usize + 1];
                DragQueryFileW(drop, index, buf.as_mut_ptr(), n + 1);
                MODEL.with_borrow_mut(|s| s.pin(&String::from_utf16_lossy(&buf[..n as usize])));
            }
            DragFinish(drop);
            changed(h);
        }
        sync::SYNC => sync::apply(h),
        system::WM_SYS_UPDATE => {
            system::sync_atomic_audio();
            bar::repaint();
            panel::repaint_if_open();
        }
        WM_TIMER if wp == surface::ANIM_TIMER => {
            surface::on_timer(h);
        }
        WM_TIMER if wp == surface::EDGE_TIMER => {
            surface::on_edge_timer(h);
        }
        WM_TIMER if wp == 91 || wp == 92 => {
            KillTimer(h, 91);
            KillTimer(h, 92);
            sync::request();
        }
        _ if msg != 0 && msg == sync::SHELL_MSG.get() => {
            let code = wp & 0x7fff;
            if matches!(code, 1 | 2 | 4 | 13 | 14) {
                SetTimer(h, 91, 300, None);
            } else if code == 6 {
                // Coalesce frequent HSHELL_REDRAW title/progress updates without waking UIA continuously
                SetTimer(h, 92, 1200, None);
            }
        }
        WM_HOTKEY => {
            if wp == 2 {
                launcher::toggle(h);
            } else {
                DestroyWindow(h);
            }
        }
        WM_CLOSE => {
            DestroyWindow(h);
        }
        TRAY => {
            if lp as u32 == WM_LBUTTONUP {
                DestroyWindow(h);
            } else if lp as u32 == WM_RBUTTONUP {
                menu(h, -1);
            }
        }
        WM_DESTROY => {
            launcher::close();
            panel::close();
            bar::close();
            surface::close();
            system::stop();
            DeregisterShellHookWindow(h);
            tray(h, NIM_DELETE);
            UnregisterHotKey(h, 1);
            UnregisterHotKey(h, 2);
            PostQuitMessage(0);
        }
        _ if msg != 0 && msg == TRAY_CREATED.get() => {
            tray(h, NIM_ADD);
            bar::explorer_restarted();
            sync::request();
        }
        _ => return DefWindowProcW(h, msg, wp, lp),
    };
    0
}

fn main() {
    unsafe {
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("--restore-guard") {
            if let Some(pid) = args.get(2).and_then(|v| v.parse().ok()) {
                let bar_h = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(30);
                guard::run(pid, bar_h);
            }
            return;
        }
        if args.get(1).map(String::as_str) == Some("--scan-uwp") {
            launcher::run_scan_uwp_helper();
            return;
        }
        if args.get(1).map(String::as_str) == Some("--uwp-icons") {
            let size = args
                .get(2)
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(96)
                .clamp(48, 256);
            tasks::run_uwp_icons_helper(size);
            return;
        }
        let class = w("MacUI.GlassDock.1");
        if matches!(
            args.get(1).map(String::as_str),
            Some("--quit" | "--restore")
        ) {
            let h = FindWindowW(class.as_ptr(), null());
            if !h.is_null() {
                PostMessageW(h, WM_CLOSE, 0, 0);
                // Wait briefly for the main window to close before ensuring restore
                for _ in 0..40 {
                    if IsWindow(h) == 0 {
                        break;
                    }
                    Sleep(50);
                }
            }
            if args[1] == "--restore" || h.is_null() {
                guard::restore();
            }
            return;
        }
        let singleton = CreateMutexW(null(), 0, w("Local\\MacUI.Dock.1").as_ptr());
        if singleton.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        )
        .ok()
        .ok();
        glass::init();

        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(proc);
        wc.lpszClassName = class.as_ptr();
        wc.hInstance = GetModuleHandleW(null());
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        if RegisterClassW(&wc) == 0 {
            return;
        }
        let h = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class.as_ptr(),
            w("MacUI Dock").as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            wc.hInstance,
            null(),
        );
        if h.is_null() {
            return;
        }

        system::start(h);
        sync::start(h);
        bar::create(h);
        surface::init(h);
        surface::layout(h);
        ShowWindow(h, SW_SHOWNOACTIVATE);
        TRAY_CREATED.set(RegisterWindowMessageW(w("TaskbarCreated").as_ptr()));
        tray(h, NIM_ADD);
        RegisterHotKey(h, 1, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, 0x51);
        RegisterHotKey(h, 2, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, VK_SPACE as u32);
        DragAcceptFiles(h, 1);

        // Spawn guard completely detached from this process group/job so force-killing
        // the main process (e.g. via Task Manager) never kills the taskbar restore guard!
        let spawned = std::env::current_exe().and_then(|exe| {
            use std::os::windows::process::CommandExt;
            const DETACHED: u32 = 0x00000008 | 0x00000200 | 0x08000000; // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
            const BREAKAWAY: u32 = 0x01000000; // CREATE_BREAKAWAY_FROM_JOB
            let pid = std::process::id().to_string();
            let bh = bar::height().to_string();
            std::process::Command::new(&exe)
                .arg("--restore-guard")
                .arg(&pid)
                .arg(&bh)
                .creation_flags(DETACHED | BREAKAWAY)
                .spawn()
                .or_else(|_| {
                    std::process::Command::new(&exe)
                        .arg("--restore-guard")
                        .arg(&pid)
                        .arg(&bh)
                        .creation_flags(DETACHED)
                        .spawn()
                })
        });
        let guard_ok = spawned.is_ok();
        if !guard_ok {
            note(
                h,
                "復元プロセスを開始できないため、タスクバーを表示したままにします。",
            );
        }

        let mut msg = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        MODEL.with_borrow_mut(|s| s.items.clear());
        glass::shutdown();
        if !guard_ok {
            guard::restore();
        }
        CloseHandle(singleton);
    }
}
