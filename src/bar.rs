use super::*;
use crate::glass::{
    d2d_rect, draw_battery_icon, draw_chevron_icon, draw_control_center_icon, draw_ime_badge,
    draw_mac_emblem, draw_menu_bar_glass, draw_search_icon, draw_text, draw_volume_icon,
    draw_wifi_icon, measure_text_width, pt, rgba, solid, GlassSurface, SurfaceRole, TextAlign,
    TextWeight,
};
use windows::Win32::Graphics::Direct2D::{Common::D2D_RECT_F, D2D1_ELLIPSE, D2D1_ROUNDED_RECT};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BarItem {
    AppleMenu,
    ActiveApp,
    FileMenu,
    ViewMenu,
    WindowMenu,
    TrayOverflow,
    Ime,
    Battery,
    Wifi,
    Volume,
    Spotlight,
    ControlCenter,
    Clock,
}

struct HitZone {
    rect: D2D_RECT_F,
    item: BarItem,
}

struct BarState {
    hwnd: HWND,
    owner: HWND,
    surface: Option<GlassSurface>,
    status: tasks::Status,
    hits: Vec<HitZone>,
    hovered: Option<BarItem>,
    placing: bool,
    fullscreen: bool,
    width: i32,
    height: i32,
}

impl Default for BarState {
    fn default() -> Self {
        Self {
            hwnd: null_mut(),
            owner: null_mut(),
            surface: None,
            status: tasks::Status::default(),
            hits: Vec::new(),
            hovered: None,
            placing: false,
            fullscreen: false,
            width: 0,
            height: 0,
        }
    }
}

thread_local! {
    static STATE: RefCell<BarState> = RefCell::new(BarState::default());
}

pub unsafe fn create(owner: HWND) {
    panel::register("MacUI.Status", Some(proc));
    let h = CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        w("MacUI.Status").as_ptr(),
        w("MacUI ステータスバー").as_ptr(),
        WS_POPUP,
        0,
        0,
        1,
        1,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if h.is_null() {
        return;
    }
    let style = MODEL.with_borrow(|s| s.glass);
    // Single seamless full-bar Liquid Glass backdrop (zero double-backdrop seams)
    let surf = GlassSurface::new(h, SurfaceRole::Bar, &[], style);
    STATE.with_borrow_mut(|st| {
        st.hwnd = h;
        st.owner = owner;
        st.surface = surf;
    });
    // Position directly without registering a conflicting Explorer AppBar;
    // primary monitor work area is managed cleanly by guard.rs, and fullscreen apps
    // are detected via EVENT_SYSTEM_FOREGROUND in system.rs.
    reposition_inner(false);
    ShowWindow(h, SW_SHOWNOACTIVATE);
    tick(h);
}

pub unsafe fn set_fullscreen(fullscreen: bool) {
    let h = STATE.with_borrow_mut(|st| {
        if st.hwnd.is_null() || st.fullscreen == fullscreen {
            return null_mut();
        }
        st.fullscreen = fullscreen;
        st.hwnd
    });
    if !h.is_null() {
        SetWindowPos(
            h,
            if fullscreen {
                HWND_BOTTOM
            } else {
                HWND_TOPMOST
            },
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

pub unsafe fn height() -> i32 {
    STATE.with_borrow(|st| if st.height > 0 { st.height } else { 30 })
}

unsafe fn tick(h: HWND) {
    let mut t = zeroed();
    GetLocalTime(&mut t);
    let ms = 60000u32
        .saturating_sub(t.wSecond as u32 * 1000 + t.wMilliseconds as u32)
        .max(250);
    SetTimer(h, 1, ms, None);
    repaint();
}

pub unsafe fn explorer_restarted() {
    let h = STATE.with_borrow(|st| st.hwnd);
    if !h.is_null() {
        reposition();
    }
}

pub unsafe fn reposition() {
    reposition_inner(true);
}

unsafe fn reposition_inner(update_workarea: bool) {
    let h = STATE.with_borrow_mut(|st| {
        if st.hwnd.is_null() || st.placing {
            return null_mut();
        }
        st.placing = true;
        st.hwnd
    });
    if h.is_null() {
        return;
    }
    let dpi = GetDpiForWindow(h).max(96) as i32;
    let height = (30 * dpi / 96).max(26);
    let width = GetSystemMetrics(SM_CXSCREEN).max(1);
    SetWindowPos(h, HWND_TOPMOST, 0, 0, width, height, SWP_NOACTIVATE);
    STATE.with_borrow_mut(|st| {
        st.width = width;
        st.height = height;
        st.placing = false;
    });
    if update_workarea {
        guard::apply_primary_workarea(height);
    }
    repaint();
}

pub unsafe fn status(s: tasks::Status) -> bool {
    STATE.with_borrow_mut(|st| {
        if st.status == s {
            return false;
        }
        st.status = s;
        true
    })
}

pub fn notification_count() -> String {
    STATE.with_borrow(|st| st.status.notifications.clone())
}

pub unsafe fn apply_style() {
    let style = MODEL.with_borrow(|s| s.glass);
    STATE.with_borrow(|st| {
        if let Some(surf) = &st.surface {
            surf.set_style(style);
        }
    });
    repaint();
}

const WM_BAR_REPAINT: u32 = WM_APP + 61;

pub unsafe fn post_repaint() {
    let h = STATE.with_borrow(|st| st.hwnd);
    if !h.is_null() {
        PostMessageW(h, WM_BAR_REPAINT, 0, 0);
    }
}

pub unsafe fn close() {
    let h = STATE.with_borrow_mut(|st| {
        st.surface = None;
        let h = st.hwnd;
        st.hwnd = null_mut();
        h
    });
    if !h.is_null() {
        DestroyWindow(h);
    }
}

pub unsafe fn shortcut(key: u16) {
    let event = |vk: u16, up: bool| INPUT {
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
    let mut keys = vec![event(VK_LWIN, false)];
    if key != VK_LWIN {
        keys.extend([event(key, false), event(key, true)]);
    }
    keys.push(event(VK_LWIN, true));
    SendInput(keys.len() as u32, keys.as_ptr(), size_of::<INPUT>() as i32);
}

pub unsafe fn shortcut_shift(key: u16) {
    let event = |vk: u16, up: bool| INPUT {
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
    let keys = [
        event(VK_LWIN, false),
        event(VK_LSHIFT, false),
        event(key, false),
        event(key, true),
        event(VK_LSHIFT, true),
        event(VK_LWIN, true),
    ];
    SendInput(keys.len() as u32, keys.as_ptr(), size_of::<INPUT>() as i32);
}

fn format_mac_clock() -> String {
    unsafe {
        let mut t = zeroed();
        GetLocalTime(&mut t);
        let wd = ["日", "月", "火", "水", "木", "金", "土"][(t.wDayOfWeek as usize) % 7];
        format!(
            "{}月{}日({}) {:02}:{:02}",
            t.wMonth, t.wDay, wd, t.wHour, t.wMinute
        )
    }
}

pub unsafe fn repaint() {
    let style = MODEL.with_borrow(|s| s.glass);
    let sys = system::state();

    STATE.with_borrow_mut(|st| {
        if st.hwnd.is_null() || st.width <= 0 || st.height <= 0 {
            return;
        }
        let dpi = GetDpiForWindow(st.hwnd).max(96) as f32 / 96.0;
        let w = st.width;
        let h = st.height;
        let wf = w as f32;
        let hf = h as f32;
        let hovered = st.hovered;
        let notif_raw = st.status.notifications.clone();
        let has_unread = notif_raw.parse::<u32>().unwrap_or(0) > 0;
        let active_tag = if launcher::is_open() {
            "launcher".to_owned()
        } else if sync::is_overflow_open() {
            "overflow".to_owned()
        } else {
            panel::open_tag()
        };

        let mut hits: Vec<HitZone> = Vec::with_capacity(14);
        let Some(surf) = st.surface.as_mut() else {
            return;
        };

        let tone = sys.bar_tone;
        let is_light = tone.is_light(style);

        // Always render full-width Liquid Glass backdrop across Clear / Standard / Dark
        // (never raw transparent wallpaper showing through the top status bar!)
        surf.main_backdrop.set_rect(0.0, 0.0, wf, hf, 0.0, 1.0);

        surf.draw(w, h, |ctx, wf, hf| {
            draw_menu_bar_glass(ctx, wf, hf, style, tone);

            let fg = if is_light {
                rgba(0.06, 0.08, 0.12, 0.92)
            } else {
                rgba(1.0, 1.0, 1.0, 0.96)
            };
            let fg_soft = if is_light {
                rgba(0.08, 0.10, 0.14, 0.84)
            } else {
                rgba(1.0, 1.0, 1.0, 0.90)
            };
            let icon_sh = if is_light {
                rgba(1.0, 1.0, 1.0, 0.35)
            } else {
                rgba(0.0, 0.0, 0.0, 0.38)
            };
            let txt_sh = !is_light;
            let pill_pad_y = 2.5 * dpi;
            let pill_r = (hf - pill_pad_y * 2.0) * 0.5;
            let font_sz = 13.0 * dpi;

            let draw_pill = |rect: D2D_RECT_F, item: BarItem, tag: &str| {
                let is_open = active_tag == tag && !tag.is_empty();
                let is_hov = hovered == Some(item);
                if is_open || is_hov {
                    let pill = d2d_rect(rect.left, pill_pad_y, rect.right, hf - pill_pad_y);
                    let pr = D2D1_ROUNDED_RECT {
                        rect: pill,
                        radiusX: pill_r,
                        radiusY: pill_r,
                    };
                    let (fill_c, rim_c) = if is_light {
                        (
                            rgba(0.0, 0.0, 0.0, if is_open { 0.14 } else { 0.08 }),
                            rgba(0.0, 0.0, 0.0, if is_open { 0.22 } else { 0.12 }),
                        )
                    } else {
                        (
                            rgba(1.0, 1.0, 1.0, if is_open { 0.24 } else { 0.14 }),
                            rgba(1.0, 1.0, 1.0, if is_open { 0.44 } else { 0.24 }),
                        )
                    };
                    if let Some(b) = solid(ctx, fill_c) {
                        ctx.FillRoundedRectangle(&pr, &b);
                    }
                    if let Some(rim) = solid(ctx, rim_c) {
                        ctx.DrawRoundedRectangle(&pr, &rim, 0.9, None);
                    }
                }
            };

            // ---------- LEFT SIDE: Authentic macOS Menu Bar ----------
            let mut x = 8.0 * dpi;

            // 1. Apple / MacUI Emblem
            let apple_w = 34.0 * dpi;
            let apple_r = d2d_rect(x, 0.0, x + apple_w, hf);
            draw_pill(apple_r, BarItem::AppleMenu, "apple");
            draw_mac_emblem(ctx, x + apple_w * 0.5, hf * 0.5 + 0.9, 14.5 * dpi, icon_sh);
            draw_mac_emblem(ctx, x + apple_w * 0.5, hf * 0.5, 14.5 * dpi, fg);
            hits.push(HitZone {
                rect: apple_r,
                item: BarItem::AppleMenu,
            });
            x += apple_w + 2.0 * dpi;

            // 2. Active Foreground App Name (Bold, authentic macOS)
            let app_label = if sys.active_app.is_empty() {
                "Finder".to_owned()
            } else {
                sys.active_app.clone()
            };
            let app_w = (measure_text_width(&app_label, font_sz, TextWeight::Bold) + 20.0 * dpi)
                .clamp(54.0 * dpi, 220.0 * dpi);
            let app_r = d2d_rect(x, 0.0, x + app_w, hf);
            draw_pill(app_r, BarItem::ActiveApp, "app");
            draw_text(
                ctx,
                &app_label,
                d2d_rect(
                    app_r.left + 8.0 * dpi,
                    0.0,
                    app_r.right - 8.0 * dpi,
                    hf - 1.0,
                ),
                font_sz,
                TextWeight::Bold,
                fg,
                TextAlign::Center,
                txt_sh,
            );
            hits.push(HitZone {
                rect: app_r,
                item: BarItem::ActiveApp,
            });
            x += app_w + 2.0 * dpi;

            // 3. Functional macOS Menus (ファイル / 表示 / ウィンドウ)
            for (label, item, tag) in [
                ("ファイル", BarItem::FileMenu, "file"),
                ("表示", BarItem::ViewMenu, "view"),
                ("ウィンドウ", BarItem::WindowMenu, "window"),
            ] {
                let mw = measure_text_width(label, font_sz, TextWeight::Regular) + 18.0 * dpi;
                let mr = d2d_rect(x, 0.0, x + mw, hf);
                draw_pill(mr, item, tag);
                draw_text(
                    ctx,
                    label,
                    d2d_rect(mr.left, 0.0, mr.right, hf - 1.0),
                    font_sz,
                    TextWeight::Regular,
                    fg_soft,
                    TextAlign::Center,
                    txt_sh,
                );
                hits.push(HitZone { rect: mr, item });
                x += mw + 1.0 * dpi;
            }

            // ---------- RIGHT SIDE: Authentic macOS System Status Icons + Clock ----------
            let mut right = wf - 8.0 * dpi;
            let icon_s = 15.0 * dpi;

            // 1. Date & Time (`11月21日(土) 07:25`) + subtle glowing notification dot
            let clock = format_mac_clock();
            let clock_text_w = measure_text_width(&clock, font_sz, TextWeight::Medium);
            let dot_extra = if has_unread { 12.0 * dpi } else { 0.0 };
            let clock_w = clock_text_w + 18.0 * dpi + dot_extra;
            let clock_r = d2d_rect(right - clock_w, 0.0, right, hf);
            if clock_r.left > x + 20.0 * dpi {
                draw_pill(clock_r, BarItem::Clock, "");
                draw_text(
                    ctx,
                    &clock,
                    d2d_rect(
                        clock_r.left + 8.0 * dpi,
                        0.0,
                        clock_r.right - 8.0 * dpi - dot_extra,
                        hf - 1.0,
                    ),
                    font_sz,
                    TextWeight::Medium,
                    fg,
                    TextAlign::Center,
                    txt_sh,
                );
                if has_unread {
                    let dx = clock_r.right - 9.0 * dpi;
                    let dy = hf * 0.5;
                    if let Some(b) = solid(ctx, rgba(0.42, 0.72, 1.0, 0.95)) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(dx, dy),
                                radiusX: 3.1 * dpi,
                                radiusY: 3.1 * dpi,
                            },
                            &b,
                        );
                    }
                }
                hits.push(HitZone {
                    rect: clock_r,
                    item: BarItem::Clock,
                });
                right = clock_r.left - 2.0 * dpi;
            }

            // 2. Control Center Icon (Double Toggle Switches)
            let cc_w = 30.0 * dpi;
            let cc_r = d2d_rect(right - cc_w, 0.0, right, hf);
            if cc_r.left > x + 16.0 * dpi {
                draw_pill(cc_r, BarItem::ControlCenter, "cc");
                let cx = (cc_r.left + cc_r.right) * 0.5;
                draw_control_center_icon(ctx, cx, hf * 0.5 + 0.9, icon_s, icon_sh);
                draw_control_center_icon(ctx, cx, hf * 0.5, icon_s, fg);
                hits.push(HitZone {
                    rect: cc_r,
                    item: BarItem::ControlCenter,
                });
                right = cc_r.left - 1.0 * dpi;
            }

            // 3. Spotlight Search Icon (opens Liquid Glass App Launcher!)
            let sp_w = 28.0 * dpi;
            let sp_r = d2d_rect(right - sp_w, 0.0, right, hf);
            if sp_r.left > x + 16.0 * dpi {
                draw_pill(sp_r, BarItem::Spotlight, "launcher");
                let cx = (sp_r.left + sp_r.right) * 0.5;
                draw_search_icon(ctx, cx, hf * 0.5 + 0.9, icon_s * 0.96, icon_sh);
                draw_search_icon(ctx, cx, hf * 0.5, icon_s * 0.96, fg);
                hits.push(HitZone {
                    rect: sp_r,
                    item: BarItem::Spotlight,
                });
                right = sp_r.left - 1.0 * dpi;
            }

            // 4. Volume Vector Icon (dynamic sound waves / mute)
            let vol_w = 28.0 * dpi;
            let vol_r = d2d_rect(right - vol_w, 0.0, right, hf);
            if vol_r.left > x + 16.0 * dpi {
                draw_pill(vol_r, BarItem::Volume, "cc");
                let cx = (vol_r.left + vol_r.right) * 0.5;
                draw_volume_icon(
                    ctx,
                    cx,
                    hf * 0.5 + 0.9,
                    icon_s,
                    sys.volume,
                    sys.muted,
                    icon_sh,
                );
                draw_volume_icon(ctx, cx, hf * 0.5, icon_s, sys.volume, sys.muted, fg);
                hits.push(HitZone {
                    rect: vol_r,
                    item: BarItem::Volume,
                });
                right = vol_r.left - 1.0 * dpi;
            }

            // 5. Wi-Fi / Ethernet Vector Icon (opens Liquid Glass Network Panel!)
            let wifi_w = 28.0 * dpi;
            let wifi_r = d2d_rect(right - wifi_w, 0.0, right, hf);
            if wifi_r.left > x + 16.0 * dpi {
                draw_pill(wifi_r, BarItem::Wifi, "net");
                let cx = (wifi_r.left + wifi_r.right) * 0.5;
                draw_wifi_icon(ctx, cx, hf * 0.5 + 0.9, icon_s, sys.wifi_signal, icon_sh);
                draw_wifi_icon(ctx, cx, hf * 0.5, icon_s, sys.wifi_signal, fg);
                hits.push(HitZone {
                    rect: wifi_r,
                    item: BarItem::Wifi,
                });
                right = wifi_r.left - 1.0 * dpi;
            }

            // 6. Battery Vector Icon (shown only if device has a battery)
            if sys.battery_present {
                let pct_txt = format!("{}%", sys.battery_percent);
                let pct_w = measure_text_width(&pct_txt, 11.5 * dpi, TextWeight::Medium);
                let bat_w = pct_w + 34.0 * dpi;
                let bat_r = d2d_rect(right - bat_w, 0.0, right, hf);
                if bat_r.left > x + 16.0 * dpi {
                    draw_pill(bat_r, BarItem::Battery, "");
                    draw_text(
                        ctx,
                        &pct_txt,
                        d2d_rect(
                            bat_r.left + 6.0 * dpi,
                            0.0,
                            bat_r.left + 8.0 * dpi + pct_w,
                            hf - 1.0,
                        ),
                        11.5 * dpi,
                        TextWeight::Medium,
                        fg_soft,
                        TextAlign::Right,
                        txt_sh,
                    );
                    draw_battery_icon(
                        ctx,
                        bat_r.right - 14.0 * dpi,
                        hf * 0.5,
                        icon_s,
                        sys.battery_percent,
                        sys.battery_charging,
                        fg,
                    );
                    hits.push(HitZone {
                        rect: bat_r,
                        item: BarItem::Battery,
                    });
                    right = bat_r.left - 2.0 * dpi;
                }
            }

            // 7. Input Source / IME Badge (`あ` / `ア` / `A`)
            let ime_w = 28.0 * dpi;
            let ime_r = d2d_rect(right - ime_w, 0.0, right, hf);
            if ime_r.left > x + 16.0 * dpi {
                draw_pill(ime_r, BarItem::Ime, "");
                let badge = if sys.ime_badge.is_empty() {
                    "A"
                } else {
                    &sys.ime_badge
                };
                draw_ime_badge(
                    ctx,
                    (ime_r.left + ime_r.right) * 0.5,
                    hf * 0.5,
                    14.5 * dpi,
                    badge,
                    fg,
                );
                hits.push(HitZone {
                    rect: ime_r,
                    item: BarItem::Ime,
                });
                right = ime_r.left - 2.0 * dpi;
            }

            // 8. Discreet Hidden System Tray Overflow Chevron (`^`)
            let tray_w = 24.0 * dpi;
            let tray_r = d2d_rect(right - tray_w, 0.0, right, hf);
            if tray_r.left > x + 16.0 * dpi {
                draw_pill(tray_r, BarItem::TrayOverflow, "overflow");
                let cx = (tray_r.left + tray_r.right) * 0.5;
                draw_chevron_icon(ctx, cx, hf * 0.5 + 0.8, icon_s * 0.88, icon_sh);
                draw_chevron_icon(ctx, cx, hf * 0.5, icon_s * 0.88, fg_soft);
                hits.push(HitZone {
                    rect: tray_r,
                    item: BarItem::TrayOverflow,
                });
            }
        });

        st.hits = hits;
    });
}

unsafe fn find_hit(x: i32) -> Option<(BarItem, D2D_RECT_F)> {
    let xf = x as f32;
    STATE.with_borrow(|st| {
        st.hits
            .iter()
            .find(|z| xf >= z.rect.left && xf < z.rect.right)
            .map(|z| (z.item, z.rect))
    })
}

unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps = zeroed();
            BeginPaint(h, &mut ps);
            EndPaint(h, &ps);
            repaint();
        }
        WM_BAR_REPAINT => {
            repaint();
        }
        WM_TIMER if wp == 2 => {
            KillTimer(h, 2);
            system::refresh_foreground();
            repaint();
        }
        WM_TIMER | WM_TIMECHANGE => tick(h),
        WM_POWERBROADCAST => {
            system::refresh_battery();
            repaint();
        }
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
        WM_MOUSEMOVE => {
            let x = lp as i16 as i32;
            let hit = find_hit(x);
            let hit_item = hit.map(|v| v.0);
            let (was_none, changed) = STATE.with_borrow_mut(|st| {
                let wn = st.hovered.is_none();
                if st.hovered != hit_item {
                    st.hovered = hit_item;
                    (wn, true)
                } else {
                    (wn, false)
                }
            });
            if was_none && hit_item.is_some() {
                system::refresh_battery();
            }
            if changed {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
                // Authentic macOS Menu Bar scrubbing: gliding across open menus switches cleanly!
                let cur_tag = panel::open_tag();
                if matches!(
                    cur_tag.as_str(),
                    "apple" | "app" | "file" | "view" | "window"
                ) {
                    if let Some((item, rect)) = hit {
                        let (owner, bar_h) = STATE.with_borrow(|st| (st.owner, st.height));
                        let ay = bar_h + 6;
                        match item {
                            BarItem::AppleMenu if cur_tag != "apple" => {
                                panel::open_apple_menu(owner, rect.left as i32, ay)
                            }
                            BarItem::ActiveApp if cur_tag != "app" => {
                                panel::open_active_app_menu(owner, rect.left as i32, ay)
                            }
                            BarItem::FileMenu if cur_tag != "file" => {
                                panel::open_file_menu(owner, rect.left as i32, ay)
                            }
                            BarItem::ViewMenu if cur_tag != "view" => {
                                panel::open_view_menu(owner, rect.left as i32, ay)
                            }
                            BarItem::WindowMenu if cur_tag != "window" => {
                                panel::open_window_menu(owner, rect.left as i32, ay)
                            }
                            _ => {}
                        }
                    }
                }
                repaint();
            }
        }
        0x02a3 => {
            // WM_MOUSELEAVE
            let changed = STATE.with_borrow_mut(|st| st.hovered.take().is_some());
            if changed {
                repaint();
            }
        }
        WM_MOUSEWHEEL => {
            // Smooth macOS-like scroll over Volume icon adjusts system volume directly!
            let mut pt_s = zeroed();
            GetCursorPos(&mut pt_s);
            if let Some((BarItem::Volume, _)) = find_hit(pt_s.x) {
                let delta = if (wp >> 16) as i16 > 0 { 4 } else { -4 };
                system::adjust_volume(delta);
                repaint();
                panel::repaint_if_open();
            }
        }
        WM_RBUTTONUP => {
            let x = lp as i16 as i32;
            let (owner, height) = STATE.with_borrow(|st| (st.owner, st.height));
            if let Some((item, rect)) = find_hit(x) {
                match item {
                    BarItem::Volume => {
                        system::toggle_mute();
                        repaint();
                        panel::repaint_if_open();
                    }
                    BarItem::Wifi => {
                        panel::open_network(owner, rect.right as i32, height + 6, false);
                    }
                    BarItem::Battery | BarItem::ControlCenter => {
                        panel::open_control_center(owner, rect.right as i32, height + 6);
                    }
                    BarItem::TrayOverflow => {
                        panel::close();
                        launcher::close();
                        sync::overflow(
                            owner,
                            ((rect.left + rect.right) * 0.5).round() as i32,
                            height + 6,
                        );
                    }
                    _ => {
                        panel::open_settings(owner, x, height + 6);
                    }
                }
                repaint();
            } else {
                // Clicked blank space on the Menu Bar -> dismiss any open Panel or Launcher!
                panel::close();
                launcher::close_animated();
                repaint();
            }
        }
        WM_LBUTTONUP => {
            let x = lp as i16 as i32;
            let (owner, bar_h) = STATE.with_borrow(|st| (st.owner, st.height));
            if let Some((item, rect)) = find_hit(x) {
                let anchor_y = bar_h + 6;
                match item {
                    BarItem::AppleMenu => {
                        if panel::toggle_close_if("apple") {
                            repaint();
                            return 0;
                        }
                        panel::open_apple_menu(owner, rect.left as i32, anchor_y);
                    }
                    BarItem::ActiveApp => {
                        if panel::toggle_close_if("app") {
                            repaint();
                            return 0;
                        }
                        panel::open_active_app_menu(owner, rect.left as i32, anchor_y);
                    }
                    BarItem::FileMenu => {
                        if panel::toggle_close_if("file") {
                            repaint();
                            return 0;
                        }
                        panel::open_file_menu(owner, rect.left as i32, anchor_y);
                    }
                    BarItem::ViewMenu => {
                        if panel::toggle_close_if("view") {
                            repaint();
                            return 0;
                        }
                        panel::open_view_menu(owner, rect.left as i32, anchor_y);
                    }
                    BarItem::WindowMenu => {
                        if panel::toggle_close_if("window") {
                            repaint();
                            return 0;
                        }
                        panel::open_window_menu(owner, rect.left as i32, anchor_y);
                    }
                    BarItem::TrayOverflow => {
                        panel::close();
                        launcher::close();
                        sync::overflow(
                            owner,
                            ((rect.left + rect.right) * 0.5).round() as i32,
                            anchor_y,
                        );
                    }
                    BarItem::Ime => {
                        system::toggle_ime();
                        SetTimer(h, 2, 180, None);
                    }
                    BarItem::Spotlight => {
                        launcher::toggle(owner);
                    }
                    BarItem::Clock => shortcut(0x4e), // Win+N Notification Center
                    BarItem::Wifi => {
                        if panel::toggle_close_if("net") {
                            repaint();
                            return 0;
                        }
                        panel::open_network(owner, rect.right as i32, anchor_y, false);
                    }
                    BarItem::ControlCenter | BarItem::Volume | BarItem::Battery => {
                        if panel::toggle_close_if("cc") {
                            repaint();
                            return 0;
                        }
                        panel::open_control_center(owner, rect.right as i32, anchor_y);
                    }
                }
                repaint();
            } else {
                // Clicked blank space on the Menu Bar -> dismiss any open Panel or Launcher!
                panel::close();
                launcher::close_animated();
                repaint();
            }
        }
        _ => return DefWindowProcW(h, msg, wp, lp),
    };
    0
}
