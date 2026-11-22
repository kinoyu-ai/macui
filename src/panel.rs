use super::*;
use crate::glass::{
    d2d_rect, draw_back_chevron, draw_cc_quick_icon, draw_liquid_glass_pill, draw_lock_icon,
    draw_text, draw_volume_icon, draw_wifi_icon, linear_grad, pt, rgba, solid, AmbientTone,
    GlassStyle, GlassSurface, SurfaceRole, TextAlign, TextWeight,
};
use windows::Win32::Graphics::Direct2D::{
    Common::{D2D1_GRADIENT_STOP, D2D_RECT_F},
    D2D1_ELLIPSE, D2D1_ROUNDED_RECT,
};
use windows_sys::Win32::System::Shutdown::LockWorkStation;

#[derive(Clone)]
enum Action {
    Setting(i32),
    Window(usize),
    Close(usize),
    Minimize(usize),
    ToggleMaximize(usize),
    Launch(String),
    Shortcut(u16),
    ShortcutShift(u16),
    LockScreen,
    OpenSettingsPanel,
    OpenControlCenter,
    OpenNetworkPanel(bool),
    OpenLauncher,
    RefreshNetwork,
    ConnectWifi(windows::core::GUID, String, bool),
    ToggleMute,
    Noop,
}

#[derive(Clone)]
struct MenuRow {
    label: String,
    shortcut: String,
    action: Action,
    checked: bool,
    separator: bool,
    destructive: bool,
}

impl MenuRow {
    fn item(label: impl Into<String>, action: Action) -> Self {
        Self {
            label: label.into(),
            shortcut: String::new(),
            action,
            checked: false,
            separator: false,
            destructive: false,
        }
    }
    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.shortcut = hint.into();
        self
    }
    fn checked(mut self, c: bool) -> Self {
        self.checked = c;
        self
    }
    fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
    fn sep() -> Self {
        Self {
            label: String::new(),
            shortcut: String::new(),
            action: Action::Noop,
            checked: false,
            separator: true,
            destructive: false,
        }
    }
}

#[derive(Clone)]
struct ClickZone {
    rect: D2D_RECT_F,
    action: Action,
    keep_open: bool,
}

enum PanelKind {
    Menu {
        title: Option<String>,
        rows: Vec<MenuRow>,
        selected: i32,
        offset: i32,
    },
    Settings,
    ControlCenter,
    Network {
        from_cc: bool,
    },
}

const PANEL_OUTSIDE_TIMER: usize = 45;

struct PanelState {
    hwnd: HWND,
    owner: HWND,
    tag: String,
    key: String,
    kind: PanelKind,
    surface: Option<GlassSurface>,
    tone: AmbientTone,
    opened_at: std::time::Instant,
    prev_mouse_down: bool,
    zones: Vec<ClickZone>,
    module_rects: Vec<D2D_RECT_F>,
    hovered_zone: Option<usize>,
    cursor_pos: Option<(f32, f32)>,
    tracking_leave: bool,
    slider_rect: Option<D2D_RECT_F>,
    dragging_volume: bool,
    width: i32,
    height: i32,
    margin: i32,
}

thread_local! {
    static DATA: RefCell<Option<PanelState>> = const { RefCell::new(None) };
    static HANDLE: Cell<HWND> = const { Cell::new(null_mut()) };
}

pub unsafe fn register(name: &str, proc: WNDPROC) {
    let mut c: WNDCLASSW = zeroed();
    let class = w(name);
    c.hInstance = GetModuleHandleW(null());
    c.lpszClassName = class.as_ptr();
    c.lpfnWndProc = proc;
    c.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
    RegisterClassW(&c);
}

pub fn is_open() -> bool {
    !HANDLE.get().is_null()
}

pub fn open_tag() -> String {
    DATA.with_borrow(|d| d.as_ref().map(|p| p.tag.clone()).unwrap_or_default())
}

pub unsafe fn close() {
    let h = HANDLE.get();
    if !h.is_null() {
        DestroyWindow(h);
    }
}

pub unsafe fn toggle_close_if(tag: &str) -> bool {
    if open_tag() == tag && is_open() {
        close();
        true
    } else {
        false
    }
}

pub unsafe fn repaint_if_open() {
    if is_open() {
        paint();
    }
}

pub unsafe fn apply_style() {
    let style = MODEL.with_borrow(|s| s.glass);
    DATA.with_borrow(|d| {
        if let Some(p) = d {
            if let Some(surf) = &p.surface {
                surf.set_style(style);
            }
        }
    });
    repaint_if_open();
}

fn dpi_scale(h: HWND) -> f32 {
    unsafe { GetDpiForWindow(h).max(96) as f32 / 96.0 }
}

unsafe fn create_panel_window(
    owner: HWND,
    tag: &str,
    key: String,
    kind: PanelKind,
    content_w: i32,
    content_h: i32,
    anchor_x: i32,
    anchor_y: i32,
    align_left: bool,
    align_right: bool,
    open_upward: bool,
) {
    close();
    launcher::close();
    register("MacUI.Panel", Some(proc));
    let dpi = dpi_scale(owner);
    let margin = (16.0 * dpi).round() as i32;
    let total_w = content_w + margin * 2;
    let total_h = content_h + margin * 2;

    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);

    let cx = if align_left {
        anchor_x
    } else if align_right {
        anchor_x - content_w
    } else {
        anchor_x - content_w / 2
    };
    let clamped_cx = cx.clamp(8, (screen_w - content_w - 8).max(8));

    let bar_h = bar::height().max(26);
    let min_top = bar_h + (6.0 * dpi).round() as i32;
    let cy = if open_upward {
        (anchor_y - content_h - (10.0 * dpi).round() as i32).max(min_top)
    } else {
        anchor_y
            .min((screen_h - content_h - 12).max(min_top))
            .max(min_top)
    };

    let wx = clamped_cx - margin;
    let wy = cy - margin;

    let h = CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        w("MacUI.Panel").as_ptr(),
        w("MacUI パネル").as_ptr(),
        WS_POPUP,
        wx,
        wy,
        total_w,
        total_h,
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if h.is_null() {
        return;
    }

    let style = MODEL.with_borrow(|s| s.glass);
    // Sample exact background color & brightness directly behind where this panel will appear!
    let tone = system::sample_screen_tone(clamped_cx, cy, content_w, content_h);
    // Apple HIG "Avoid Glass on Glass": Control Center (4 modules) and Network (3 modules)
    // spawn independent floating Liquid Glass modules instead of nesting glass inside glass.
    let extra_roles: &[SurfaceRole] = match kind {
        PanelKind::ControlCenter => &[SurfaceRole::Panel; 4],
        PanelKind::Network { .. } => &[SurfaceRole::Panel; 3],
        _ => &[],
    };
    let surf = GlassSurface::new(h, SurfaceRole::Panel, extra_roles, style);

    DATA.with_borrow_mut(|d| {
        *d = Some(PanelState {
            hwnd: h,
            owner,
            tag: tag.to_owned(),
            key,
            kind,
            surface: surf,
            tone,
            opened_at: std::time::Instant::now(),
            prev_mouse_down: (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0,
            zones: Vec::new(),
            module_rects: Vec::new(),
            hovered_zone: None,
            cursor_pos: None,
            tracking_leave: false,
            slider_rect: None,
            dragging_volume: false,
            width: total_w,
            height: total_h,
            margin,
        });
    });
    HANDLE.set(h);
    paint();
    ShowWindow(h, SW_SHOW);
    launcher::force_foreground(h);
    SetTimer(h, PANEL_OUTSIDE_TIMER, 75, None);
}

fn measure_menu_height(dpi: f32, title: &Option<String>, rows: &[MenuRow]) -> i32 {
    let mut h = 14.0 * dpi;
    if title.is_some() {
        h += 30.0 * dpi;
    }
    for row in rows {
        h += if row.separator {
            10.0 * dpi
        } else {
            30.0 * dpi
        };
    }
    let bar_h = unsafe { bar::height().max(26) } as f32;
    let max_h = unsafe { GetSystemMetrics(SM_CYSCREEN) as f32 - bar_h - 84.0 * dpi };
    h.min(max_h.max(120.0)).round() as i32
}

// ---------- Public Panel Openers ----------

pub unsafe fn open_settings(owner: HWND, anchor_x: i32, anchor_y: i32) {
    let dpi = dpi_scale(owner);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);
    let max_h = (screen_h - bar::height().max(26) - (24.0 * dpi).round() as i32).max(240);
    let w = (336.0 * dpi).round() as i32;
    let h = ((338.0 * dpi).round() as i32).min(max_h);
    let open_up = anchor_y > screen_h / 2;
    create_panel_window(
        owner,
        "settings",
        String::new(),
        PanelKind::Settings,
        w,
        h,
        anchor_x,
        anchor_y,
        false,
        false,
        open_up,
    );
}

pub unsafe fn open_control_center(owner: HWND, right_x: i32, top_y: i32) {
    system::refresh_all();
    let dpi = dpi_scale(owner);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);
    let max_h = (screen_h - bar::height().max(26) - (24.0 * dpi).round() as i32).max(200);
    let w = (324.0 * dpi).round() as i32;
    let h = ((240.0 * dpi).round() as i32).min(max_h);
    create_panel_window(
        owner,
        "cc",
        String::new(),
        PanelKind::ControlCenter,
        w,
        h,
        right_x,
        top_y,
        false,
        true,
        false,
    );
}

pub unsafe fn open_network(owner: HWND, right_x: i32, top_y: i32, from_cc: bool) {
    system::refresh_network();
    let dpi = dpi_scale(owner);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);
    let max_h = (screen_h - bar::height().max(26) - (24.0 * dpi).round() as i32).max(240);
    let w = (330.0 * dpi).round() as i32;
    let h = ((388.0 * dpi).round() as i32).min(max_h);
    create_panel_window(
        owner,
        "net",
        String::new(),
        PanelKind::Network { from_cc },
        w,
        h,
        right_x,
        top_y,
        false,
        true,
        false,
    );
}

pub unsafe fn open_dock_item(owner: HWND, index: i32, group: bool, anchor_x: i32, anchor_y: i32) {
    let mut title = None;
    let mut key = String::new();
    let mut rows = Vec::new();
    let active_hwnd = system::state().active_hwnd;

    MODEL.with_borrow(|s| {
        if let Some(item) = s.items.get(index as usize) {
            title = Some(item.name.clone());
            key = item.path.clone();
            if let Some(app) = &item.app {
                for (hw, wtitle) in &app.windows {
                    rows.push(
                        MenuRow::item(wtitle.clone(), Action::Window(*hw))
                            .checked(*hw == active_hwnd),
                    );
                }
                if !app.windows.is_empty() {
                    rows.push(MenuRow::sep());
                }
                if !group && app.windows.len() == 1 {
                    rows.push(
                        MenuRow::item("ウィンドウを閉じる", Action::Close(app.windows[0].0))
                            .destructive(),
                    );
                }
            }
            rows.push(MenuRow::item(
                "新しいウィンドウを開く",
                Action::Launch(item.path.clone()),
            ));
            if item.custom {
                rows.push(MenuRow::sep());
                rows.push(MenuRow::item("Dock から外す", Action::Setting(2)).destructive());
            }
        }
    });

    if rows.is_empty() {
        open_settings(owner, anchor_x, anchor_y);
        return;
    }

    let dpi = dpi_scale(owner);
    let w = (276.0 * dpi).round() as i32;
    let h = measure_menu_height(dpi, &title, &rows);
    create_panel_window(
        owner,
        "dock_item",
        key,
        PanelKind::Menu {
            title,
            rows,
            selected: -1,
            offset: 0,
        },
        w,
        h,
        anchor_x,
        anchor_y,
        false,
        false,
        true,
    );
}

pub unsafe fn open_apple_menu(owner: HWND, left_x: i32, top_y: i32) {
    let rows = vec![
        MenuRow::item(
            "この PC について",
            Action::Launch("ms-settings:about".into()),
        ),
        MenuRow::sep(),
        MenuRow::item("システム設定…", Action::Launch("ms-settings:".into())),
        MenuRow::item("MacUI 設定…", Action::OpenSettingsPanel),
        MenuRow::item("アプリ ランチャー…", Action::OpenLauncher).with_hint("Ctrl+Alt+Space"),
        MenuRow::sep(),
        MenuRow::item("タスク マネージャー", Action::Launch("taskmgr.exe".into()))
            .with_hint("Ctrl+Shift+Esc"),
        MenuRow::item("スタート メニュー", Action::Shortcut(VK_LWIN)).with_hint("Win"),
        MenuRow::item("デスクトップを表示", Action::Shortcut(0x44)).with_hint("Win+D"),
        MenuRow::sep(),
        MenuRow::item("画面をロック", Action::LockScreen).with_hint("Win+L"),
        MenuRow::sep(),
        MenuRow::item("MacUI を終了して元に戻す", Action::Setting(99))
            .with_hint("Ctrl+Alt+Q")
            .destructive(),
    ];
    let dpi = dpi_scale(owner);
    let w = (268.0 * dpi).round() as i32;
    let h = measure_menu_height(dpi, &None, &rows);
    create_panel_window(
        owner,
        "apple",
        String::new(),
        PanelKind::Menu {
            title: None,
            rows,
            selected: -1,
            offset: 0,
        },
        w,
        h,
        left_x,
        top_y,
        true,
        false,
        false,
    );
}

pub unsafe fn open_active_app_menu(owner: HWND, left_x: i32, top_y: i32) {
    let sys = system::state();
    let hw = sys.active_hwnd;
    let mut rows = vec![
        MenuRow::item(format!("{} を最小化", sys.active_app), Action::Minimize(hw)),
        MenuRow::item("最大化 / 元のサイズ", Action::ToggleMaximize(hw)),
        MenuRow::sep(),
        MenuRow::item("タスク マネージャー", Action::Launch("taskmgr.exe".into())),
    ];
    if hw != 0 {
        rows.push(MenuRow::sep());
        rows.push(
            MenuRow::item(format!("{} を閉じる", sys.active_app), Action::Close(hw))
                .with_hint("Alt+F4")
                .destructive(),
        );
    }
    let dpi = dpi_scale(owner);
    let w = (240.0 * dpi).round() as i32;
    let h = measure_menu_height(dpi, &None, &rows);
    create_panel_window(
        owner,
        "app",
        String::new(),
        PanelKind::Menu {
            title: None,
            rows,
            selected: -1,
            offset: 0,
        },
        w,
        h,
        left_x,
        top_y,
        true,
        false,
        false,
    );
}

pub unsafe fn open_file_menu(owner: HWND, left_x: i32, top_y: i32) {
    let rows = vec![
        MenuRow::item("新しいエクスプローラー", Action::Shortcut(0x45)).with_hint("Win+E"),
        MenuRow::item("ファイル名を指定して実行…", Action::Shortcut(0x52)).with_hint("Win+R"),
        MenuRow::sep(),
        MenuRow::item("スクリーンショットを撮る", Action::ShortcutShift(0x53))
            .with_hint("Win+Shift+S"),
        MenuRow::item("クリップボード履歴", Action::Shortcut(0x56)).with_hint("Win+V"),
    ];
    let dpi = dpi_scale(owner);
    let w = (264.0 * dpi).round() as i32;
    let h = measure_menu_height(dpi, &None, &rows);
    create_panel_window(
        owner,
        "file",
        String::new(),
        PanelKind::Menu {
            title: None,
            rows,
            selected: -1,
            offset: 0,
        },
        w,
        h,
        left_x,
        top_y,
        true,
        false,
        false,
    );
}

pub unsafe fn open_view_menu(owner: HWND, left_x: i32, top_y: i32) {
    // Use Windows shell known folders so OneDrive-redirected or custom-drive Desktop/Downloads/Documents
    // ALWAYS open the user's real folder on any PC!
    let rows = vec![
        MenuRow::item(
            "ホーム フォルダー",
            Action::Launch("shell:UsersFilesFolder".into()),
        ),
        MenuRow::item("デスクトップ", Action::Launch("shell:Desktop".into())),
        MenuRow::item("ダウンロード", Action::Launch("shell:Downloads".into())),
        MenuRow::item("ドキュメント", Action::Launch("shell:Personal".into())),
        MenuRow::sep(),
        MenuRow::item(
            "PC (コンピューター)",
            Action::Launch("shell:MyComputerFolder".into()),
        ),
        MenuRow::item("ゴミ箱", Action::Launch("shell:RecycleBinFolder".into())),
    ];
    let dpi = dpi_scale(owner);
    let w = (232.0 * dpi).round() as i32;
    let h = measure_menu_height(dpi, &None, &rows);
    create_panel_window(
        owner,
        "view",
        String::new(),
        PanelKind::Menu {
            title: None,
            rows,
            selected: -1,
            offset: 0,
        },
        w,
        h,
        left_x,
        top_y,
        true,
        false,
        false,
    );
}

pub unsafe fn open_window_menu(owner: HWND, left_x: i32, top_y: i32) {
    let hw = system::state().active_hwnd;
    let rows = vec![
        MenuRow::item("タスク ビュー (Mission Control)", Action::Shortcut(VK_TAB))
            .with_hint("Win+Tab"),
        MenuRow::item("スナップ レイアウト", Action::Shortcut(0x5A)).with_hint("Win+Z"),
        MenuRow::sep(),
        MenuRow::item("現在のウィンドウを最小化", Action::Minimize(hw)),
        MenuRow::item("最大化 / 元のサイズ", Action::ToggleMaximize(hw)),
        MenuRow::sep(),
        MenuRow::item("デスクトップを表示", Action::Shortcut(0x44)).with_hint("Win+D"),
    ];
    let dpi = dpi_scale(owner);
    let w = (276.0 * dpi).round() as i32;
    let h = measure_menu_height(dpi, &None, &rows);
    create_panel_window(
        owner,
        "window",
        String::new(),
        PanelKind::Menu {
            title: None,
            rows,
            selected: -1,
            offset: 0,
        },
        w,
        h,
        left_x,
        top_y,
        true,
        false,
        false,
    );
}

#[inline]
fn pt_in_rect(x: f32, y: f32, r: D2D_RECT_F) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

fn shimmer_for(
    cursor: Option<(f32, f32)>,
    rect: D2D_RECT_F,
    hovered: bool,
    dpi: f32,
) -> Option<(f32, f32, f32)> {
    let (cx, cy) = cursor?;
    let pad = 12.0 * dpi;
    if cx < rect.left - pad
        || cx > rect.right + pad
        || cy < rect.top - pad
        || cy > rect.bottom + pad
    {
        return None;
    }
    let inside = pt_in_rect(cx, cy, rect);
    let intensity = if hovered {
        0.86
    } else if inside {
        0.60
    } else {
        0.26
    };
    Some((
        cx.clamp(rect.left, rect.right),
        cy.clamp(rect.top, rect.bottom),
        intensity,
    ))
}

// ---------- Rendering ----------

unsafe fn paint() {
    let style = MODEL.with_borrow(|s| s.glass);
    let sys = system::state();
    let notif_str = bar::notification_count();

    DATA.with_borrow_mut(|slot| {
        let Some(p) = slot.as_mut() else { return };
        let dpi = dpi_scale(p.hwnd);
        let w = p.width;
        let h = p.height;
        let m = p.margin as f32;
        let cr = d2d_rect(m, m, w as f32 - m, h as f32 - m);

        let mut zones: Vec<ClickZone> = Vec::new();
        let mut module_rects: Vec<D2D_RECT_F> = Vec::new();
        let mut slider_rect: Option<D2D_RECT_F> = None;
        let hovered_zone = p.hovered_zone;
        let cursor_pos = p.cursor_pos;

        let Some(surf) = p.surface.as_mut() else {
            return;
        };

        // Pre-compute floating Control Center module layout (Apple HIG: Avoid Glass on Glass)
        let cc_gap = 10.0 * dpi;
        let cc_w = cr.right - cr.left;
        let cc_half_w = (cc_w - cc_gap) * 0.5;
        let cc_top_h = 74.0 * dpi;
        let cc_snd_h = 78.0 * dpi;
        let cc_qa_h = 68.0 * dpi;

        let cc_wifi = d2d_rect(cr.left, cr.top, cr.left + cc_half_w, cr.top + cc_top_h);
        let cc_notif = d2d_rect(
            cr.left + cc_half_w + cc_gap,
            cr.top,
            cr.right,
            cr.top + cc_top_h,
        );
        let y_snd = cr.top + cc_top_h + cc_gap;
        let cc_snd = d2d_rect(cr.left, y_snd, cr.right, y_snd + cc_snd_h);
        let y_qa = y_snd + cc_snd_h + cc_gap;
        let cc_qa = d2d_rect(cr.left, y_qa, cr.right, y_qa + cc_qa_h);

        let cc_modules: [(D2D_RECT_F, f32); 4] = [
            (cc_wifi, 22.0 * dpi),
            (cc_notif, 22.0 * dpi),
            (cc_snd, 22.0 * dpi),
            (cc_qa, 22.0 * dpi),
        ];

        // Pre-compute 3 floating Liquid Glass modules for the native Network panel (ZERO Glass-on-Glass!)
        let net_hdr_h = 86.0 * dpi;
        let net_list_h = 232.0 * dpi;
        let net_foot_h = 50.0 * dpi;
        let net_hdr = d2d_rect(cr.left, cr.top, cr.right, cr.top + net_hdr_h);
        let y_nlist = cr.top + net_hdr_h + cc_gap;
        let net_list = d2d_rect(cr.left, y_nlist, cr.right, y_nlist + net_list_h);
        let y_nfoot = y_nlist + net_list_h + cc_gap;
        let net_foot = d2d_rect(cr.left, y_nfoot, cr.right, y_nfoot + net_foot_h);
        let net_modules: [(D2D_RECT_F, f32); 3] = [
            (net_hdr, 22.0 * dpi),
            (net_list, 22.0 * dpi),
            (net_foot, 18.0 * dpi),
        ];

        // Configure Composition backdrops per panel kind
        let outer_radius = match p.kind {
            PanelKind::Menu { .. } => 16.0 * dpi,
            PanelKind::Settings => 24.0 * dpi,
            PanelKind::ControlCenter | PanelKind::Network { .. } => 22.0 * dpi,
        };

        match p.kind {
            PanelKind::ControlCenter => {
                // Hide single outer container glass — render 5 separate floating Liquid Glass modules!
                surf.main_backdrop.set_rect(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
                for (i, (mr, mr_rad)) in cc_modules.iter().enumerate() {
                    module_rects.push(*mr);
                    if let Some((bd, _)) = surf.extra_backdrops.get(i) {
                        bd.set_rect(
                            mr.left,
                            mr.top,
                            mr.right - mr.left,
                            mr.bottom - mr.top,
                            *mr_rad,
                            1.0,
                        );
                    }
                }
            }
            PanelKind::Network { .. } => {
                surf.main_backdrop.set_rect(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
                for (i, (mr, mr_rad)) in net_modules.iter().enumerate() {
                    module_rects.push(*mr);
                    if let Some((bd, _)) = surf.extra_backdrops.get(i) {
                        bd.set_rect(
                            mr.left,
                            mr.top,
                            mr.right - mr.left,
                            mr.bottom - mr.top,
                            *mr_rad,
                            1.0,
                        );
                    }
                }
            }
            PanelKind::Menu { .. } | PanelKind::Settings => {
                surf.main_backdrop.set_rect(
                    cr.left,
                    cr.top,
                    cr.right - cr.left,
                    cr.bottom - cr.top,
                    outer_radius,
                    1.0,
                );
                for (bd, _) in &surf.extra_backdrops {
                    bd.set_rect(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
                }
                module_rects.push(cr);
            }
        }

        let kind_ref = &p.kind;
        let tone = p.tone;
        let is_light = tone.is_light(style);
        let txt_sh = !is_light;
        let ink_primary = if is_light {
            rgba(0.06, 0.08, 0.12, 0.94)
        } else {
            rgba(1.0, 1.0, 1.0, 0.98)
        };
        let ink_secondary = if is_light {
            rgba(0.10, 0.12, 0.18, 0.74)
        } else {
            rgba(1.0, 1.0, 1.0, 0.80)
        };
        let ink_muted = if is_light {
            rgba(0.12, 0.14, 0.20, 0.56)
        } else {
            rgba(1.0, 1.0, 1.0, 0.56)
        };
        let ink_destructive = if is_light {
            rgba(0.84, 0.14, 0.14, 0.96)
        } else {
            rgba(1.0, 0.48, 0.45, 0.98)
        };
        let sep_col = if is_light {
            rgba(0.0, 0.0, 0.0, 0.12)
        } else {
            rgba(1.0, 1.0, 1.0, 0.16)
        };
        let track_bg = if is_light {
            rgba(0.0, 0.0, 0.0, 0.10)
        } else {
            rgba(0.0, 0.0, 0.0, 0.24)
        };
        let track_bd = if is_light {
            rgba(0.0, 0.0, 0.0, 0.12)
        } else {
            rgba(1.0, 1.0, 1.0, 0.16)
        };
        let hover_fill = if is_light {
            rgba(0.0, 0.0, 0.0, 0.08)
        } else {
            rgba(1.0, 1.0, 1.0, 0.12)
        };
        let active_top = if is_light {
            rgba(1.0, 1.0, 1.0, 0.88)
        } else {
            rgba(1.0, 1.0, 1.0, 0.34)
        };
        let active_bot = if is_light {
            rgba(0.95, 0.97, 1.0, 0.72)
        } else {
            rgba(0.92, 0.96, 1.0, 0.22)
        };
        let active_rim = if is_light {
            rgba(0.0, 0.0, 0.0, 0.14)
        } else {
            rgba(1.0, 1.0, 1.0, 0.46)
        };

        surf.draw(w, h, |ctx, _, _| {
            match kind_ref {
                PanelKind::Menu {
                    title,
                    rows,
                    selected,
                    offset,
                } => {
                    let shimmer = shimmer_for(cursor_pos, cr, false, dpi);
                    draw_liquid_glass_pill(
                        ctx,
                        cr,
                        outer_radius,
                        style,
                        tone,
                        true,
                        shimmer,
                        true,
                        1.0,
                    );

                    let mut y = cr.top + 7.0 * dpi;
                    let pad_x = 12.0 * dpi;
                    let row_inset = 6.0 * dpi;
                    // Strict HIG concentricity: outer 16dp - inset 6dp = 10dp row radius
                    let row_radius = (outer_radius - row_inset).max(6.0 * dpi);

                    if let Some(t) = title {
                        draw_text(
                            ctx,
                            t,
                            d2d_rect(
                                cr.left + pad_x + 4.0 * dpi,
                                y,
                                cr.right - pad_x,
                                y + 26.0 * dpi,
                            ),
                            13.0 * dpi,
                            TextWeight::SemiBold,
                            ink_secondary,
                            TextAlign::Left,
                            txt_sh,
                        );
                        y += 28.0 * dpi;
                        if let Some(b) = solid(ctx, sep_col) {
                            ctx.FillRectangle(
                                &d2d_rect(
                                    cr.left + 12.0 * dpi,
                                    y - 2.0,
                                    cr.right - 12.0 * dpi,
                                    y - 1.0,
                                ),
                                &b,
                            );
                        }
                    }

                    for (i, row) in rows.iter().enumerate().skip(*offset as usize) {
                        if row.separator {
                            let sy = y + 5.0 * dpi;
                            if sy < cr.bottom - 8.0 * dpi {
                                if let Some(b) = solid(ctx, sep_col) {
                                    ctx.FillRectangle(
                                        &d2d_rect(
                                            cr.left + 14.0 * dpi,
                                            sy,
                                            cr.right - 14.0 * dpi,
                                            sy + 1.0,
                                        ),
                                        &b,
                                    );
                                }
                            }
                            y += 10.0 * dpi;
                            continue;
                        }
                        let rh = 30.0 * dpi;
                        if y + rh > cr.bottom - 5.0 * dpi {
                            break;
                        }
                        let r = d2d_rect(cr.left + row_inset, y, cr.right - row_inset, y + rh);
                        let is_sel = *selected == i as i32;
                        if is_sel {
                            // Apple macOS Tahoe Liquid Glass translucent highlight pill
                            let sel_rr = D2D1_ROUNDED_RECT {
                                rect: r,
                                radiusX: row_radius,
                                radiusY: row_radius,
                            };
                            if row.destructive {
                                if let Some(b) = solid(ctx, rgba(0.92, 0.24, 0.22, 0.72)) {
                                    ctx.FillRoundedRectangle(&sel_rr, &b);
                                }
                            } else if let Some(g) = linear_grad(
                                ctx,
                                pt(r.left, r.top),
                                pt(r.left, r.bottom),
                                &[
                                    D2D1_GRADIENT_STOP {
                                        position: 0.0,
                                        color: if is_light {
                                            rgba(0.0, 0.0, 0.0, 0.11)
                                        } else {
                                            rgba(1.0, 1.0, 1.0, 0.30)
                                        },
                                    },
                                    D2D1_GRADIENT_STOP {
                                        position: 1.0,
                                        color: if is_light {
                                            rgba(0.0, 0.0, 0.0, 0.08)
                                        } else {
                                            rgba(0.92, 0.96, 1.0, 0.18)
                                        },
                                    },
                                ],
                            ) {
                                ctx.FillRoundedRectangle(&sel_rr, &g);
                            }
                            let sel_rim = if row.destructive {
                                rgba(1.0, 1.0, 1.0, 0.42)
                            } else if is_light {
                                rgba(0.0, 0.0, 0.0, 0.16)
                            } else {
                                rgba(1.0, 1.0, 1.0, 0.46)
                            };
                            if let Some(rim) = solid(ctx, sel_rim) {
                                ctx.DrawRoundedRectangle(&sel_rr, &rim, 0.9, None);
                            }
                        }

                        let text_col = if row.destructive {
                            if is_sel {
                                rgba(1.0, 1.0, 1.0, 0.98)
                            } else {
                                ink_destructive
                            }
                        } else {
                            ink_primary
                        };

                        let has_checks = rows.iter().any(|r| r.checked);
                        let left_inset = if has_checks { 24.0 * dpi } else { 12.0 * dpi };

                        if row.checked {
                            draw_text(
                                ctx,
                                "✓",
                                d2d_rect(r.left + 6.0 * dpi, r.top, r.left + 22.0 * dpi, r.bottom),
                                13.0 * dpi,
                                TextWeight::Bold,
                                text_col,
                                TextAlign::Center,
                                txt_sh,
                            );
                        }

                        let right_reserve = if row.shortcut.is_empty() {
                            10.0 * dpi
                        } else {
                            92.0 * dpi
                        };
                        draw_text(
                            ctx,
                            &row.label,
                            d2d_rect(
                                r.left + left_inset,
                                r.top,
                                r.right - right_reserve,
                                r.bottom,
                            ),
                            13.0 * dpi,
                            if is_sel {
                                TextWeight::Medium
                            } else {
                                TextWeight::Regular
                            },
                            text_col,
                            TextAlign::Left,
                            txt_sh || (row.destructive && is_sel),
                        );
                        if !row.shortcut.is_empty() {
                            draw_text(
                                ctx,
                                &row.shortcut,
                                d2d_rect(
                                    r.right - 90.0 * dpi,
                                    r.top,
                                    r.right - 10.0 * dpi,
                                    r.bottom,
                                ),
                                11.5 * dpi,
                                TextWeight::Regular,
                                if is_sel && row.destructive {
                                    rgba(1.0, 1.0, 1.0, 0.90)
                                } else if is_sel {
                                    ink_secondary
                                } else {
                                    ink_muted
                                },
                                TextAlign::Right,
                                txt_sh || (row.destructive && is_sel),
                            );
                        }
                        zones.push(ClickZone {
                            rect: r,
                            action: row.action.clone(),
                            keep_open: false,
                        });
                        y += rh;
                    }
                }

                PanelKind::Settings => {
                    let shimmer = shimmer_for(cursor_pos, cr, false, dpi);
                    draw_liquid_glass_pill(
                        ctx,
                        cr,
                        outer_radius,
                        style,
                        tone,
                        true,
                        shimmer,
                        true,
                        1.0,
                    );

                    let (cur_size, cur_glass, cur_mag, cur_auto, custom_count) =
                        MODEL.with_borrow(|s| {
                            (
                                s.size,
                                s.glass,
                                s.magnify,
                                s.autohide,
                                s.items.iter().filter(|i| i.custom).count(),
                            )
                        });
                    // Strict Apple HIG Concentricity:
                    // outer 24dp - pad 14dp = 10dp track/button radius;
                    // track 10dp - thumb inset 2.5dp = 7.5dp active thumb radius.
                    let pad = 14.0 * dpi;
                    let inner_r_rad = (outer_radius - pad).max(8.0 * dpi);
                    let thumb_inset = 2.5 * dpi;
                    let thumb_rad = (inner_r_rad - thumb_inset).max(6.0 * dpi);

                    let inner_l = cr.left + pad;
                    let inner_r = cr.right - pad;
                    let mut y = cr.top + pad;

                    // Header
                    draw_text(
                        ctx,
                        "MacUI 設定",
                        d2d_rect(inner_l, y, inner_r, y + 24.0 * dpi),
                        16.5 * dpi,
                        TextWeight::Bold,
                        ink_primary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    draw_text(
                        ctx,
                        "Liquid Glass",
                        d2d_rect(inner_l, y, inner_r, y + 24.0 * dpi),
                        11.5 * dpi,
                        TextWeight::Medium,
                        if is_light {
                            rgba(0.10, 0.36, 0.78, 0.90)
                        } else {
                            rgba(0.76, 0.90, 1.0, 0.90)
                        },
                        TextAlign::Right,
                        txt_sh,
                    );
                    y += 32.0 * dpi;

                    // Helper for macOS Tahoe concentric segmented glass control
                    let mut draw_segmented =
                        |label: &str, opts: &[(&str, i32, bool)], y_pos: &mut f32| {
                            draw_text(
                                ctx,
                                label,
                                d2d_rect(inner_l, *y_pos, inner_r, *y_pos + 18.0 * dpi),
                                12.0 * dpi,
                                TextWeight::Medium,
                                ink_secondary,
                                TextAlign::Left,
                                txt_sh,
                            );
                            *y_pos += 21.0 * dpi;
                            let seg_h = 30.0 * dpi;
                            let track = d2d_rect(inner_l, *y_pos, inner_r, *y_pos + seg_h);
                            if let Some(bg) = solid(ctx, track_bg) {
                                ctx.FillRoundedRectangle(
                                    &D2D1_ROUNDED_RECT {
                                        rect: track,
                                        radiusX: inner_r_rad,
                                        radiusY: inner_r_rad,
                                    },
                                    &bg,
                                );
                            }
                            if let Some(bd) = solid(ctx, track_bd) {
                                ctx.DrawRoundedRectangle(
                                    &D2D1_ROUNDED_RECT {
                                        rect: track,
                                        radiusX: inner_r_rad,
                                        radiusY: inner_r_rad,
                                    },
                                    &bd,
                                    0.9,
                                    None,
                                );
                            }
                            let n = opts.len() as f32;
                            let cell_w = (inner_r - inner_l - thumb_inset * 2.0) / n;
                            for (idx, (text, cmd_id, active)) in opts.iter().enumerate() {
                                let cl = inner_l + thumb_inset + idx as f32 * cell_w;
                                let cr_box = d2d_rect(
                                    cl,
                                    *y_pos + thumb_inset,
                                    cl + cell_w,
                                    *y_pos + seg_h - thumb_inset,
                                );
                                let z_idx = zones.len();
                                let is_hov = hovered_zone == Some(z_idx);
                                if *active {
                                    let rr = D2D1_ROUNDED_RECT {
                                        rect: cr_box,
                                        radiusX: thumb_rad,
                                        radiusY: thumb_rad,
                                    };
                                    if let Some(g) = linear_grad(
                                        ctx,
                                        pt(cr_box.left, cr_box.top),
                                        pt(cr_box.left, cr_box.bottom),
                                        &[
                                            D2D1_GRADIENT_STOP {
                                                position: 0.0,
                                                color: active_top,
                                            },
                                            D2D1_GRADIENT_STOP {
                                                position: 1.0,
                                                color: active_bot,
                                            },
                                        ],
                                    ) {
                                        ctx.FillRoundedRectangle(&rr, &g);
                                    }
                                    if let Some(rim) = solid(ctx, active_rim) {
                                        ctx.DrawRoundedRectangle(&rr, &rim, 0.9, None);
                                    }
                                } else if is_hov {
                                    if let Some(b) = solid(ctx, hover_fill) {
                                        ctx.FillRoundedRectangle(
                                            &D2D1_ROUNDED_RECT {
                                                rect: cr_box,
                                                radiusX: thumb_rad,
                                                radiusY: thumb_rad,
                                            },
                                            &b,
                                        );
                                    }
                                }
                                draw_text(
                                    ctx,
                                    text,
                                    cr_box,
                                    12.5 * dpi,
                                    if *active {
                                        TextWeight::SemiBold
                                    } else {
                                        TextWeight::Regular
                                    },
                                    if *active { ink_primary } else { ink_secondary },
                                    TextAlign::Center,
                                    txt_sh,
                                );
                                zones.push(ClickZone {
                                    rect: cr_box,
                                    action: Action::Setting(*cmd_id),
                                    keep_open: true,
                                });
                            }
                            *y_pos += seg_h + 10.0 * dpi;
                        };

                    draw_segmented(
                        "Dock サイズ",
                        &[
                            ("小", 10, cur_size == 46),
                            ("標準", 11, cur_size == 54),
                            ("大", 12, cur_size == 64),
                        ],
                        &mut y,
                    );
                    draw_segmented(
                        "Liquid Glass 質感",
                        &[
                            ("クリア", 20, cur_glass == GlassStyle::Clear),
                            ("標準", 21, cur_glass == GlassStyle::Standard),
                            ("ダーク", 22, cur_glass == GlassStyle::Dark),
                        ],
                        &mut y,
                    );
                    draw_segmented(
                        "ホバー拡大アニメーション",
                        &[
                            ("オフ", 30, cur_mag == 0),
                            ("弱", 31, cur_mag == 1),
                            ("標準", 32, cur_mag == 2),
                            ("強", 33, cur_mag == 3),
                        ],
                        &mut y,
                    );

                    // Auto-hide macOS Toggle Switch Row
                    let row_h = 32.0 * dpi;
                    let toggle_row = d2d_rect(inner_l, y, inner_r, y + row_h);
                    let tz_idx = zones.len();
                    if hovered_zone == Some(tz_idx) {
                        if let Some(b) = solid(ctx, hover_fill) {
                            ctx.FillRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: toggle_row,
                                    radiusX: inner_r_rad,
                                    radiusY: inner_r_rad,
                                },
                                &b,
                            );
                        }
                    }
                    draw_text(
                        ctx,
                        "Dock を自動的に隠す",
                        d2d_rect(inner_l + 4.0 * dpi, y, inner_r - 52.0 * dpi, y + row_h),
                        13.0 * dpi,
                        TextWeight::Regular,
                        ink_primary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    // Switch capsule
                    let sw_w = 40.0 * dpi;
                    let sw_h = 22.0 * dpi;
                    let sw_l = inner_r - sw_w - 2.0 * dpi;
                    let sw_t = y + (row_h - sw_h) * 0.5;
                    let sw_col = if cur_auto {
                        rgba(0.20, 0.78, 0.40, 0.94)
                    } else if is_light {
                        rgba(0.0, 0.0, 0.0, 0.18)
                    } else {
                        rgba(1.0, 1.0, 1.0, 0.22)
                    };
                    if let Some(sb) = solid(ctx, sw_col) {
                        ctx.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: d2d_rect(sw_l, sw_t, sw_l + sw_w, sw_t + sw_h),
                                radiusX: sw_h * 0.5,
                                radiusY: sw_h * 0.5,
                            },
                            &sb,
                        );
                    }
                    let knob_x = if cur_auto {
                        sw_l + sw_w - sw_h * 0.5
                    } else {
                        sw_l + sw_h * 0.5
                    };
                    if let Some(kb) = solid(ctx, rgba(1.0, 1.0, 1.0, 0.98)) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(knob_x, sw_t + sw_h * 0.5),
                                radiusX: sw_h * 0.39,
                                radiusY: sw_h * 0.39,
                            },
                            &kb,
                        );
                    }
                    zones.push(ClickZone {
                        rect: toggle_row,
                        action: Action::Setting(40),
                        keep_open: true,
                    });
                    y += row_h + 6.0 * dpi;

                    // Separator
                    if let Some(sep) = solid(ctx, sep_col) {
                        ctx.FillRectangle(&d2d_rect(inner_l, y, inner_r, y + 1.0), &sep);
                    }
                    y += 8.0 * dpi;

                    // Bottom Action Buttons (concentric radius = inner_r_rad)
                    let btn_h = 32.0 * dpi;
                    if custom_count < 6 {
                        let pin_r = d2d_rect(inner_l, y, inner_r, y + btn_h);
                        let pz = zones.len();
                        let bg = if hovered_zone == Some(pz) {
                            if is_light {
                                rgba(0.0, 0.0, 0.0, 0.12)
                            } else {
                                rgba(1.0, 1.0, 1.0, 0.22)
                            }
                        } else if is_light {
                            rgba(0.0, 0.0, 0.0, 0.06)
                        } else {
                            rgba(1.0, 1.0, 1.0, 0.12)
                        };
                        if let Some(b) = solid(ctx, bg) {
                            ctx.FillRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: pin_r,
                                    radiusX: inner_r_rad,
                                    radiusY: inner_r_rad,
                                },
                                &b,
                            );
                        }
                        if let Some(rim) = solid(ctx, track_bd) {
                            ctx.DrawRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: pin_r,
                                    radiusX: inner_r_rad,
                                    radiusY: inner_r_rad,
                                },
                                &rim,
                                0.85,
                                None,
                            );
                        }
                        draw_text(
                            ctx,
                            "＋  Dock にアプリやフォルダーを追加…",
                            pin_r,
                            12.5 * dpi,
                            TextWeight::Medium,
                            ink_primary,
                            TextAlign::Center,
                            txt_sh,
                        );
                        zones.push(ClickZone {
                            rect: pin_r,
                            action: Action::Setting(1),
                            keep_open: false,
                        });
                        y += btn_h + 7.0 * dpi;
                    }

                    let quit_r = d2d_rect(inner_l, y, inner_r, y + btn_h);
                    let qz = zones.len();
                    let q_hov = hovered_zone == Some(qz);
                    let qbg = if q_hov {
                        rgba(0.92, 0.24, 0.22, 0.80)
                    } else if is_light {
                        rgba(0.92, 0.24, 0.22, 0.16)
                    } else {
                        rgba(0.95, 0.28, 0.25, 0.28)
                    };
                    if let Some(b) = solid(ctx, qbg) {
                        ctx.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: quit_r,
                                radiusX: inner_r_rad,
                                radiusY: inner_r_rad,
                            },
                            &b,
                        );
                    }
                    if let Some(rim) = solid(ctx, rgba(0.95, 0.34, 0.30, 0.42)) {
                        ctx.DrawRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: quit_r,
                                radiusX: inner_r_rad,
                                radiusY: inner_r_rad,
                            },
                            &rim,
                            0.9,
                            None,
                        );
                    }
                    draw_text(
                        ctx,
                        "終了してタスクバーを戻す (Ctrl+Alt+Q)",
                        quit_r,
                        12.5 * dpi,
                        TextWeight::SemiBold,
                        if is_light && !q_hov {
                            ink_destructive
                        } else {
                            rgba(1.0, 0.96, 0.96, 0.98)
                        },
                        TextAlign::Center,
                        txt_sh || q_hov,
                    );
                    zones.push(ClickZone {
                        rect: quit_r,
                        action: Action::Setting(99),
                        keep_open: false,
                    });
                }

                PanelKind::ControlCenter => {
                    // Apple HIG macOS Tahoe Control Center:
                    // 4 independent floating Liquid Glass modules separated by transparent gaps (ZERO Glass-on-Glass!)

                    // Module 0: Floating Wi-Fi / Network Glass Module
                    let (wifi_card, wifi_rad) = cc_modules[0];
                    let wz = zones.len();
                    draw_liquid_glass_pill(
                        ctx,
                        wifi_card,
                        wifi_rad,
                        style,
                        tone,
                        true,
                        shimmer_for(cursor_pos, wifi_card, hovered_zone == Some(wz), dpi),
                        true,
                        1.0,
                    );
                    let badge_cx = wifi_card.left + 26.0 * dpi;
                    let badge_cy = wifi_card.top + cc_top_h * 0.5;
                    let net_on = sys.wifi_signal > 0;
                    if let Some(cb) = solid(
                        ctx,
                        if net_on {
                            rgba(0.14, 0.54, 1.0, 0.94)
                        } else if is_light {
                            rgba(0.0, 0.0, 0.0, 0.14)
                        } else {
                            rgba(1.0, 1.0, 1.0, 0.22)
                        },
                    ) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(badge_cx, badge_cy),
                                radiusX: 16.0 * dpi,
                                radiusY: 16.0 * dpi,
                            },
                            &cb,
                        );
                    }
                    draw_wifi_icon(
                        ctx,
                        badge_cx,
                        badge_cy,
                        15.0 * dpi,
                        sys.wifi_signal,
                        if net_on || !is_light {
                            rgba(1.0, 1.0, 1.0, 0.98)
                        } else {
                            ink_primary
                        },
                    );
                    let net_title = if !sys.net_title.is_empty() {
                        sys.net_title.as_str()
                    } else if sys.wifi_signal == 4 {
                        "イーサネット"
                    } else {
                        "Wi-Fi"
                    };
                    let net_sub = if !sys.net_detail.is_empty() {
                        sys.net_detail.clone()
                    } else if !sys.wifi_ssid.is_empty() {
                        sys.wifi_ssid.clone()
                    } else if net_on {
                        "接続済み".into()
                    } else {
                        "未接続".into()
                    };
                    draw_text(
                        ctx,
                        net_title,
                        d2d_rect(
                            wifi_card.left + 48.0 * dpi,
                            wifi_card.top + 15.0 * dpi,
                            wifi_card.right - 10.0 * dpi,
                            wifi_card.top + 37.0 * dpi,
                        ),
                        13.0 * dpi,
                        TextWeight::SemiBold,
                        ink_primary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    draw_text(
                        ctx,
                        &net_sub,
                        d2d_rect(
                            wifi_card.left + 48.0 * dpi,
                            wifi_card.top + 36.0 * dpi,
                            wifi_card.right - 10.0 * dpi,
                            wifi_card.top + 57.0 * dpi,
                        ),
                        11.5 * dpi,
                        TextWeight::Regular,
                        ink_secondary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    zones.push(ClickZone {
                        rect: wifi_card,
                        action: Action::OpenNetworkPanel(true),
                        keep_open: false,
                    });

                    // Module 1: Floating Notification Center Glass Module
                    let (notif_card, notif_rad) = cc_modules[1];
                    let nz = zones.len();
                    draw_liquid_glass_pill(
                        ctx,
                        notif_card,
                        notif_rad,
                        style,
                        tone,
                        true,
                        shimmer_for(cursor_pos, notif_card, hovered_zone == Some(nz), dpi),
                        true,
                        1.0,
                    );
                    let n_cx = notif_card.left + 26.0 * dpi;
                    let n_cy = notif_card.top + cc_top_h * 0.5;
                    let n_cnt = notif_str.parse::<u32>().unwrap_or(0);
                    if let Some(cb) = solid(
                        ctx,
                        if n_cnt > 0 {
                            rgba(0.46, 0.42, 0.98, 0.94)
                        } else if is_light {
                            rgba(0.0, 0.0, 0.0, 0.14)
                        } else {
                            rgba(1.0, 1.0, 1.0, 0.22)
                        },
                    ) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(n_cx, n_cy),
                                radiusX: 16.0 * dpi,
                                radiusY: 16.0 * dpi,
                            },
                            &cb,
                        );
                    }
                    draw_text(
                        ctx,
                        if n_cnt > 99 {
                            "99+"
                        } else if n_cnt > 0 {
                            &notif_str
                        } else {
                            "0"
                        },
                        d2d_rect(
                            n_cx - 16.0 * dpi,
                            n_cy - 12.0 * dpi,
                            n_cx + 16.0 * dpi,
                            n_cy + 12.0 * dpi,
                        ),
                        12.0 * dpi,
                        TextWeight::Bold,
                        if n_cnt > 0 || !is_light {
                            rgba(1.0, 1.0, 1.0, 0.98)
                        } else {
                            ink_primary
                        },
                        TextAlign::Center,
                        false,
                    );
                    draw_text(
                        ctx,
                        "通知センター",
                        d2d_rect(
                            notif_card.left + 48.0 * dpi,
                            notif_card.top + 15.0 * dpi,
                            notif_card.right - 10.0 * dpi,
                            notif_card.top + 37.0 * dpi,
                        ),
                        13.0 * dpi,
                        TextWeight::SemiBold,
                        ink_primary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    draw_text(
                        ctx,
                        if n_cnt > 0 {
                            "新着あり"
                        } else {
                            "通知なし"
                        },
                        d2d_rect(
                            notif_card.left + 48.0 * dpi,
                            notif_card.top + 36.0 * dpi,
                            notif_card.right - 10.0 * dpi,
                            notif_card.top + 57.0 * dpi,
                        ),
                        11.5 * dpi,
                        TextWeight::Regular,
                        ink_secondary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    zones.push(ClickZone {
                        rect: notif_card,
                        action: Action::Shortcut(0x4e), // Win+N
                        keep_open: false,
                    });

                    // Module 2: Floating Sound Liquid Glass Capsule
                    let (snd_card, snd_rad) = cc_modules[2];
                    draw_liquid_glass_pill(
                        ctx,
                        snd_card,
                        snd_rad,
                        style,
                        tone,
                        true,
                        shimmer_for(cursor_pos, snd_card, p.dragging_volume, dpi),
                        true,
                        1.0,
                    );
                    draw_text(
                        ctx,
                        "サウンド",
                        d2d_rect(
                            snd_card.left + 15.0 * dpi,
                            snd_card.top + 10.0 * dpi,
                            snd_card.right - 74.0 * dpi,
                            snd_card.top + 30.0 * dpi,
                        ),
                        12.5 * dpi,
                        TextWeight::SemiBold,
                        ink_primary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    let vol_txt = if sys.muted {
                        "ミュート".to_owned()
                    } else {
                        format!("{}%", sys.volume)
                    };
                    draw_text(
                        ctx,
                        &vol_txt,
                        d2d_rect(
                            snd_card.right - 84.0 * dpi,
                            snd_card.top + 10.0 * dpi,
                            snd_card.right - 15.0 * dpi,
                            snd_card.top + 30.0 * dpi,
                        ),
                        12.0 * dpi,
                        TextWeight::Medium,
                        ink_secondary,
                        TextAlign::Right,
                        txt_sh,
                    );

                    // Mute circular button
                    let mute_r = d2d_rect(
                        snd_card.left + 11.0 * dpi,
                        snd_card.top + 36.0 * dpi,
                        snd_card.left + 39.0 * dpi,
                        snd_card.top + 64.0 * dpi,
                    );
                    let mz = zones.len();
                    if hovered_zone == Some(mz) {
                        if let Some(b) = solid(ctx, hover_fill) {
                            ctx.FillEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(
                                        (mute_r.left + mute_r.right) * 0.5,
                                        (mute_r.top + mute_r.bottom) * 0.5,
                                    ),
                                    radiusX: 13.5 * dpi,
                                    radiusY: 13.5 * dpi,
                                },
                                &b,
                            );
                        }
                    }
                    draw_volume_icon(
                        ctx,
                        (mute_r.left + mute_r.right) * 0.5,
                        (mute_r.top + mute_r.bottom) * 0.5,
                        15.0 * dpi,
                        sys.volume,
                        sys.muted,
                        ink_primary,
                    );
                    zones.push(ClickZone {
                        rect: mute_r,
                        action: Action::ToggleMute,
                        keep_open: true,
                    });

                    // Authentic macOS Tahoe Concentric Thick Glass Capsule Slider
                    let tr_h = 22.0 * dpi;
                    let tr_l = snd_card.left + 45.0 * dpi;
                    let tr_r = snd_card.right - 13.0 * dpi;
                    let tr_t = snd_card.top + 39.0 * dpi;
                    let tr_rect = d2d_rect(tr_l, tr_t, tr_r, tr_t + tr_h);
                    slider_rect = Some(tr_rect);

                    if let Some(tb) = solid(ctx, track_bg) {
                        ctx.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: tr_rect,
                                radiusX: tr_h * 0.5,
                                radiusY: tr_h * 0.5,
                            },
                            &tb,
                        );
                    }
                    let frac = if sys.muted {
                        0.0
                    } else {
                        sys.volume as f32 / 100.0
                    };
                    let fill_w = (tr_h + (tr_r - tr_l - tr_h) * frac).clamp(tr_h, tr_r - tr_l);
                    let fill_rect = d2d_rect(tr_l, tr_t, tr_l + fill_w, tr_t + tr_h);
                    if let Some(fb) = linear_grad(
                        ctx,
                        pt(fill_rect.left, fill_rect.top),
                        pt(fill_rect.left, fill_rect.bottom),
                        &[
                            D2D1_GRADIENT_STOP {
                                position: 0.0,
                                color: rgba(1.0, 1.0, 1.0, if sys.muted { 0.42 } else { 0.96 }),
                            },
                            D2D1_GRADIENT_STOP {
                                position: 1.0,
                                color: rgba(0.92, 0.96, 1.0, if sys.muted { 0.28 } else { 0.84 }),
                            },
                        ],
                    ) {
                        ctx.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: fill_rect,
                                radiusX: tr_h * 0.5,
                                radiusY: tr_h * 0.5,
                            },
                            &fb,
                        );
                    }
                    if let Some(rim) = solid(ctx, track_bd) {
                        ctx.DrawRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: tr_rect,
                                radiusX: tr_h * 0.5,
                                radiusY: tr_h * 0.5,
                            },
                            &rim,
                            0.9,
                            None,
                        );
                    }
                    let thumb_cx = tr_l + fill_w - tr_h * 0.5;
                    if let Some(th) = solid(ctx, rgba(1.0, 1.0, 1.0, 0.99)) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(thumb_cx, tr_t + tr_h * 0.5),
                                radiusX: tr_h * 0.44,
                                radiusY: tr_h * 0.44,
                            },
                            &th,
                        );
                    }

                    // Module 3: Unified Floating Quick Actions Pill Bar (4 circular tactile buttons, zero nested glass!)
                    let (qa_card, qa_rad) = cc_modules[3];
                    draw_liquid_glass_pill(
                        ctx,
                        qa_card,
                        qa_rad,
                        style,
                        tone,
                        true,
                        shimmer_for(cursor_pos, qa_card, false, dpi),
                        true,
                        1.0,
                    );
                    let qa_pad = 10.0 * dpi;
                    let qa_cell_w = (qa_card.right - qa_card.left - qa_pad * 2.0) / 4.0;
                    let quick_items = [
                        ("タスク", Action::Shortcut(VK_TAB)),
                        ("スクショ", Action::ShortcutShift(0x53)),
                        ("アプリ", Action::OpenLauncher),
                        ("設定", Action::OpenSettingsPanel),
                    ];
                    for (idx, (label, act)) in quick_items.into_iter().enumerate() {
                        let ql = qa_card.left + qa_pad + idx as f32 * qa_cell_w;
                        let qr = d2d_rect(
                            ql,
                            qa_card.top + 4.0 * dpi,
                            ql + qa_cell_w,
                            qa_card.bottom - 4.0 * dpi,
                        );
                        let qz = zones.len();
                        let is_hov = hovered_zone == Some(qz);
                        let cx_btn = (qr.left + qr.right) * 0.5;
                        let cy_btn = qa_card.top + 24.0 * dpi;
                        let btn_r = 16.5 * dpi;
                        let circle_fill = if is_light {
                            rgba(0.0, 0.0, 0.0, if is_hov { 0.14 } else { 0.07 })
                        } else {
                            rgba(1.0, 1.0, 1.0, if is_hov { 0.28 } else { 0.14 })
                        };
                        let circle_rim = if is_light {
                            rgba(0.0, 0.0, 0.0, if is_hov { 0.22 } else { 0.12 })
                        } else {
                            rgba(1.0, 1.0, 1.0, if is_hov { 0.50 } else { 0.24 })
                        };
                        if let Some(cb) = solid(ctx, circle_fill) {
                            ctx.FillEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(cx_btn, cy_btn),
                                    radiusX: btn_r,
                                    radiusY: btn_r,
                                },
                                &cb,
                            );
                        }
                        if let Some(rim) = solid(ctx, circle_rim) {
                            ctx.DrawEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(cx_btn, cy_btn),
                                    radiusX: btn_r,
                                    radiusY: btn_r,
                                },
                                &rim,
                                0.9,
                                None,
                            );
                        }
                        draw_cc_quick_icon(ctx, idx, cx_btn, cy_btn, 16.5 * dpi, ink_primary);
                        draw_text(
                            ctx,
                            label,
                            d2d_rect(
                                qr.left,
                                qa_card.top + 43.0 * dpi,
                                qr.right,
                                qa_card.bottom - 3.0 * dpi,
                            ),
                            10.5 * dpi,
                            TextWeight::Medium,
                            if is_hov { ink_primary } else { ink_secondary },
                            TextAlign::Center,
                            txt_sh,
                        );
                        zones.push(ClickZone {
                            rect: qr,
                            action: act,
                            keep_open: false,
                        });
                    }
                }

                PanelKind::Network { from_cc } => {
                    // Apple HIG macOS Tahoe Native Liquid Glass Network & Connectivity Panel
                    // 3 independent floating Liquid Glass modules (ZERO Glass-on-Glass!)

                    // Module 0: Primary Connection Status & Header
                    let (hdr_card, hdr_rad) = net_modules[0];
                    draw_liquid_glass_pill(
                        ctx,
                        hdr_card,
                        hdr_rad,
                        style,
                        tone,
                        true,
                        shimmer_for(cursor_pos, hdr_card, false, dpi),
                        true,
                        1.0,
                    );

                    // Top row: Back pill (if from CC) or Title + Refresh button
                    let top_row_y = hdr_card.top + 9.0 * dpi;
                    let top_row_h = 22.0 * dpi;
                    if *from_cc {
                        let back_r = d2d_rect(
                            hdr_card.left + 10.0 * dpi,
                            top_row_y,
                            hdr_card.left + 150.0 * dpi,
                            top_row_y + top_row_h,
                        );
                        let bz = zones.len();
                        if hovered_zone == Some(bz) {
                            if let Some(b) = solid(ctx, hover_fill) {
                                ctx.FillRoundedRectangle(
                                    &D2D1_ROUNDED_RECT {
                                        rect: back_r,
                                        radiusX: top_row_h * 0.5,
                                        radiusY: top_row_h * 0.5,
                                    },
                                    &b,
                                );
                            }
                        }
                        draw_back_chevron(
                            ctx,
                            back_r.left + 9.0 * dpi,
                            top_row_y + top_row_h * 0.5,
                            12.0 * dpi,
                            ink_secondary,
                        );
                        draw_text(
                            ctx,
                            "コントロールセンター",
                            d2d_rect(
                                back_r.left + 18.0 * dpi,
                                top_row_y,
                                back_r.right,
                                top_row_y + top_row_h,
                            ),
                            11.5 * dpi,
                            TextWeight::Medium,
                            ink_secondary,
                            TextAlign::Left,
                            txt_sh,
                        );
                        zones.push(ClickZone {
                            rect: back_r,
                            action: Action::OpenControlCenter,
                            keep_open: false,
                        });
                    } else {
                        draw_text(
                            ctx,
                            "ネットワーク",
                            d2d_rect(
                                hdr_card.left + 14.0 * dpi,
                                top_row_y,
                                hdr_card.right - 72.0 * dpi,
                                top_row_y + top_row_h,
                            ),
                            12.0 * dpi,
                            TextWeight::SemiBold,
                            ink_secondary,
                            TextAlign::Left,
                            txt_sh,
                        );
                    }

                    // Refresh button capsule
                    let ref_r = d2d_rect(
                        hdr_card.right - 58.0 * dpi,
                        top_row_y,
                        hdr_card.right - 12.0 * dpi,
                        top_row_y + top_row_h,
                    );
                    let rz = zones.len();
                    if let Some(b) = solid(
                        ctx,
                        if hovered_zone == Some(rz) {
                            hover_fill
                        } else {
                            track_bg
                        },
                    ) {
                        ctx.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: ref_r,
                                radiusX: top_row_h * 0.5,
                                radiusY: top_row_h * 0.5,
                            },
                            &b,
                        );
                    }
                    draw_text(
                        ctx,
                        "更新",
                        ref_r,
                        11.0 * dpi,
                        TextWeight::Medium,
                        ink_secondary,
                        TextAlign::Center,
                        txt_sh,
                    );
                    zones.push(ClickZone {
                        rect: ref_r,
                        action: Action::RefreshNetwork,
                        keep_open: true,
                    });

                    // Primary Active Interface Summary Row
                    let sum_cy = hdr_card.top + 57.0 * dpi;
                    let badge_cx = hdr_card.left + 30.0 * dpi;
                    let net_on = sys.wifi_signal > 0;
                    if let Some(cb) = solid(
                        ctx,
                        if net_on {
                            rgba(0.14, 0.54, 1.0, 0.94)
                        } else if is_light {
                            rgba(0.0, 0.0, 0.0, 0.14)
                        } else {
                            rgba(1.0, 1.0, 1.0, 0.20)
                        },
                    ) {
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(badge_cx, sum_cy),
                                radiusX: 16.0 * dpi,
                                radiusY: 16.0 * dpi,
                            },
                            &cb,
                        );
                    }
                    draw_wifi_icon(
                        ctx,
                        badge_cx,
                        sum_cy,
                        15.0 * dpi,
                        sys.wifi_signal,
                        if net_on || !is_light {
                            rgba(1.0, 1.0, 1.0, 0.98)
                        } else {
                            ink_primary
                        },
                    );
                    let primary_ip = sys
                        .adapters
                        .iter()
                        .find(|a| a.up && !a.ipv4.is_empty())
                        .map(|a| a.ipv4.as_str())
                        .unwrap_or("");
                    let sum_sub = if !primary_ip.is_empty() && !sys.net_detail.contains(primary_ip)
                    {
                        format!("{} • {}", sys.net_detail, primary_ip)
                    } else {
                        sys.net_detail.clone()
                    };
                    draw_text(
                        ctx,
                        &sys.net_title,
                        d2d_rect(
                            hdr_card.left + 54.0 * dpi,
                            hdr_card.top + 36.0 * dpi,
                            hdr_card.right - 14.0 * dpi,
                            hdr_card.top + 58.0 * dpi,
                        ),
                        14.0 * dpi,
                        TextWeight::SemiBold,
                        ink_primary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    draw_text(
                        ctx,
                        &sum_sub,
                        d2d_rect(
                            hdr_card.left + 54.0 * dpi,
                            hdr_card.top + 56.0 * dpi,
                            hdr_card.right - 14.0 * dpi,
                            hdr_card.top + 76.0 * dpi,
                        ),
                        11.5 * dpi,
                        TextWeight::Regular,
                        ink_secondary,
                        TextAlign::Left,
                        txt_sh,
                    );

                    // Module 1: Live Adapters + Nearby Wi-Fi Networks List
                    let (list_card, list_rad) = net_modules[1];
                    draw_liquid_glass_pill(
                        ctx,
                        list_card,
                        list_rad,
                        style,
                        tone,
                        true,
                        shimmer_for(cursor_pos, list_card, false, dpi),
                        true,
                        1.0,
                    );

                    let row_inset = 10.0 * dpi;
                    let row_rad = (list_rad - row_inset).max(8.0 * dpi);
                    let mut ly = list_card.top + 10.0 * dpi;
                    let max_y = list_card.bottom - 8.0 * dpi;

                    draw_text(
                        ctx,
                        "インターフェース / Wi-Fi",
                        d2d_rect(
                            list_card.left + 14.0 * dpi,
                            ly,
                            list_card.right - 14.0 * dpi,
                            ly + 18.0 * dpi,
                        ),
                        11.5 * dpi,
                        TextWeight::Medium,
                        ink_secondary,
                        TextAlign::Left,
                        txt_sh,
                    );
                    ly += 22.0 * dpi;

                    let row_h = 30.0 * dpi;
                    // Physical / VPN Adapters first
                    for adapter in sys.adapters.iter().take(4) {
                        if ly + row_h > max_y {
                            break;
                        }
                        let rr_rect = d2d_rect(
                            list_card.left + row_inset,
                            ly,
                            list_card.right - row_inset,
                            ly + row_h,
                        );
                        let az = zones.len();
                        let is_hov = hovered_zone == Some(az);
                        if is_hov {
                            if let Some(b) = solid(ctx, hover_fill) {
                                ctx.FillRoundedRectangle(
                                    &D2D1_ROUNDED_RECT {
                                        rect: rr_rect,
                                        radiusX: row_rad,
                                        radiusY: row_rad,
                                    },
                                    &b,
                                );
                            }
                        }
                        let icx = rr_rect.left + 18.0 * dpi;
                        let icy = ly + row_h * 0.5;
                        let badge_col = if adapter.up {
                            match adapter.kind {
                                system::NetKind::Vpn => rgba(0.42, 0.46, 0.96, 0.92),
                                _ => rgba(0.14, 0.54, 1.0, 0.92),
                            }
                        } else if is_light {
                            rgba(0.0, 0.0, 0.0, 0.14)
                        } else {
                            rgba(1.0, 1.0, 1.0, 0.18)
                        };
                        if let Some(cb) = solid(ctx, badge_col) {
                            ctx.FillEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(icx, icy),
                                    radiusX: 12.0 * dpi,
                                    radiusY: 12.0 * dpi,
                                },
                                &cb,
                            );
                        }
                        let icon_sig = match (adapter.kind, adapter.up) {
                            (system::NetKind::Ethernet, true) => 4,
                            (system::NetKind::Wifi, true) => sys.wifi_signal.clamp(1, 3),
                            (system::NetKind::Wifi, false) => 0,
                            (_, true) => 4,
                            (_, false) => 0,
                        };
                        draw_wifi_icon(
                            ctx,
                            icx,
                            icy,
                            12.0 * dpi,
                            icon_sig,
                            if adapter.up || !is_light {
                                rgba(1.0, 1.0, 1.0, 0.98)
                            } else {
                                ink_primary
                            },
                        );

                        let right_badge = if adapter.up {
                            if !adapter.speed_label.is_empty() {
                                adapter.speed_label.clone()
                            } else if !adapter.ipv4.is_empty() {
                                adapter.ipv4.clone()
                            } else {
                                "接続済み".into()
                            }
                        } else {
                            "未接続".into()
                        };
                        draw_text(
                            ctx,
                            &adapter.name,
                            d2d_rect(
                                rr_rect.left + 36.0 * dpi,
                                ly,
                                rr_rect.right - 130.0 * dpi,
                                ly + row_h,
                            ),
                            12.5 * dpi,
                            if adapter.up {
                                TextWeight::SemiBold
                            } else {
                                TextWeight::Regular
                            },
                            ink_primary,
                            TextAlign::Left,
                            txt_sh,
                        );
                        draw_text(
                            ctx,
                            &right_badge,
                            d2d_rect(
                                rr_rect.right - 128.0 * dpi,
                                ly,
                                rr_rect.right - 8.0 * dpi,
                                ly + row_h,
                            ),
                            11.0 * dpi,
                            TextWeight::Medium,
                            if adapter.up { ink_secondary } else { ink_muted },
                            TextAlign::Right,
                            txt_sh,
                        );
                        let target_uri = match adapter.kind {
                            system::NetKind::Wifi => "ms-settings:network-wifi",
                            system::NetKind::Ethernet => "ms-settings:network-ethernet",
                            system::NetKind::Vpn => "ms-settings:network-vpn",
                            system::NetKind::Other => "ms-settings:network",
                        };
                        zones.push(ClickZone {
                            rect: rr_rect,
                            action: Action::Launch(target_uri.into()),
                            keep_open: false,
                        });
                        ly += row_h;
                    }

                    // Nearby Wi-Fi networks (if any scanned & space remains)
                    if !sys.wifi_networks.is_empty() && ly + row_h <= max_y {
                        if let Some(sep) = solid(ctx, sep_col) {
                            ctx.FillRectangle(
                                &d2d_rect(
                                    list_card.left + 14.0 * dpi,
                                    ly + 2.0,
                                    list_card.right - 14.0 * dpi,
                                    ly + 3.0,
                                ),
                                &sep,
                            );
                        }
                        ly += 6.0 * dpi;
                        for net in &sys.wifi_networks {
                            if ly + row_h > max_y {
                                break;
                            }
                            let rr_rect = d2d_rect(
                                list_card.left + row_inset,
                                ly,
                                list_card.right - row_inset,
                                ly + row_h,
                            );
                            let wz_i = zones.len();
                            if hovered_zone == Some(wz_i) {
                                if let Some(b) = solid(ctx, hover_fill) {
                                    ctx.FillRoundedRectangle(
                                        &D2D1_ROUNDED_RECT {
                                            rect: rr_rect,
                                            radiusX: row_rad,
                                            radiusY: row_rad,
                                        },
                                        &b,
                                    );
                                }
                            }
                            let bars = if net.signal >= 68 {
                                3
                            } else if net.signal >= 35 {
                                2
                            } else {
                                1
                            };
                            draw_wifi_icon(
                                ctx,
                                rr_rect.left + 18.0 * dpi,
                                ly + row_h * 0.5,
                                13.0 * dpi,
                                bars,
                                if net.connected {
                                    rgba(0.18, 0.58, 1.0, 0.98)
                                } else {
                                    ink_primary
                                },
                            );
                            let ssid_right = if net.connected {
                                rr_rect.right - 82.0 * dpi
                            } else {
                                rr_rect.right - 34.0 * dpi
                            };
                            draw_text(
                                ctx,
                                &net.ssid,
                                d2d_rect(rr_rect.left + 36.0 * dpi, ly, ssid_right, ly + row_h),
                                12.5 * dpi,
                                if net.connected {
                                    TextWeight::SemiBold
                                } else {
                                    TextWeight::Regular
                                },
                                ink_primary,
                                TextAlign::Left,
                                txt_sh,
                            );
                            if net.connected {
                                draw_text(
                                    ctx,
                                    "接続済み",
                                    d2d_rect(
                                        rr_rect.right - 80.0 * dpi,
                                        ly,
                                        rr_rect.right - if net.secured { 28.0 } else { 8.0 } * dpi,
                                        ly + row_h,
                                    ),
                                    10.5 * dpi,
                                    TextWeight::Medium,
                                    rgba(0.18, 0.58, 1.0, 0.98),
                                    TextAlign::Right,
                                    txt_sh,
                                );
                            }
                            if net.secured {
                                draw_lock_icon(
                                    ctx,
                                    rr_rect.right - 16.0 * dpi,
                                    ly + row_h * 0.5,
                                    11.0 * dpi,
                                    ink_muted,
                                );
                            }
                            zones.push(ClickZone {
                                rect: rr_rect,
                                action: Action::ConnectWifi(
                                    net.iface_guid,
                                    net.ssid.clone(),
                                    net.has_profile,
                                ),
                                keep_open: false,
                            });
                            ly += row_h;
                        }
                    }

                    // Module 2: Floating Quick Connectivity Pill Bar
                    let (foot_card, foot_rad) = net_modules[2];
                    draw_liquid_glass_pill(
                        ctx,
                        foot_card,
                        foot_rad,
                        style,
                        tone,
                        true,
                        shimmer_for(cursor_pos, foot_card, false, dpi),
                        true,
                        1.0,
                    );
                    let f_pad = 7.0 * dpi;
                    let f_rad = (foot_rad - f_pad).max(7.0 * dpi);
                    let f_btns = [
                        ("ネットワーク", "ms-settings:network"),
                        ("Wi-Fi 設定", "ms-settings:network-wifi"),
                        ("Bluetooth", "ms-settings:bluetooth"),
                    ];
                    let f_w = (foot_card.right - foot_card.left - f_pad * 2.0) / 3.0;
                    for (idx, (lbl, uri)) in f_btns.into_iter().enumerate() {
                        let fl = foot_card.left + f_pad + idx as f32 * f_w;
                        let fr = d2d_rect(
                            fl + 2.0 * dpi,
                            foot_card.top + f_pad,
                            fl + f_w - 2.0 * dpi,
                            foot_card.bottom - f_pad,
                        );
                        let fz = zones.len();
                        let bg = if hovered_zone == Some(fz) {
                            hover_fill
                        } else {
                            track_bg
                        };
                        if let Some(b) = solid(ctx, bg) {
                            ctx.FillRoundedRectangle(
                                &D2D1_ROUNDED_RECT {
                                    rect: fr,
                                    radiusX: f_rad,
                                    radiusY: f_rad,
                                },
                                &b,
                            );
                        }
                        draw_text(
                            ctx,
                            lbl,
                            fr,
                            11.5 * dpi,
                            TextWeight::Medium,
                            ink_primary,
                            TextAlign::Center,
                            txt_sh,
                        );
                        zones.push(ClickZone {
                            rect: fr,
                            action: Action::Launch(uri.into()),
                            keep_open: false,
                        });
                    }
                }
            }
        });

        p.zones = zones;
        p.module_rects = module_rects;
        p.slider_rect = slider_rect;
    });
}

unsafe fn run_action(owner: HWND, key: &str, action: Action, keep_open: bool, panel_h: HWND) {
    let mut r = zeroed();
    GetWindowRect(panel_h, &mut r);
    let m = DATA.with_borrow(|d| d.as_ref().map(|p| p.margin).unwrap_or(16));
    let open_settings_pos = ((r.left + r.right) / 2, r.top + m);
    let right_anchor = (r.right - m, r.top + m);

    if !keep_open {
        DestroyWindow(panel_h);
    }

    match action {
        Action::Setting(id) => {
            let index = MODEL
                .with_borrow(|s| s.items.iter().position(|i| i.path == key))
                .map(|i| i as i32)
                .unwrap_or(-1);
            command(owner, index, id);
            if keep_open {
                apply_style();
            }
        }
        Action::Window(target) => {
            let _ = sync::switch(target as HWND, false);
        }
        Action::Close(target) => {
            if target != 0 {
                PostMessageW(target as HWND, WM_CLOSE, 0, 0);
            }
        }
        Action::Minimize(target) => {
            if target != 0 {
                ShowWindowAsync(target as HWND, SW_MINIMIZE);
            }
        }
        Action::ToggleMaximize(target) => {
            if target != 0 {
                let cmd = if IsZoomed(target as HWND) != 0 {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                ShowWindowAsync(target as HWND, cmd);
            }
        }
        Action::Launch(path) => sync::launch(owner, &path),
        Action::Shortcut(vk) => bar::shortcut(vk),
        Action::ShortcutShift(vk) => bar::shortcut_shift(vk),
        Action::LockScreen => {
            LockWorkStation();
        }
        Action::OpenSettingsPanel => {
            open_settings(owner, open_settings_pos.0, open_settings_pos.1);
        }
        Action::OpenControlCenter => {
            open_control_center(owner, right_anchor.0, right_anchor.1);
        }
        Action::OpenNetworkPanel(from_cc) => {
            open_network(owner, right_anchor.0, right_anchor.1, from_cc);
        }
        Action::OpenLauncher => {
            launcher::open(owner);
        }
        Action::RefreshNetwork => {
            system::refresh_network();
            bar::repaint();
            paint();
        }
        Action::ConnectWifi(guid, ssid, has_profile) => {
            if has_profile && system::connect_wifi_profile(guid, &ssid) {
                system::refresh_network();
                bar::repaint();
            } else {
                sync::launch(owner, "ms-settings:network-wifi");
            }
        }
        Action::ToggleMute => {
            system::toggle_mute();
            bar::repaint();
            paint();
        }
        Action::Noop => {}
    }
}

unsafe fn update_volume_from_x(x: f32) {
    let pct = DATA.with_borrow(|d| {
        let sr = d.as_ref()?.slider_rect?;
        let w = (sr.right - sr.left).max(1.0);
        Some(((x - sr.left) / w * 100.0).clamp(0.0, 100.0))
    });
    if let Some(p) = pct {
        system::set_volume(p);
        bar::repaint();
        paint();
    }
}

unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut ps = zeroed();
            BeginPaint(h, &mut ps);
            EndPaint(h, &ps);
            paint();
        }
        WM_MOUSEMOVE => {
            let x = lp as i16 as i32 as f32;
            let y = (lp >> 16) as i16 as i32 as f32;
            let dragging = DATA.with_borrow(|d| d.as_ref().is_some_and(|p| p.dragging_volume));
            if dragging {
                DATA.with_borrow_mut(|d| {
                    if let Some(p) = d.as_mut() {
                        p.cursor_pos = Some((x, y));
                    }
                });
                update_volume_from_x(x);
                return 0;
            }
            let (changed, arm_leave) = DATA.with_borrow_mut(|d| {
                let Some(p) = d.as_mut() else {
                    return (false, false);
                };
                let arm = !p.tracking_leave;
                if arm {
                    p.tracking_leave = true;
                }
                let moved_specular = match p.cursor_pos {
                    Some((px, py)) => {
                        let dx = x - px;
                        let dy = y - py;
                        dx * dx + dy * dy >= 9.0
                    }
                    None => true,
                };
                if moved_specular {
                    p.cursor_pos = Some((x, y));
                }
                let hit = p.zones.iter().position(|z| pt_in_rect(x, y, z.rect));
                let mut ch = p.hovered_zone != hit || moved_specular;
                p.hovered_zone = hit;
                if let PanelKind::Menu {
                    rows,
                    selected,
                    offset,
                    ..
                } = &mut p.kind
                {
                    let new_sel = hit
                        .and_then(|hi| {
                            let mut idx_count = 0usize;
                            for (ri, r) in rows.iter().enumerate().skip(*offset as usize) {
                                if !r.separator {
                                    if idx_count == hi {
                                        return Some(ri as i32);
                                    }
                                    idx_count += 1;
                                }
                            }
                            None
                        })
                        .unwrap_or(-1);
                    if *selected != new_sel {
                        *selected = new_sel;
                        ch = true;
                    }
                }
                (ch, arm)
            });
            if arm_leave {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
            }
            if changed {
                paint();
            }
        }
        0x02a3 => {
            // WM_MOUSELEAVE: rest cursor-tracking specular highlights cleanly
            let changed = DATA.with_borrow_mut(|d| {
                let Some(p) = d.as_mut() else { return false };
                p.tracking_leave = false;
                let mut ch = p.cursor_pos.take().is_some() || p.hovered_zone.take().is_some();
                if let PanelKind::Menu { selected, .. } = &mut p.kind {
                    if *selected != -1 {
                        *selected = -1;
                        ch = true;
                    }
                }
                ch
            });
            if changed {
                paint();
            }
        }
        WM_LBUTTONDOWN => {
            let x = lp as i16 as i32 as f32;
            let y = (lp >> 16) as i16 as i32 as f32;
            let hit_slider = DATA.with_borrow_mut(|d| {
                let Some(p) = d.as_mut() else { return false };
                if let Some(sr) = p.slider_rect {
                    if x >= sr.left - 4.0
                        && x <= sr.right + 4.0
                        && y >= sr.top - 4.0
                        && y <= sr.bottom + 4.0
                    {
                        p.dragging_volume = true;
                        p.cursor_pos = Some((x, y));
                        return true;
                    }
                }
                false
            });
            if hit_slider {
                SetCapture(h);
                update_volume_from_x(x);
                return 0;
            }
        }
        WM_LBUTTONUP => {
            let was_drag = DATA.with_borrow_mut(|d| {
                if let Some(p) = d.as_mut() {
                    if p.dragging_volume {
                        p.dragging_volume = false;
                        return true;
                    }
                }
                false
            });
            if was_drag {
                ReleaseCapture();
                return 0;
            }
            let x = lp as i16 as i32 as f32;
            let y = (lp >> 16) as i16 as i32 as f32;
            let clicked = DATA.with_borrow(|d| {
                let p = d.as_ref()?;
                let z = p.zones.iter().find(|z| pt_in_rect(x, y, z.rect))?;
                Some((p.owner, p.key.clone(), z.action.clone(), z.keep_open))
            });
            if let Some((owner, key, action, keep_open)) = clicked {
                run_action(owner, &key, action, keep_open, h);
            } else {
                // Clicked in transparent shadow margin or gaps outside floating glass modules -> dismiss!
                let outside = DATA.with_borrow(|d| {
                    let Some(p) = d.as_ref() else { return true };
                    !p.module_rects.iter().any(|r| pt_in_rect(x, y, *r))
                });
                if outside {
                    DestroyWindow(h);
                }
            }
        }
        WM_KEYDOWN | WM_MOUSEWHEEL => {
            if msg == WM_KEYDOWN && wp == VK_ESCAPE as usize {
                DestroyWindow(h);
                return 0;
            }
            if msg == WM_KEYDOWN && wp == VK_RETURN as usize {
                let chosen = DATA.with_borrow(|d| {
                    let p = d.as_ref()?;
                    if let PanelKind::Menu { rows, selected, .. } = &p.kind {
                        let r = rows.get(*selected as usize)?;
                        if !r.separator {
                            return Some((p.owner, p.key.clone(), r.action.clone()));
                        }
                    }
                    None
                });
                if let Some((owner, key, action)) = chosen {
                    run_action(owner, &key, action, false, h);
                }
                return 0;
            }
            let step = if msg == WM_MOUSEWHEEL {
                if (wp >> 16) as i16 > 0 {
                    -1
                } else {
                    1
                }
            } else if wp == VK_UP as usize {
                -1
            } else if wp == VK_DOWN as usize {
                1
            } else {
                return 0;
            };
            let changed = DATA.with_borrow_mut(|d| {
                let Some(p) = d.as_mut() else { return false };
                if let PanelKind::Menu {
                    rows,
                    selected,
                    offset,
                    ..
                } = &mut p.kind
                {
                    if rows.is_empty() {
                        return false;
                    }
                    let mut idx = if *selected < 0 {
                        if step > 0 {
                            0
                        } else {
                            rows.len() as i32 - 1
                        }
                    } else {
                        (*selected + step).clamp(0, rows.len() as i32 - 1)
                    };
                    while idx >= 0 && (idx as usize) < rows.len() && rows[idx as usize].separator {
                        idx += step;
                    }
                    if idx >= 0 && (idx as usize) < rows.len() && !rows[idx as usize].separator {
                        *selected = idx;
                        let dpi = dpi_scale(p.hwnd);
                        let row_h = (30.0 * dpi).round().max(1.0) as i32;
                        let chrome_h = (44.0 * dpi).round() as i32;
                        let vis_cap = ((p.height - p.margin * 2 - chrome_h) / row_h).max(1);
                        if *selected < *offset {
                            *offset = *selected;
                        } else if *selected >= *offset + vis_cap {
                            *offset = (*selected - vis_cap + 1).max(0);
                        }
                        return true;
                    }
                }
                false
            });
            if changed {
                paint();
            }
        }
        WM_TIMER if wp == PANEL_OUTSIDE_TIMER => {
            let down = (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0;
            let dismiss = DATA.with_borrow_mut(|d| {
                let Some(p) = d.as_mut() else { return false };
                let fresh = down && !p.prev_mouse_down;
                p.prev_mouse_down = down;
                if fresh
                    && !p.dragging_volume
                    && p.opened_at.elapsed() >= std::time::Duration::from_millis(85)
                {
                    launcher::is_outside_click_target(h, &p.module_rects)
                } else {
                    false
                }
            });
            if dismiss {
                DestroyWindow(h);
            }
        }
        WM_ACTIVATE if wp as u16 == WA_INACTIVE as u16 => {
            PostMessageW(h, WM_CLOSE, 0, 0);
        }
        WM_DESTROY => {
            KillTimer(h, PANEL_OUTSIDE_TIMER);
            HANDLE.set(null_mut());
            DATA.with_borrow_mut(|d| *d = None);
            bar::repaint();
            surface::poke_autohide();
        }
        _ => return DefWindowProcW(h, msg, wp, lp),
    };
    0
}
