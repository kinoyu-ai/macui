use super::*;
use windows_sys::Win32::{
    System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY},
    UI::{Accessibility::*, HiDpi::*},
};

// Explorer owns these properties for the lifetime of the taskbar window.
const LAYER_MARK: &str = "MacUI.TaskbarLayer.2";
const KEY_MARK: &str = "MacUI.TaskbarColor.2";
const MARK: &str = "MacUI.RestoreTaskbar.1";
const MONITORINFOF_PRIMARY: u32 = 1;
const SYNCHRONIZE_ACCESS: u32 = 0x00100000;

#[derive(Clone, Copy)]
struct PrimaryMonitor {
    rc_monitor: RECT,
    rc_work: RECT,
}

thread_local! {
    static GUARD_BAR_HEIGHT: Cell<i32> = const { Cell::new(30) };
}

unsafe fn primary_taskbar(h: HWND) -> bool {
    let mut name = [0u16; 64];
    let n = GetClassNameW(h, name.as_mut_ptr(), 64);
    String::from_utf16_lossy(&name[..n as usize]) == "Shell_TrayWnd"
}

unsafe fn any_taskbar(h: HWND) -> bool {
    let mut name = [0u16; 64];
    let n = GetClassNameW(h, name.as_mut_ptr(), 64);
    matches!(
        String::from_utf16_lossy(&name[..n as usize]).as_str(),
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

unsafe fn hide(h: HWND) {
    if IsWindowVisible(h) == 0 || !primary_taskbar(h) {
        return;
    }
    if !GetPropW(h, w(LAYER_MARK).as_ptr()).is_null() {
        return;
    }
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    let mut key = 0;
    let mut alpha = 255u8;
    let mut flags = 0;
    if ex & WS_EX_LAYERED != 0
        && GetLayeredWindowAttributes(h, &mut key, &mut alpha, &mut flags) == 0
    {
        return;
    }
    let state = 1
        | ((ex & WS_EX_LAYERED != 0) as usize) << 1
        | ((ex & WS_EX_TRANSPARENT != 0) as usize) << 2
        | (alpha as usize) << 8
        | (flags as usize) << 16;
    if SetPropW(h, w(KEY_MARK).as_ptr(), (key as usize + 1) as HANDLE) == 0 {
        return;
    }
    if SetPropW(h, w(LAYER_MARK).as_ptr(), state as HANDLE) == 0 {
        RemovePropW(h, w(KEY_MARK).as_ptr());
        return;
    }
    SetWindowLongW(
        h,
        GWL_EXSTYLE,
        (ex | WS_EX_LAYERED | WS_EX_TRANSPARENT) as i32,
    );
    if SetLayeredWindowAttributes(h, 0, 0, LWA_ALPHA) == 0 {
        restore_window(h, true);
    }
}

unsafe extern "system" fn each(h: HWND, _: LPARAM) -> i32 {
    hide(h);
    1
}

unsafe extern "system" fn shown(
    _: HWINEVENTHOOK,
    _: u32,
    h: HWND,
    obj: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    if obj == 0 && child == 0 && primary_taskbar(h) {
        hide(h);
        apply_primary_workarea(GUARD_BAR_HEIGHT.get());
    }
}

unsafe extern "system" fn find_primary_monitor(
    hmon: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    let mut info: MONITORINFO = zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(hmon, &mut info) != 0 && (info.dwFlags & MONITORINFOF_PRIMARY) != 0 {
        *(data as *mut Option<PrimaryMonitor>) = Some(PrimaryMonitor {
            rc_monitor: info.rcMonitor,
            rc_work: info.rcWork,
        });
        return 0;
    }
    1
}

unsafe fn query_primary_monitor() -> Option<PrimaryMonitor> {
    let mut out: Option<PrimaryMonitor> = None;
    EnumDisplayMonitors(
        null_mut(),
        null(),
        Some(find_primary_monitor),
        (&mut out as *mut Option<PrimaryMonitor>) as isize,
    );
    out
}

fn rect_eq(a: RECT, b: RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

unsafe extern "system" fn notify_maximized_window(h: HWND, data: LPARAM) -> i32 {
    if IsWindowVisible(h) == 0 || IsZoomed(h) == 0 || any_taskbar(h) {
        return 1;
    }
    let mut cls = [0u16; 64];
    let n = GetClassNameW(h, cls.as_mut_ptr(), 64);
    let name = String::from_utf16_lossy(&cls[..n as usize]);
    if matches!(name.as_str(), "Progman" | "WorkerW") || name.starts_with("MacUI.") {
        return 1;
    }
    // Only notify windows on the primary monitor; leave secondary monitors completely untouched!
    let mon = &*(data as *const RECT);
    let mut wr = zeroed();
    if GetWindowRect(h, &mut wr) != 0 {
        let cx = (wr.left + wr.right) / 2;
        let cy = (wr.top + wr.bottom) / 2;
        if cx < mon.left || cx >= mon.right || cy < mon.top || cy >= mon.bottom {
            return 1;
        }
    }
    let mut res = 0usize;
    SendMessageTimeoutW(
        h,
        WM_SETTINGCHANGE,
        SPI_SETWORKAREA as usize,
        0,
        SMTO_ABORTIFHUNG | SMTO_NORMAL,
        40,
        &mut res,
    );
    1
}

/// Updates kernel WorkArea immediately.
/// - While MacUI is active (`broadcast = false`), avoids broadcasting SPIF_SENDCHANGE so Explorer's
///   hidden Shell_TrayWnd does not immediately shrink WorkArea.bottom back to H-48.
/// - When restoring (`broadcast = true`), sends SPIF_SENDCHANGE so Explorer, DWM, and all maximized
///   windows snap cleanly back to the restored desktop bounds (0px top gap).
unsafe fn set_workarea_rect(mut rc: RECT, broadcast: bool) {
    let flags = if broadcast { SPIF_SENDCHANGE } else { 0 };
    if SystemParametersInfoW(SPI_SETWORKAREA, 0, (&mut rc as *mut RECT).cast(), flags) != 0 {
        if let Some(m) = query_primary_monitor() {
            EnumWindows(
                Some(notify_maximized_window),
                (&m.rc_monitor as *const RECT) as isize,
            );
        }
    }
}

/// Pure geometry helper: subtracts a docked taskbar rect from the primary monitor bounds.
fn subtract_taskbar_rect(mon: RECT, r: RECT) -> RECT {
    let mut work = mon;
    if r.right <= r.left || r.bottom <= r.top {
        return work;
    }
    // Ignore taskbar rects belonging to a different monitor
    if r.right <= mon.left || r.left >= mon.right || r.bottom <= mon.top || r.top >= mon.bottom {
        return work;
    }
    let tw = r.right - r.left;
    let th = r.bottom - r.top;
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    if tw >= th {
        if cy > (mon.top + mon.bottom) / 2 {
            work.bottom = work.bottom.min(r.top).max(mon.top);
        } else {
            work.top = work.top.max(r.bottom).min(mon.bottom);
        }
    } else if cx > (mon.left + mon.right) / 2 {
        work.right = work.right.min(r.left).max(mon.left);
    } else {
        work.left = work.left.max(r.right).min(mon.right);
    }
    work
}

/// Reads Windows Explorer's saved taskbar auto-hide flag and docked rect from `StuckRects3`
/// as a fallback when `Shell_TrayWnd` is temporarily unreachable (e.g. Explorer restart).
unsafe fn read_taskbar_from_registry() -> Option<(bool, RECT)> {
    let subkey = w("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StuckRects3");
    let value = w("Settings");
    let mut buf = [0u8; 64];
    let mut cb = buf.len() as u32;
    let status = RegGetValueW(
        HKEY_CURRENT_USER,
        subkey.as_ptr(),
        value.as_ptr(),
        RRF_RT_REG_BINARY,
        null_mut(),
        buf.as_mut_ptr().cast(),
        &mut cb,
    );
    if status != 0 || (cb as usize) < 40 {
        return None;
    }
    let autohide = (buf[8] & 1) != 0;
    let rect = RECT {
        left: i32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
        top: i32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]),
        right: i32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]),
        bottom: i32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
    };
    Some((autohide, rect))
}

/// Expands ONLY the primary monitor work area so maximized windows sit flush under the
/// top Menu Bar and reach the very bottom edge (0px bottom gap!), never touching secondary monitors.
pub unsafe fn apply_primary_workarea(bar_height: i32) {
    let Some(m) = query_primary_monitor() else {
        return;
    };
    let desired = RECT {
        left: m.rc_monitor.left,
        top: m.rc_monitor.top + bar_height.max(0),
        right: m.rc_monitor.right,
        bottom: m.rc_monitor.bottom,
    };
    if !rect_eq(m.rc_work, desired) {
        set_workarea_rect(desired, false);
    }
}

unsafe fn restore_primary_workarea_from_taskbar() {
    let Some(m) = query_primary_monitor() else {
        return;
    };
    let tb = FindWindowW(w("Shell_TrayWnd").as_ptr(), null());
    let work = if !tb.is_null() {
        let mut abd: APPBARDATA = zeroed();
        abd.cbSize = size_of::<APPBARDATA>() as u32;
        let autohide = (SHAppBarMessage(ABM_GETSTATE, &mut abd) & ABS_AUTOHIDE as usize) != 0;
        if autohide {
            m.rc_monitor
        } else {
            let mut r: RECT = zeroed();
            let got = IsWindowVisible(tb) != 0
                && GetWindowRect(tb, &mut r) != 0
                && r.right > r.left
                && r.bottom > r.top;
            if got {
                subtract_taskbar_rect(m.rc_monitor, r)
            } else if SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) != 0 {
                subtract_taskbar_rect(m.rc_monitor, abd.rc)
            } else if let Some((reg_autohide, reg_r)) = read_taskbar_from_registry() {
                if reg_autohide {
                    m.rc_monitor
                } else {
                    subtract_taskbar_rect(m.rc_monitor, reg_r)
                }
            } else {
                m.rc_monitor
            }
        }
    } else if let Some((reg_autohide, reg_r)) = read_taskbar_from_registry() {
        if reg_autohide {
            m.rc_monitor
        } else {
            subtract_taskbar_rect(m.rc_monitor, reg_r)
        }
    } else {
        m.rc_monitor
    };
    // Always apply with SPIF_SENDCHANGE on restore so maximized windows snap cleanly back to top=0
    set_workarea_rect(work, true);
}

unsafe fn restore_window(h: HWND, force_unconditional: bool) {
    if !any_taskbar(h) {
        return;
    }
    let saved = GetPropW(h, w(LAYER_MARK).as_ptr()) as usize;
    let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
    let mut needs_fix = saved != 0;

    if !needs_fix && force_unconditional {
        let mut key = 0u32;
        let mut alpha = 255u8;
        let mut flags = 0u32;
        let invisible_layer = ex & WS_EX_LAYERED != 0
            && GetLayeredWindowAttributes(h, &mut key, &mut alpha, &mut flags) != 0
            && (flags & LWA_ALPHA != 0)
            && alpha < 255;
        if invisible_layer || (ex & WS_EX_TRANSPARENT != 0) || IsWindowVisible(h) == 0 {
            needs_fix = true;
        }
    }

    if needs_fix {
        let mut style = ex;
        if saved != 0 {
            let key = (GetPropW(h, w(KEY_MARK).as_ptr()) as usize).saturating_sub(1) as u32;
            if saved & 2 == 0 {
                // Reset alpha to 255 BEFORE removing WS_EX_LAYERED so DWM never caches alpha=0!
                SetLayeredWindowAttributes(h, 0, 255, LWA_ALPHA);
                style &= !WS_EX_LAYERED;
            } else {
                SetLayeredWindowAttributes(h, key, (saved >> 8) as u8, (saved >> 16) as u32);
            }
            if saved & 4 == 0 {
                style &= !WS_EX_TRANSPARENT;
            }
            RemovePropW(h, w(KEY_MARK).as_ptr());
            RemovePropW(h, w(LAYER_MARK).as_ptr());
        } else {
            if style & WS_EX_LAYERED != 0 {
                SetLayeredWindowAttributes(h, 0, 255, LWA_ALPHA);
                style &= !WS_EX_LAYERED;
            }
            style &= !WS_EX_TRANSPARENT;
        }
        SetWindowLongW(h, GWL_EXSTYLE, style as i32);
        SetWindowPos(
            h,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOACTIVATE
                | SWP_FRAMECHANGED
                | SWP_SHOWWINDOW,
        );
        ShowWindow(h, SW_SHOWNA);
        RedrawWindow(
            h,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }

    let mark = w(MARK);
    if !GetPropW(h, mark.as_ptr()).is_null() {
        ShowWindow(h, SW_SHOWNA);
        RemovePropW(h, mark.as_ptr());
    }
}

unsafe extern "system" fn restore_each_marked(h: HWND, _: LPARAM) -> i32 {
    restore_window(h, false);
    1
}

unsafe extern "system" fn restore_each_force(h: HWND, _: LPARAM) -> i32 {
    restore_window(h, true);
    1
}

pub unsafe fn run(pid: u32, bar_height: i32) {
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let parent = OpenProcess(SYNCHRONIZE_ACCESS, 0, pid);
    if parent.is_null() {
        return;
    }
    // Serialize restoration against a new Dock started immediately after the old one quits.
    let mutex = CreateMutexW(null(), 0, w("Local\\MacUI.Taskbars.1").as_ptr());
    if mutex.is_null() {
        CloseHandle(parent);
        return;
    }
    let lock = WaitForSingleObject(mutex, INFINITE);
    if (lock == WAIT_OBJECT_0 || lock == WAIT_ABANDONED)
        && WaitForSingleObject(parent, 0) == WAIT_TIMEOUT
    {
        GUARD_BAR_HEIGHT.set(bar_height.max(0));

        // Pure windowless watchdog process (never creates a top-level GUI window)
        let hook = SetWinEventHook(
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_SHOW,
            null_mut(),
            Some(shown),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        EnumWindows(Some(each), 0);
        apply_primary_workarea(bar_height);

        while MsgWaitForMultipleObjects(1, &parent, 0, INFINITE, QS_ALLINPUT) == WAIT_OBJECT_0 + 1 {
            let mut msg = zeroed();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&msg);
            }
        }

        if !hook.is_null() {
            UnhookWinEvent(hook);
        }
        EnumWindows(Some(restore_each_marked), 0);
        // Always recompute WorkArea from the live restored taskbar (never replay a stale
        // top=30 WorkArea left behind by a prior crash or resolution/DPI change!)
        restore_primary_workarea_from_taskbar();
    }
    ReleaseMutex(mutex);
    CloseHandle(mutex);
    CloseHandle(parent);
}

/// Unconditional restoration for `--restore` / `restore-taskbar.cmd` and clean exit fallback.
pub unsafe fn restore() {
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    EnumWindows(Some(restore_each_force), 0);
    restore_primary_workarea_from_taskbar();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_taskbar_restores_zero_top_gap() {
        let mon = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        let bottom_tb = RECT {
            left: 0,
            top: 1392,
            right: 2560,
            bottom: 1440,
        };
        let w = subtract_taskbar_rect(mon, bottom_tb);
        assert_eq!((w.left, w.top, w.right, w.bottom), (0, 0, 2560, 1392));

        let top_tb = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 48,
        };
        let wt = subtract_taskbar_rect(mon, top_tb);
        assert_eq!((wt.left, wt.top, wt.right, wt.bottom), (0, 48, 2560, 1440));

        let other_mon_tb = RECT {
            left: 2560,
            top: 1392,
            right: 5120,
            bottom: 1440,
        };
        let wo = subtract_taskbar_rect(mon, other_mon_tb);
        assert_eq!((wo.left, wo.top, wo.right, wo.bottom), (0, 0, 2560, 1440));
    }

    #[test]
    fn registry_taskbar_fallback_parses_live_stuckrects3() {
        unsafe {
            if let Some((autohide, r)) = read_taskbar_from_registry() {
                println!(
                    "StuckRects3 parsed: autohide={autohide} rect=({},{},{},{})",
                    r.left, r.top, r.right, r.bottom
                );
                assert!(r.right > r.left && r.bottom > r.top);
            }
        }
    }
}
