use super::*;
use crate::glass::{
    d2d_rect, draw_dock_launcher_icon, draw_dock_settings_icon, draw_icon_with_badge,
    draw_liquid_glass_pill, draw_text, measure_text_width, pt, radial_grad, rgba, solid,
    GlassSurface, SurfaceRole, TextAlign, TextWeight,
};
use std::time::Instant;
use windows::Win32::Graphics::Direct2D::{
    Common::D2D1_GRADIENT_STOP, ID2D1DeviceContext, D2D1_ELLIPSE, D2D1_ROUNDED_RECT,
};

pub const ANIM_TIMER: usize = 42;
pub const EDGE_TIMER: usize = 89;
const ICON_FETCH_SIZE: i32 = 112;

struct Bounce {
    key: String,
    start: Instant,
    big: bool,
}

#[derive(Clone, Copy)]
struct RestMetrics {
    dpi: f32,
    screen_w: i32,
    screen_h: i32,
    n_total: i32,
    sep_after: i32, // separator appears after this index (if > 0)
    base_icon: f32,
    gap: f32,
    pitch: f32,
    sep_w: f32,
    pad_x: f32,
    pill_h: f32,
    bottom_margin: f32,
    rest_w: f32,
    stage_left: i32,
    stage_w: i32,
    stage_h: i32,
}

struct ComputedItem {
    center_x: f32,
    size: f32,
    scale: f32,
    bounce_y: f32,
}

struct DockState {
    hitbox: HWND,
    visual_hwnd: HWND,
    surface: Option<GlassSurface>,
    fullscreen: bool,
    // Smooth animation state
    hovering: bool,
    cursor_stage_x: f32,
    smooth_mouse_x: f32,
    hover_strength: f32, // 0..1 smooth magnification envelope
    shimmer_alpha: f32,  // 0..1 cursor liquid glass glint
    hovered_idx: i32,
    pressed_idx: i32,
    press_amount: f32,
    tooltip_alpha: f32,
    tooltip_x: f32,
    tooltip_w: f32,
    tooltip_label: String,
    hide_progress: f32, // 0=visible, 1=hidden off bottom
    hidden_cleared: bool,
    anim_running: bool,
    edge_running: bool,
    last_frame: Instant,
    bounces: Vec<Bounce>,
    // Last drawn item layout for crisp hit-testing during magnification
    last_centers: Vec<(f32, f32)>, // (center_x, width) in stage coordinates
    last_pill_left: f32,
    last_pill_right: f32,
    last_pill_top: f32,
    last_vis_rect: (i32, i32, i32, i32, bool),
}

impl Default for DockState {
    fn default() -> Self {
        Self {
            hitbox: null_mut(),
            visual_hwnd: null_mut(),
            surface: None,
            fullscreen: false,
            hovering: false,
            cursor_stage_x: 0.0,
            smooth_mouse_x: 0.0,
            hover_strength: 0.0,
            shimmer_alpha: 0.0,
            hovered_idx: -1,
            pressed_idx: -1,
            press_amount: 0.0,
            tooltip_alpha: 0.0,
            tooltip_x: 0.0,
            tooltip_w: 80.0,
            tooltip_label: String::new(),
            hide_progress: 0.0,
            hidden_cleared: false,
            anim_running: false,
            edge_running: false,
            last_frame: Instant::now(),
            bounces: Vec::new(),
            last_centers: Vec::new(),
            last_pill_left: 0.0,
            last_pill_right: 0.0,
            last_pill_top: 0.0,
            last_vis_rect: (0, 0, 0, 0, false),
        }
    }
}

thread_local! {
    static STATE: RefCell<DockState> = RefCell::new(DockState::default());
}

unsafe extern "system" fn visual_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_ERASEBKGND => 1,
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

pub unsafe fn init(hitbox: HWND) {
    let class = w("MacUI.DockVisual");
    let mut wc: WNDCLASSW = zeroed();
    wc.lpfnWndProc = Some(visual_proc);
    wc.lpszClassName = class.as_ptr();
    wc.hInstance = GetModuleHandleW(null());
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
    RegisterClassW(&wc);

    let visual = CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP
            | WS_EX_LAYERED
            | WS_EX_TRANSPARENT
            | WS_EX_TOOLWINDOW
            | WS_EX_TOPMOST
            | WS_EX_NOACTIVATE,
        class.as_ptr(),
        w("MacUI Dock Surface").as_ptr(),
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

    let style = MODEL.with_borrow(|s| s.glass);
    let surf = if !visual.is_null() {
        GlassSurface::new(visual, SurfaceRole::Dock, &[SurfaceRole::Dock], style)
    } else {
        None
    };

    STATE.with_borrow_mut(|st| {
        st.hitbox = hitbox;
        st.visual_hwnd = visual;
        st.surface = surf;
        st.last_frame = Instant::now();
    });

    if !visual.is_null() {
        ShowWindow(visual, SW_SHOWNOACTIVATE);
    }
}

pub unsafe fn close() {
    STATE.with_borrow_mut(|st| {
        if st.anim_running && !st.hitbox.is_null() {
            KillTimer(st.hitbox, ANIM_TIMER);
            st.anim_running = false;
        }
        if st.edge_running && !st.hitbox.is_null() {
            KillTimer(st.hitbox, EDGE_TIMER);
            st.edge_running = false;
        }
        st.surface = None;
        if !st.visual_hwnd.is_null() {
            DestroyWindow(st.visual_hwnd);
            st.visual_hwnd = null_mut();
        }
    });
}

unsafe fn sync_edge_timer(h: HWND, autohide: bool) {
    STATE.with_borrow_mut(|st| {
        if autohide && !st.edge_running {
            SetTimer(h, EDGE_TIMER, 150, None);
            st.edge_running = true;
        } else if !autohide && st.edge_running {
            KillTimer(h, EDGE_TIMER);
            st.edge_running = false;
        }
    });
}

/// Ensures Dock windows stay topmost if Windows/Explorer/popups pushed them down over time.
pub unsafe fn ensure_topmost() {
    if panel::is_open() || launcher::is_open() || sync::is_overflow_open() {
        return;
    }
    let (h, visual, fullscreen) =
        STATE.with_borrow(|st| (st.hitbox, st.visual_hwnd, st.fullscreen));
    if h.is_null() || fullscreen {
        return;
    }
    let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOSENDCHANGING;
    if !visual.is_null() {
        SetWindowPos(visual, HWND_TOPMOST, 0, 0, 0, 0, flags);
    }
    SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, flags);
}

/// Wakes the auto-hide animation cleanly after a Panel or Launcher closes.
pub unsafe fn poke_autohide() {
    if !MODEL.with_borrow(|s| s.autohide) {
        return;
    }
    let h = STATE.with_borrow(|st| st.hitbox);
    if !h.is_null() {
        ensure_animating(h);
    }
}

/// Immediately hides an auto-hidden Dock if the user clicked into another app/desktop away from it.
pub unsafe fn on_foreground_change() {
    if !MODEL.with_borrow(|s| s.autohide) || panel::is_open() || launcher::is_open() {
        return;
    }
    let (h, hovering) = STATE.with_borrow(|st| (st.hitbox, st.hovering));
    if h.is_null() || !hovering {
        return;
    }
    let mut pt_s: POINT = zeroed();
    if GetCursorPos(&mut pt_s) == 0 {
        return;
    }
    let m = compute_rest_metrics(h);
    let extra_top = (m.base_icon * 0.72).round() as i32;
    let top_bound = m.screen_h - m.pill_h as i32 - extra_top - m.bottom_margin as i32 - 16;
    if pt_s.y < top_bound {
        STATE.with_borrow_mut(|st| {
            st.hovering = false;
            st.pressed_idx = -1;
        });
        ensure_animating(h);
    }
}

/// Zero-CPU 120ms bottom-edge cursor watcher (active ONLY when auto-hide is enabled).
/// Guarantees the Dock ALWAYS wakes when the cursor touches the bottom screen edge,
/// even if another topmost window or shell overlay sat above the thin trigger strip,
/// and recovers cleanly from any missed WM_MOUSELEAVE.
pub unsafe fn on_edge_timer(h: HWND) {
    let autohide = MODEL.with_borrow(|s| s.autohide);
    if !autohide {
        sync_edge_timer(h, false);
        return;
    }
    let mut pt_s: POINT = zeroed();
    if GetCursorPos(&mut pt_s) == 0 {
        return;
    }
    let screen_w = GetSystemMetrics(SM_CXSCREEN).max(640);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);
    let (fullscreen, hovering, hide_p, pill_l, pill_r, anim_running, pressed_idx) = STATE
        .with_borrow(|st| {
            (
                st.fullscreen,
                st.hovering,
                st.hide_progress,
                st.last_pill_left,
                st.last_pill_right,
                st.anim_running,
                st.pressed_idx,
            )
        });
    if fullscreen {
        return;
    }
    let dpi = GetDpiForWindow(h).max(96) as f32 / 96.0;
    let trigger_h = (6.0 * dpi).round().max(6.0) as i32;
    let on_primary_x = pt_s.x >= 0 && pt_s.x < screen_w;
    let at_bottom_edge = on_primary_x && pt_s.y >= screen_h - trigger_h && pt_s.y <= screen_h;

    if !hovering && !at_bottom_edge {
        if !anim_running && !panel::is_open() && !launcher::is_open() && hide_p < 0.99 {
            ensure_animating(h);
        }
        return;
    }

    if !hovering && at_bottom_edge {
        let m = compute_rest_metrics(h);
        STATE.with_borrow_mut(|st| {
            st.hovering = true;
            st.cursor_stage_x = pt_s.x as f32;
            st.smooth_mouse_x = pt_s.x as f32;
            st.tooltip_x = pt_s.x as f32;
        });
        position_windows_inner(h, &m, true);
        ensure_animating(h);
        on_timer(h);
        return;
    }

    // Never prematurely hide while cursor is resting along the bottom edge trigger strip!
    if hovering && !at_bottom_edge && pressed_idx < 0 && !panel::is_open() && !launcher::is_open() {
        let m = compute_rest_metrics(h);
        let extra_top = (m.base_icon * 0.72).round() as i32;
        let top_bound = m.screen_h - m.pill_h as i32 - extra_top - m.bottom_margin as i32 - 24;
        let left_bound = (if pill_r > pill_l {
            pill_l
        } else {
            (m.screen_w as f32 - m.rest_w) * 0.5
        }) as i32
            - 28;
        let right_bound = (if pill_r > pill_l {
            pill_r
        } else {
            (m.screen_w as f32 + m.rest_w) * 0.5
        }) as i32
            + 28;
        if pt_s.y < top_bound || pt_s.x < left_bound || pt_s.x > right_bound {
            STATE.with_borrow_mut(|st| {
                st.hovering = false;
            });
            ensure_animating(h);
        }
    }
}

unsafe fn compute_rest_metrics(h: HWND) -> RestMetrics {
    let dpi = GetDpiForWindow(h).max(96) as f32 / 96.0;
    let screen_w = GetSystemMetrics(SM_CXSCREEN).max(640);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);

    MODEL.with_borrow(|s| {
        let n_items = s.items.len() as i32;
        let n_apps = s.items.iter().filter(|i| !i.custom).count() as i32;
        // Trailing built-in Dock icons: [n_items] = App Launcher, [n_items + 1] = MacUI Settings
        let n_total = (n_items + 2).max(2);
        let sep_after = if n_apps > 0 { n_apps } else { 0 };

        let pad_x = (12.0 * dpi).round();
        let sep_w = if sep_after > 0 {
            (12.0 * dpi).round()
        } else {
            0.0
        };
        let desired_icon = (s.size as f32) * dpi;
        let desired_gap = (7.0 * dpi).round().max(4.0);
        // Leave comfortable horizontal margin on both sides for smooth magnification expansion!
        let max_rest_w = (screen_w as f32 * 0.88 - 2.0 * pad_x - sep_w).max(200.0);
        let pitch = (desired_icon + desired_gap).min(max_rest_w / n_total as f32);
        let gap = (pitch * 0.14).clamp(3.0 * dpi, desired_gap);
        let base_icon = (pitch - gap).max(24.0 * dpi);
        let pill_h = (base_icon + 16.0 * dpi).round();
        let bottom_margin = (6.0 * dpi).round();
        let rest_w = (2.0 * pad_x + n_total as f32 * pitch + sep_w).round();
        let mag_boost = s.mag_boost();
        let max_icon = base_icon * (1.0 + mag_boost);
        let max_lift = mag_boost * base_icon * 0.18 + base_icon * 0.55;
        let stage_h = (bottom_margin
            + 9.0 * dpi
            + max_icon
            + max_lift
            + 10.0 * dpi
            + 28.0 * dpi
            + 16.0 * dpi)
            .ceil()
            .max(pill_h + 72.0 * dpi) as i32;
        let stage_w = ((rest_w + 3.2 * base_icon * mag_boost + 280.0 * dpi).ceil() as i32)
            .clamp(320, screen_w);
        let stage_left = (screen_w - stage_w) / 2;

        RestMetrics {
            dpi,
            screen_w,
            screen_h,
            n_total,
            sep_after,
            base_icon,
            gap,
            pitch,
            sep_w,
            pad_x,
            pill_h,
            bottom_margin,
            rest_w,
            stage_left,
            stage_w,
            stage_h,
        }
    })
}

pub unsafe fn layout(h: HWND) {
    // Load any missing high-resolution icons (112x112 premultiplied BGRA).
    // Fast local .lnk/.exe icons decode directly via PrivateExtractIconsW (zero shell extensions);
    // remaining UWP / virtual items decode together in ONE disposable `--uwp-icons` subprocess.
    let missing = MODEL.with_borrow(|s| {
        s.items
            .iter()
            .filter(|i| i.icon_loaded_size != ICON_FETCH_SIZE)
            .map(|i| {
                (
                    i.path.clone(),
                    i.app
                        .as_ref()
                        .map(|a| a.image.clone())
                        .unwrap_or_else(|| i.path.clone()),
                    i.fallback_icon_idx,
                )
            })
            .collect::<Vec<_>>()
    });
    let mut need_uwp: Vec<(String, String, i32)> = Vec::new();
    for (key, image, fallback_idx) in missing {
        if let Some(raw) = tasks::extract_clean_icon_fast(&image, ICON_FETCH_SIZE) {
            MODEL.with_borrow_mut(|s| {
                if let Some(item) = s
                    .items
                    .iter_mut()
                    .find(|i| i.path == key && i.icon_loaded_size != ICON_FETCH_SIZE)
                {
                    item.raw_icon = Some(raw);
                    item.d2d_bitmap = None;
                    item.icon_loaded_size = ICON_FETCH_SIZE;
                }
            });
        } else {
            need_uwp.push((key, image, fallback_idx));
        }
    }
    if !need_uwp.is_empty() {
        let images: Vec<String> = need_uwp.iter().map(|(_, img, _)| img.clone()).collect();
        let decoded = tasks::extract_uwp_icons_batch(&images, ICON_FETCH_SIZE);
        for ((key, _, fallback_idx), raw) in need_uwp.into_iter().zip(decoded) {
            let final_raw =
                raw.or_else(|| tasks::fallback_shell32_icon(fallback_idx, ICON_FETCH_SIZE));
            MODEL.with_borrow_mut(|s| {
                if let Some(item) = s
                    .items
                    .iter_mut()
                    .find(|i| i.path == key && i.icon_loaded_size != ICON_FETCH_SIZE)
                {
                    item.raw_icon = final_raw;
                    item.d2d_bitmap = None;
                    item.icon_loaded_size = ICON_FETCH_SIZE;
                }
            });
        }
    }

    let m = compute_rest_metrics(h);
    let (style, autohide) = MODEL.with_borrow(|s| (s.glass, s.autohide));

    STATE.with_borrow_mut(|st| {
        if let Some(surf) = &st.surface {
            surf.set_style(style);
        }
        if !autohide {
            st.hide_progress = 0.0;
        }
    });

    sync_edge_timer(h, autohide);
    position_windows_inner(h, &m, true);
    render(h);
    if autohide {
        ensure_animating(h);
    }
}

unsafe fn position_windows(h: HWND, m: &RestMetrics) {
    position_windows_inner(h, m, false);
}

unsafe fn position_windows_inner(h: HWND, m: &RestMetrics, force_z: bool) {
    let (visual, fullscreen, autohide, hovering, hide_p) = STATE.with_borrow(|st| {
        (
            st.visual_hwnd,
            st.fullscreen,
            MODEL.with_borrow(|s| s.autohide),
            st.hovering,
            st.hide_progress,
        )
    });

    // Invisible interactive hitbox window (alpha = 1/255 = completely imperceptible)
    SetLayeredWindowAttributes(h, 0, 1, LWA_ALPHA);

    let z_visual = if fullscreen {
        HWND_BOTTOM
    } else {
        HWND_TOPMOST
    };

    if !visual.is_null() {
        let want = (
            m.stage_left,
            m.screen_h - m.stage_h,
            m.stage_w,
            m.stage_h,
            fullscreen,
        );
        let lost_topmost =
            !fullscreen && (GetWindowLongW(visual, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST) == 0;
        let changed = STATE.with_borrow_mut(|st| {
            if st.last_vis_rect != want || force_z || lost_topmost {
                st.last_vis_rect = want;
                true
            } else {
                false
            }
        });
        if changed {
            SetWindowPos(
                visual,
                z_visual,
                want.0,
                want.1,
                want.2,
                want.3,
                SWP_NOACTIVATE | SWP_NOSENDCHANGING,
            );
        }
    }

    // Hitbox window covers exact rest pill (or expanded area while hovering, or DPI-scaled bottom trigger strip when hidden)
    let trigger_h = (6.0 * m.dpi).round().max(6.0) as i32;
    let (hx, hy, hw, hh) = if autohide && !hovering && hide_p > 0.85 {
        let tw = ((m.rest_w as i32) + 240)
            .max(m.screen_w * 3 / 4)
            .min(m.screen_w);
        ((m.screen_w - tw) / 2, m.screen_h - trigger_h, tw, trigger_h)
    } else if hovering {
        let (pl, pr) = STATE.with_borrow(|st| {
            if st.last_pill_right > st.last_pill_left {
                (st.last_pill_left, st.last_pill_right)
            } else {
                let left = (m.screen_w as f32 - m.rest_w) * 0.5;
                (left, left + m.rest_w)
            }
        });
        let extra_top = (m.base_icon * 0.65).round() as i32;
        let hw = (pr - pl).ceil() as i32;
        let hx = (pl.floor() as i32).clamp(0, (m.screen_w - hw).max(0));
        let hh = m.pill_h as i32 + extra_top + m.bottom_margin as i32;
        (hx, m.screen_h - hh, hw.max(60), hh)
    } else {
        let hw = m.rest_w as i32;
        let hh = m.pill_h as i32;
        let hx = (m.screen_w - hw) / 2;
        let hy = m.screen_h - hh - m.bottom_margin as i32;
        (hx, hy, hw, hh)
    };

    // Always place hitbox at HWND_TOPMOST (above click-through visual!) so hits never get blocked!
    let z_hit = if fullscreen {
        HWND_BOTTOM
    } else {
        HWND_TOPMOST
    };

    SetWindowPos(
        h,
        z_hit,
        hx,
        hy,
        hw,
        hh,
        SWP_NOACTIVATE | SWP_NOSENDCHANGING,
    );
}

pub unsafe fn set_fullscreen(fullscreen: bool) {
    let changed = STATE.with_borrow_mut(|st| {
        if st.fullscreen == fullscreen {
            return false;
        }
        st.fullscreen = fullscreen;
        true
    });
    if changed {
        let h = STATE.with_borrow(|st| st.hitbox);
        if !h.is_null() {
            let m = compute_rest_metrics(h);
            position_windows(h, &m);
        }
    }
}

unsafe fn ensure_animating(h: HWND) {
    STATE.with_borrow_mut(|st| {
        if !st.anim_running {
            st.anim_running = true;
            st.last_frame = Instant::now();
            SetTimer(h, ANIM_TIMER, 12, None);
        }
    });
}

pub unsafe fn on_mouse_move(h: HWND, local_x: i32, _local_y: i32) {
    let mut r = zeroed();
    GetWindowRect(h, &mut r);
    let stage_x = (r.left + local_x) as f32;

    let mut track = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: h,
        dwHoverTime: 0,
    };
    TrackMouseEvent(&mut track);

    let was_hovering = STATE.with_borrow_mut(|st| {
        let was = st.hovering;
        st.hovering = true;
        st.cursor_stage_x = stage_x;
        if !was {
            st.smooth_mouse_x = stage_x;
            st.tooltip_x = stage_x;
        }
        was
    });

    if !was_hovering {
        let m = compute_rest_metrics(h);
        position_windows(h, &m);
    }
    ensure_animating(h);
    on_timer(h);
}

pub unsafe fn on_mouse_leave(h: HWND) {
    STATE.with_borrow_mut(|st| {
        st.hovering = false;
        st.pressed_idx = -1;
    });
    ensure_animating(h);
}

pub unsafe fn on_mouse_down(h: HWND, local_x: i32, local_y: i32) {
    let idx = hit(
        h,
        ((local_y as u32 & 0xFFFF) << 16 | (local_x as u32 & 0xFFFF)) as LPARAM,
    );
    STATE.with_borrow_mut(|st| {
        st.pressed_idx = idx;
    });
    ensure_animating(h);
}

pub unsafe fn on_mouse_up(h: HWND, lp: LPARAM) -> i32 {
    let idx = hit(h, lp);
    STATE.with_borrow_mut(|st| {
        st.pressed_idx = -1;
    });
    ensure_animating(h);
    idx
}

pub unsafe fn trigger_bounce(h: HWND, key: &str, big: bool) {
    STATE.with_borrow_mut(|st| {
        st.bounces.retain(|b| b.key != key);
        st.bounces.push(Bounce {
            key: key.to_owned(),
            start: Instant::now(),
            big,
        });
    });
    ensure_animating(h);
}

pub unsafe fn hit(h: HWND, lp: LPARAM) -> i32 {
    let lx = lp as i16 as i32;
    let ly = (lp >> 16) as i16 as i32;
    let mut wr = zeroed();
    GetWindowRect(h, &mut wr);
    let m = compute_rest_metrics(h);
    let stage_x = (wr.left + lx) as f32;
    let width = (wr.right - wr.left).max(1);
    let height = (wr.bottom - wr.top).max(1);

    if lx < 0 || lx >= width || ly < 0 || ly >= height {
        return -1;
    }

    // Use live rendered icon centers if available (handles smooth magnification hit-testing!)
    let live = STATE.with_borrow(|st| {
        if st.last_centers.len() == m.n_total as usize {
            let mut best_i = -1;
            let mut best_d = f32::MAX;
            for (i, (cx, w)) in st.last_centers.iter().enumerate() {
                let d = (stage_x - *cx).abs();
                if d <= *w * 0.62 && d < best_d {
                    best_d = d;
                    best_i = i as i32;
                }
            }
            if best_i >= 0 {
                return Some(best_i);
            }
        }
        None
    });
    if let Some(i) = live {
        return i;
    }

    // Fallback to clean rest layout math
    let rel = lx as f32 - m.pad_x;
    if rel < 0.0 {
        return 0;
    }
    let sep_pos = m.sep_after as f32 * m.pitch;
    let adj = if m.sep_after > 0 && rel >= sep_pos + m.sep_w * 0.5 {
        rel - m.sep_w
    } else {
        rel
    };
    ((adj / m.pitch).floor() as i32).clamp(0, m.n_total - 1)
}

pub unsafe fn item_anchor_screen(h: HWND, index: i32) -> (i32, i32) {
    let m = compute_rest_metrics(h);
    STATE.with_borrow(|st| {
        let cx = st
            .last_centers
            .get(index as usize)
            .map(|(cx, _)| *cx as i32)
            .unwrap_or(m.screen_w / 2);
        let stage_top_screen = m.screen_h - m.stage_h;
        let pill_top_screen = stage_top_screen + st.last_pill_top.round() as i32;
        (cx, pill_top_screen)
    })
}

pub fn is_hidden() -> bool {
    STATE.with_borrow(|st| st.hidden_cleared && !st.hovering && st.hide_progress >= 0.995)
}

pub unsafe fn repaint_now() {
    let h = STATE.with_borrow(|st| st.hitbox);
    if !h.is_null() {
        render(h);
    }
}

pub unsafe fn on_timer(h: HWND) {
    let now = Instant::now();
    let autohide = MODEL.with_borrow(|s| s.autohide);
    let mut still_active = false;
    let mut need_reposition = false;

    STATE.with_borrow_mut(|st| {
        let dt = now
            .duration_since(st.last_frame)
            .as_secs_f32()
            .clamp(0.001, 0.05);
        st.last_frame = now;

        // Critically damped exponential smoothing factors
        let k_hover = 1.0 - (-dt * 18.0).exp();
        let k_mouse = 1.0 - (-dt * 24.0).exp();
        let k_press = 1.0 - (-dt * 28.0).exp();
        let k_hide = 1.0 - (-dt * 14.0).exp();
        let k_tip = 1.0 - (-dt * 20.0).exp();

        let target_hover = if st.hovering { 1.0 } else { 0.0 };
        st.hover_strength += (target_hover - st.hover_strength) * k_hover;
        if (target_hover - st.hover_strength).abs() < 0.002 {
            st.hover_strength = target_hover;
        } else {
            still_active = true;
        }

        let target_shimmer = if st.hovering { 1.0 } else { 0.0 };
        st.shimmer_alpha += (target_shimmer - st.shimmer_alpha) * k_hover;
        if (target_shimmer - st.shimmer_alpha).abs() < 0.002 {
            st.shimmer_alpha = target_shimmer;
        } else {
            still_active = true;
        }

        if st.hovering {
            let dx = st.cursor_stage_x - st.smooth_mouse_x;
            st.smooth_mouse_x += dx * k_mouse;
            if dx.abs() > 0.35 {
                still_active = true;
            } else {
                st.smooth_mouse_x = st.cursor_stage_x;
            }
        }

        let target_press = if st.pressed_idx >= 0 { 1.0 } else { 0.0 };
        st.press_amount += (target_press - st.press_amount) * k_press;
        if (target_press - st.press_amount).abs() < 0.003 {
            st.press_amount = target_press;
        } else {
            still_active = true;
        }

        // Auto-hide slide animation (only hide when cursor is off dock AND panels/launcher are closed)
        let target_hide = if autohide && !st.hovering && !panel::is_open() && !launcher::is_open() {
            1.0
        } else {
            0.0
        };
        let prev_hide = st.hide_progress;
        st.hide_progress += (target_hide - st.hide_progress) * k_hide;
        if (target_hide - st.hide_progress).abs() < 0.003 {
            st.hide_progress = target_hide;
        } else {
            still_active = true;
        }
        if (prev_hide > 0.85) != (st.hide_progress > 0.85) {
            need_reposition = true;
        }

        let target_tip = if st.hovering && st.hovered_idx >= 0 && st.hide_progress < 0.25 {
            1.0
        } else {
            0.0
        };
        st.tooltip_alpha += (target_tip - st.tooltip_alpha) * k_tip;
        if (target_tip - st.tooltip_alpha).abs() < 0.003 {
            st.tooltip_alpha = target_tip;
        } else {
            still_active = true;
        }

        // Prune finished bounces
        st.bounces.retain(|b| {
            let dur = if b.big { 1.25 } else { 0.32 };
            now.duration_since(b.start).as_secs_f32() < dur
        });
        if !st.bounces.is_empty() {
            still_active = true;
        }

        if !still_active {
            if st.anim_running {
                KillTimer(h, ANIM_TIMER);
                st.anim_running = false;
            }
            if !st.hovering {
                need_reposition = true;
            }
        }
    });

    render(h);
    if need_reposition || STATE.with_borrow(|st| st.hovering) {
        let m = compute_rest_metrics(h);
        if need_reposition {
            position_windows(h, &m);
        } else {
            // Keep interactive hitbox synchronized if pill expanded wider during magnification
            let mut wr = zeroed();
            GetWindowRect(h, &mut wr);
            let expand_diff = STATE.with_borrow(|st| {
                (st.last_pill_left - wr.left as f32).abs() > 4.0
                    || (st.last_pill_right - wr.right as f32).abs() > 4.0
            });
            if expand_diff {
                position_windows(h, &m);
            }
        }
    }
}

unsafe fn render(h: HWND) {
    let m = compute_rest_metrics(h);
    let (style, mag_boost) = MODEL.with_borrow(|s| (s.glass, s.mag_boost()));
    let active_hwnd = system::state().active_hwnd;
    let now = Instant::now();

    STATE.with_borrow_mut(|st| {
        // Compute per-icon magnification scales using smooth raised-cosine wave
        let n = m.n_total as usize;
        let rest_left = (m.screen_w as f32 - m.rest_w) * 0.5;
        let wave_radius = m.pitch * 2.65;
        let max_extra = mag_boost * st.hover_strength;

        // First compute raw scales from rest centers
        let mut scales = vec![1.0f32; n];
        let mut hovered_idx = -1i32;
        let mut best_dist = f32::MAX;

        for i in 0..n {
            let sep_off = if m.sep_after > 0 && (i as i32) >= m.sep_after {
                m.sep_w
            } else {
                0.0
            };
            let rest_cx = rest_left + m.pad_x + sep_off + (i as f32 + 0.5) * m.pitch;
            let dist = (st.smooth_mouse_x - rest_cx).abs();
            if st.hovering && dist < best_dist && dist < m.pitch * 0.85 {
                best_dist = dist;
                hovered_idx = i as i32;
            }
            if max_extra > 0.001 && dist < wave_radius {
                let t = dist / wave_radius;
                let curve = 0.5 * (1.0 + (std::f32::consts::PI * t).cos());
                scales[i] = 1.0 + max_extra * curve;
            }
        }

        // Prevent overflow beyond 96% of screen width during extreme magnification
        let raw_slots: Vec<f32> = scales.iter().map(|sc| m.base_icon * sc + m.gap).collect();
        let raw_content_w: f32 = raw_slots.iter().sum::<f32>() + m.sep_w;
        let max_allowed_w = m.screen_w as f32 * 0.96 - 2.0 * m.pad_x;
        let squeeze = if raw_content_w > max_allowed_w && raw_content_w > 1.0 {
            max_allowed_w / raw_content_w
        } else {
            1.0
        };

        let pill_w = (2.0 * m.pad_x + raw_content_w * squeeze).round();
        let pill_left = ((m.screen_w as f32 - pill_w) * 0.5).round();
        let pill_right = pill_left + pill_w;

        // Vertical position with smooth Auto-Hide slide offset
        // Strict Apple HIG concentricity: R_dock = R_icon (0.225 * base_icon) + padding
        let hide_offset = st.hide_progress * (m.pill_h + m.bottom_margin + 18.0 * m.dpi);
        let pill_bottom = (m.stage_h as f32 - m.bottom_margin + hide_offset).round();
        let pill_top = (pill_bottom - m.pill_h).round();
        let pill_radius = (m.base_icon * 0.225 + 8.0 * m.dpi).min(m.pill_h * 0.46);

        // Compute final per-icon layout + bounce physics
        let mut items_layout: Vec<ComputedItem> = Vec::with_capacity(n);
        let mut cursor_x = pill_left + m.pad_x;
        let mut sep_x = None;

        MODEL.with_borrow(|s| {
            for i in 0..n {
                if m.sep_after > 0 && (i as i32) == m.sep_after {
                    sep_x = Some(cursor_x + m.sep_w * squeeze * 0.5);
                    cursor_x += m.sep_w * squeeze;
                }
                let slot_w = raw_slots[i] * squeeze;
                let mut size = m.base_icon * scales[i] * squeeze;
                if st.pressed_idx == i as i32 {
                    size *= 1.0 - 0.08 * st.press_amount;
                }

                // Compute parabolic macOS bounce offset
                let mut bounce_y = 0.0f32;
                if let Some(item) = s.items.get(i) {
                    if let Some(b) = st.bounces.iter().find(|b| b.key == item.path) {
                        let t = now.duration_since(b.start).as_secs_f32();
                        if b.big {
                            let env = (-t * 1.35).exp();
                            let hop = (t * std::f32::consts::PI * 2.4).sin().abs();
                            bounce_y = hop * env * (m.base_icon * 0.55);
                        } else {
                            let p = (t / 0.30).clamp(0.0, 1.0);
                            bounce_y = (p * std::f32::consts::PI).sin() * (m.base_icon * 0.18);
                        }
                    }
                }

                let cx = cursor_x + slot_w * 0.5;
                items_layout.push(ComputedItem {
                    center_x: cx,
                    size,
                    scale: scales[i],
                    bounce_y,
                });
                cursor_x += slot_w;
            }
        });

        st.hovered_idx = if st.hovering { hovered_idx } else { -1 };
        st.last_centers = items_layout
            .iter()
            .map(|c| (c.center_x, c.size + m.gap))
            .collect();
        st.last_pill_left = pill_left;
        st.last_pill_right = pill_right;
        st.last_pill_top = pill_top;

        // Update floating tooltip label & smoothly follow hovered icon
        if st.hovered_idx >= 0 {
            let label = MODEL.with_borrow(|s| {
                let idx = st.hovered_idx as usize;
                if let Some(it) = s.items.get(idx) {
                    it.name.clone()
                } else if idx == s.items.len() {
                    "アプリ ランチャー".into()
                } else {
                    "MacUI 設定".into()
                }
            });
            if st.tooltip_label != label {
                st.tooltip_label = label;
                let tw = measure_text_width(&st.tooltip_label, 12.5 * m.dpi, TextWeight::Medium);
                st.tooltip_w = (tw + 24.0 * m.dpi).clamp(54.0 * m.dpi, 280.0 * m.dpi);
            }
            if let Some(it) = items_layout.get(st.hovered_idx as usize) {
                if st.tooltip_alpha < 0.06 {
                    st.tooltip_x = it.center_x;
                } else {
                    st.tooltip_x += (it.center_x - st.tooltip_x) * 0.34;
                }
            }
        }

        let vis_opacity = (1.0 - st.hide_progress * 1.15).clamp(0.0, 1.0);
        let tip_h = (28.0 * m.dpi).round();
        let tip_w = st.tooltip_w.round();
        let hovered_top = items_layout
            .get(st.hovered_idx.max(0) as usize)
            .map(|it| pill_bottom - 9.0 * m.dpi - it.size - it.bounce_y)
            .unwrap_or(pill_top);
        let tip_bottom = (hovered_top.min(pill_top) - 10.0 * m.dpi).max(tip_h + 4.0);
        let tip_top = tip_bottom - tip_h;
        let min_tip_l = m.stage_left as f32 + 8.0 * m.dpi;
        let max_tip_l = ((m.stage_left + m.stage_w) as f32 - tip_w - 8.0 * m.dpi).max(min_tip_l);
        let tip_left = (st.tooltip_x - tip_w * 0.5).clamp(min_tip_l, max_tip_l);
        let ox = m.stage_left as f32;
        let local_pill_l = pill_left - ox;
        let local_pill_r = pill_right - ox;
        let local_tip_l = tip_left - ox;

        let shimmer_info = if st.shimmer_alpha > 0.01 {
            Some((
                st.smooth_mouse_x - ox,
                pill_top + m.pill_h * 0.22,
                st.shimmer_alpha,
            ))
        } else {
            None
        };
        let tip_alpha = st.tooltip_alpha * vis_opacity;
        let tip_label = st.tooltip_label.clone();
        let stage_w = m.stage_w;
        let stage_h = m.stage_h;
        let dpi = m.dpi;

        let Some(surf) = st.surface.as_mut() else {
            return;
        };

        // Sync GPU Dual-Zone Optical Lensing backdrops in exact lockstep with Direct2D drawing!
        surf.main_backdrop.set_rect(
            local_pill_l,
            pill_top,
            pill_w,
            m.pill_h,
            pill_radius,
            vis_opacity,
        );
        if let Some((tip_bg, _)) = surf.extra_backdrops.first() {
            if tip_alpha > 0.01 && !tip_label.is_empty() {
                tip_bg.set_rect(local_tip_l, tip_top, tip_w, tip_h, tip_h * 0.5, tip_alpha);
            } else {
                tip_bg.set_rect(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            }
        }

        // When auto-hidden completely off-screen, clear D2D surface ONCE and skip all GPU draws!
        if vis_opacity <= 0.005 {
            if !st.hidden_cleared {
                surf.draw(stage_w, stage_h, |_, _, _| {});
                st.hidden_cleared = true;
            }
            return;
        }
        st.hidden_cleared = false;

        let sys_snap = system::state();
        let dock_tone = sys_snap.dock_tone;
        let is_light = dock_tone.is_light(style);

        surf.draw(stage_w, stage_h, |ctx: &ID2D1DeviceContext, _, _| {
            // 1. Seamless Liquid Glass Dock Pill (live chromatic backdrop + smooth shadow + single Fresnel rim)
            draw_liquid_glass_pill(
                ctx,
                d2d_rect(local_pill_l, pill_top, local_pill_r, pill_bottom),
                pill_radius,
                style,
                dock_tone,
                false,
                shimmer_info,
                true,
                vis_opacity,
            );

            // 2. Etched translucent macOS vertical glass separator before Settings/Custom section
            if let Some(sx) = sep_x {
                let lsx = sx - ox;
                let top_y = pill_top + m.pill_h * 0.22;
                let bot_y = pill_bottom - m.pill_h * 0.22;
                let (sep_dark, sep_light) = if is_light { (0.24, 0.45) } else { (0.16, 0.34) };
                if let Some(b) = solid(ctx, rgba(0.0, 0.0, 0.0, sep_dark * vis_opacity)) {
                    ctx.FillRectangle(&d2d_rect(lsx - 1.0, top_y, lsx, bot_y), &b);
                }
                if let Some(b) = solid(ctx, rgba(1.0, 1.0, 1.0, sep_light * vis_opacity)) {
                    ctx.FillRectangle(&d2d_rect(lsx, top_y, lsx + 1.0, bot_y), &b);
                }
            }

            // 3. Draw App Icons + Soft Liquid Hover Halo + Contact Shadow + Glowing Running Dots
            let icon_baseline = pill_bottom - 9.0 * dpi;
            let cur_hovered = st.hovered_idx;
            let hov_alpha = st.hover_strength * vis_opacity;
            MODEL.with_borrow_mut(|s| {
                for (i, layout) in items_layout.iter().enumerate() {
                    let cx = layout.center_x - ox;
                    let sz = layout.size;
                    let lift = (layout.scale - 1.0) * m.base_icon * 0.18 + layout.bounce_y;
                    let bottom_y = icon_baseline - lift;
                    let top_y = bottom_y - sz;
                    let left_x = cx - sz * 0.5;
                    let dest = d2d_rect(left_x, top_y, left_x + sz, bottom_y);

                    // Subtle Liquid Glass radial halo behind the hovered icon
                    if cur_hovered == i as i32 && hov_alpha > 0.02 {
                        let hr = sz * 0.62;
                        let hy = (top_y + bottom_y) * 0.5;
                        if let Some(halo) = radial_grad(
                            ctx,
                            pt(cx, hy),
                            hr,
                            hr,
                            &[
                                D2D1_GRADIENT_STOP {
                                    position: 0.0,
                                    color: rgba(1.0, 1.0, 1.0, 0.16 * hov_alpha),
                                },
                                D2D1_GRADIENT_STOP {
                                    position: 1.0,
                                    color: rgba(1.0, 1.0, 1.0, 0.0),
                                },
                            ],
                        ) {
                            ctx.FillEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(cx, hy),
                                    radiusX: hr,
                                    radiusY: hr,
                                },
                                &halo,
                            );
                        }
                    }

                    // Soft grounded shadow onto the glass surface beneath each icon
                    let shadow_r_x = sz * 0.36;
                    let shadow_r_y = (sz * 0.075).max(2.2 * dpi);
                    let shadow_a = (0.28 / (1.0 + lift * 0.04)) * vis_opacity;
                    if let Some(sh) = radial_grad(
                        ctx,
                        pt(cx, icon_baseline + 0.5 * dpi),
                        shadow_r_x,
                        shadow_r_y,
                        &[
                            D2D1_GRADIENT_STOP {
                                position: 0.0,
                                color: rgba(0.0, 0.0, 0.0, shadow_a),
                            },
                            D2D1_GRADIENT_STOP {
                                position: 1.0,
                                color: rgba(0.0, 0.0, 0.0, 0.0),
                            },
                        ],
                    ) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(cx, icon_baseline + 0.5 * dpi),
                                radiusX: shadow_r_x,
                                radiusY: shadow_r_y,
                            },
                            &sh,
                        );
                    }

                    if let Some(item) = s.items.get_mut(i) {
                        if item.d2d_bitmap.is_none() {
                            if let Some(raw) = &item.raw_icon {
                                item.d2d_bitmap = raw.to_d2d(ctx);
                            }
                        }
                        if let Some(bmp) = &item.d2d_bitmap {
                            let badge = item.raw_icon.as_ref().and_then(|r| r.badge.as_ref());
                            draw_icon_with_badge(ctx, bmp, badge, dest, vis_opacity);
                        } else {
                            // Clean glass fallback badge if icon unavailable
                            let rr = D2D1_ROUNDED_RECT {
                                rect: dest,
                                radiusX: sz * 0.22,
                                radiusY: sz * 0.22,
                            };
                            if let Some(b) = solid(ctx, rgba(0.55, 0.62, 0.72, 0.75 * vis_opacity))
                            {
                                ctx.FillRoundedRectangle(&rr, &b);
                            }
                        }

                        // Authentic macOS Running Indicator Dot (brighter & slightly wider pill for foreground app!)
                        if let Some(app) = &item.app {
                            if app.running {
                                let is_fg = active_hwnd != 0
                                    && app.windows.iter().any(|(hw, _)| *hw == active_hwnd);
                                let dot_y = pill_bottom - 4.2 * dpi;
                                let dot_r = (2.1 * dpi).max(2.0);
                                // Subtle ambient glow behind dot
                                if let Some(glow) = radial_grad(
                                    ctx,
                                    pt(cx, dot_y),
                                    dot_r * 3.0,
                                    dot_r * 2.2,
                                    &[
                                        D2D1_GRADIENT_STOP {
                                            position: 0.0,
                                            color: rgba(
                                                1.0,
                                                1.0,
                                                1.0,
                                                if is_fg { 0.50 } else { 0.24 } * vis_opacity,
                                            ),
                                        },
                                        D2D1_GRADIENT_STOP {
                                            position: 1.0,
                                            color: rgba(1.0, 1.0, 1.0, 0.0),
                                        },
                                    ],
                                ) {
                                    ctx.FillEllipse(
                                        &D2D1_ELLIPSE {
                                            point: pt(cx, dot_y),
                                            radiusX: dot_r * 3.0,
                                            radiusY: dot_r * 2.2,
                                        },
                                        &glow,
                                    );
                                }
                                // Subtle dark contrast backing so pearl dot stays readable over bright windows through clear glass
                                if let Some(sh) =
                                    solid(ctx, rgba(0.0, 0.0, 0.0, 0.32 * vis_opacity))
                                {
                                    if is_fg {
                                        let w_half = dot_r * 2.35;
                                        let rr = D2D1_ROUNDED_RECT {
                                            rect: d2d_rect(
                                                cx - w_half,
                                                dot_y - dot_r * 1.1 + 0.6,
                                                cx + w_half,
                                                dot_y + dot_r * 1.1 + 0.6,
                                            ),
                                            radiusX: dot_r * 1.15,
                                            radiusY: dot_r * 1.15,
                                        };
                                        ctx.FillRoundedRectangle(&rr, &sh);
                                    } else {
                                        ctx.FillEllipse(
                                            &D2D1_ELLIPSE {
                                                point: pt(cx, dot_y + 0.6),
                                                radiusX: dot_r * 1.28,
                                                radiusY: dot_r * 1.28,
                                            },
                                            &sh,
                                        );
                                    }
                                }
                                let dot_color = if is_light {
                                    if is_fg {
                                        rgba(0.08, 0.10, 0.14, 0.92 * vis_opacity)
                                    } else {
                                        rgba(0.10, 0.12, 0.16, 0.72 * vis_opacity)
                                    }
                                } else if is_fg {
                                    rgba(1.0, 1.0, 1.0, 0.98 * vis_opacity)
                                } else {
                                    rgba(0.95, 0.97, 1.0, 0.84 * vis_opacity)
                                };
                                if let Some(db) = solid(ctx, dot_color) {
                                    if is_fg {
                                        let w_half = dot_r * 2.1;
                                        let rr = D2D1_ROUNDED_RECT {
                                            rect: d2d_rect(
                                                cx - w_half,
                                                dot_y - dot_r * 0.85,
                                                cx + w_half,
                                                dot_y + dot_r * 0.85,
                                            ),
                                            radiusX: dot_r,
                                            radiusY: dot_r,
                                        };
                                        ctx.FillRoundedRectangle(&rr, &db);
                                    } else {
                                        ctx.FillEllipse(
                                            &D2D1_ELLIPSE {
                                                point: pt(cx, dot_y),
                                                radiusX: dot_r,
                                                radiusY: dot_r,
                                            },
                                            &db,
                                        );
                                    }
                                }
                            }
                        }
                    } else if i == s.items.len() {
                        // Authentic macOS Launchpad / App Launcher 3x3 Squircle Icon
                        draw_dock_launcher_icon(ctx, cx, top_y + sz * 0.5, sz, vis_opacity);
                    } else {
                        // Sleek macOS-style System Settings Squircle Icon
                        draw_dock_settings_icon(ctx, cx, top_y + sz * 0.5, sz, vis_opacity);
                    }
                }
            });

            // 4. Floating Liquid Glass Tooltip Capsule above hovered icon
            if tip_alpha > 0.01 && !tip_label.is_empty() {
                let tip_rect = d2d_rect(local_tip_l, tip_top, local_tip_l + tip_w, tip_bottom);
                draw_liquid_glass_pill(
                    ctx,
                    tip_rect,
                    tip_h * 0.5,
                    style,
                    dock_tone,
                    true,
                    None,
                    true,
                    tip_alpha,
                );
                let tip_fg = if is_light {
                    rgba(0.06, 0.08, 0.12, 0.92 * tip_alpha)
                } else {
                    rgba(1.0, 1.0, 1.0, 0.96 * tip_alpha)
                };
                draw_text(
                    ctx,
                    &tip_label,
                    d2d_rect(
                        local_tip_l + 8.0 * dpi,
                        tip_top,
                        local_tip_l + tip_w - 8.0 * dpi,
                        tip_bottom,
                    ),
                    12.5 * dpi,
                    TextWeight::Medium,
                    tip_fg,
                    TextAlign::Center,
                    !is_light,
                );
            }
        });
    });
}
