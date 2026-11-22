use super::*;
use crate::glass::{
    d2d_rect, draw_icon_with_badge, draw_liquid_glass_pill, draw_search_icon, draw_text,
    linear_grad, measure_text_width, pt, radial_grad, rgba, solid, AmbientTone, GlassSurface,
    IconBadge, RawIcon, SurfaceRole, TextAlign, TextWeight,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};
use windows::{
    core::PCWSTR,
    Win32::{
        Graphics::Direct2D::{
            Common::{D2D1_GRADIENT_STOP, D2D_RECT_F},
            ID2D1Bitmap, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ELLIPSE, D2D1_ROUNDED_RECT,
        },
        System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED},
        UI::Shell::{
            BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName,
            SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
        },
    },
};
use windows_sys::Win32::UI::Input::Ime::{
    ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, ImmSetCompositionWindow, CFS_POINT,
    COMPOSITIONFORM, GCS_COMPSTR,
};

const COLS: usize = 5;
const ROWS: usize = 4;
const PER_PAGE: usize = COLS * ROWS;
const MAX_RAW_CACHE: usize = 80; // 4 pages (current + prev + next + buffer, ~2.8 MB at 96x96)
const MAX_GPU_CACHE: usize = 60; // 3 pages cached on GPU for instant adjacent page flips
const LAUNCHER_ICON_SIZE: i32 = 96;
const WM_LAUNCHER_ICONS: u32 = WM_APP + 41;
const WM_LAUNCHER_CATALOG: u32 = WM_APP + 42;
const ANIM_TIMER: usize = 43;
const OUTSIDE_TIMER: usize = 44;
const WM_IME_STARTCOMPOSITION: u32 = 0x010D;
const WM_IME_ENDCOMPOSITION: u32 = 0x010E;
const WM_IME_COMPOSITION: u32 = 0x010F;

#[derive(Clone)]
pub struct AppEntry {
    pub name: String,
    pub lower: String,
    pub launch: String,
    pub stem_lower: String,
}

impl AppEntry {
    fn new(name: String, launch: String) -> Self {
        let lower = name.to_lowercase();
        let stem_lower = Path::new(&launch)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        Self {
            name,
            lower,
            launch,
            stem_lower,
        }
    }
}

struct ClickTile {
    rect: D2D_RECT_F,
    app_idx: usize,
}

struct PageDot {
    rect: D2D_RECT_F,
    page: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HoverBtn {
    Clear,
    Refresh,
    Prev,
    Next,
    Dot(usize),
}

struct CachedIcon {
    raw: Option<RawIcon>,
    tick: u64,
}

struct IconRequest {
    gen: u64,
    hwnd: usize,
    visible_count: usize,
    paths: Vec<String>,
}

struct CatalogBatch {
    replace: bool,
    apps: Vec<AppEntry>,
}

static ICON_GEN: AtomicU64 = AtomicU64::new(1);
static CATALOG_LOADING: AtomicBool = AtomicBool::new(false);
static CATALOG_UWP_DONE: AtomicBool = AtomicBool::new(false);
static PENDING_ICONS: Mutex<Vec<(String, Option<RawIcon>)>> = Mutex::new(Vec::new());
static PENDING_CATALOG: Mutex<Option<CatalogBatch>> = Mutex::new(None);
static ICON_TX: OnceLock<Mutex<mpsc::Sender<IconRequest>>> = OnceLock::new();

struct LauncherState {
    hwnd: HWND,
    owner: HWND,
    surface: Option<GlassSurface>,
    tone: AmbientTone,
    opened_at: Instant,
    // Smooth GPU & spring animation state
    open_p: f32, // 0.0 -> 1.0 open zoom/fade/lift
    closing: bool,
    page_slide: f32, // -1.0..+1.0 horizontal Launchpad page-glide offset
    tile_scales: [f32; PER_PAGE],
    smooth_cursor: Option<(f32, f32)>,
    anim_running: bool,
    last_frame: Instant,
    // Search & catalog state
    query: String,
    ime_comp: String,
    filtered: Vec<usize>,
    page: usize,
    selected: usize,
    hovered_tile: Option<usize>,
    pressed_tile: Option<usize>,
    hovered_btn: Option<HoverBtn>,
    tiles: Vec<ClickTile>,
    dots: Vec<PageDot>,
    prev_btn: Option<D2D_RECT_F>,
    next_btn: Option<D2D_RECT_F>,
    clear_btn: Option<D2D_RECT_F>,
    refresh_btn: Option<D2D_RECT_F>,
    search_rect: D2D_RECT_F,
    module_rects: [D2D_RECT_F; 3],
    cursor_pos: Option<(f32, f32)>,
    tracking_leave: bool,
    prev_mouse_down: bool,
    wheel_accum: i32,
    status_hint: String,
    width: i32,
    height: i32,
    margin: i32,
    d2d_icons: HashMap<String, (ID2D1Bitmap, Option<IconBadge>)>,
}

thread_local! {
    static HANDLE: Cell<HWND> = const { Cell::new(null_mut()) };
    static STATE: RefCell<Option<LauncherState>> = const { RefCell::new(None) };
    static CATALOG: RefCell<Vec<AppEntry>> = const { RefCell::new(Vec::new()) };
    static ICON_CACHE: RefCell<HashMap<String, CachedIcon>> = RefCell::new(HashMap::new());
    static ICON_TICK: Cell<u64> = const { Cell::new(1) };
    static ICON_INFLIGHT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

pub fn is_open() -> bool {
    !HANDLE.get().is_null() && STATE.with_borrow(|slot| slot.as_ref().is_some_and(|st| !st.closing))
}

/// Immediate close (used during app shutdown or switching directly into another panel).
pub unsafe fn close() {
    let h = HANDLE.get();
    if !h.is_null() {
        DestroyWindow(h);
    }
}

/// Silky macOS Launchpad zoom-out + fade-out close animation.
pub unsafe fn close_animated() {
    let h = HANDLE.get();
    if h.is_null() {
        return;
    }
    let started = STATE.with_borrow_mut(|slot| {
        let Some(st) = slot.as_mut() else {
            return false;
        };
        if st.closing {
            return false;
        }
        st.closing = true;
        st.pressed_tile = None;
        true
    });
    if started {
        ensure_animating(h);
        bar::repaint();
    }
}

pub unsafe fn toggle(owner: HWND) {
    if is_open() {
        close_animated();
    } else {
        open(owner);
    }
}

pub unsafe fn apply_style() {
    let style = MODEL.with_borrow(|s| s.glass);
    STATE.with_borrow(|slot| {
        if let Some(st) = slot {
            if let Some(surf) = &st.surface {
                surf.set_style(style);
            }
        }
    });
    if !HANDLE.get().is_null() {
        paint();
    }
}

/// Reliably gives keyboard focus to a popup opened from a WS_EX_NOACTIVATE Dock or Menu Bar.
pub unsafe fn force_foreground(h: HWND) {
    let fg = GetForegroundWindow();
    let my_tid = GetCurrentThreadId();
    let fg_tid = if !fg.is_null() {
        GetWindowThreadProcessId(fg, null_mut())
    } else {
        0
    };
    let attached = fg_tid != 0 && fg_tid != my_tid && AttachThreadInput(my_tid, fg_tid, 1) != 0;
    BringWindowToTop(h);
    SetForegroundWindow(h);
    SetFocus(h);
    if attached {
        AttachThreadInput(my_tid, fg_tid, 0);
    }
}

unsafe fn ensure_animating(h: HWND) {
    STATE.with_borrow_mut(|slot| {
        if let Some(st) = slot.as_mut() {
            if !st.anim_running {
                st.anim_running = true;
                st.last_frame = Instant::now();
                SetTimer(h, ANIM_TIMER, 12, None);
            }
        }
    });
}

fn apply_root_transform(st: &LauncherState) {
    let Some(surf) = &st.surface else { return };
    let p = st.open_p.clamp(0.0, 1.0);
    // Smooth cubic ease-out with delicate Launchpad spring settle
    let ease = 1.0 - (1.0 - p).powi(3);
    let scale = if st.closing {
        0.92 + 0.08 * ease
    } else {
        // Gentle micro-overshoot near 0.75 -> settles crisply to 1.00
        let overshoot = (p * std::f32::consts::PI).sin() * (1.0 - p) * 0.022;
        0.91 + 0.09 * ease + overshoot
    };
    let offset_y = (1.0 - ease) * if st.closing { 10.0 } else { 15.0 };
    let opacity = ease.clamp(0.0, 1.0);
    surf.set_transform(
        st.width as f32 * 0.5,
        st.height as f32 * 0.5,
        scale,
        offset_y,
        opacity,
    );
}

fn is_clutter_name(name: &str) -> bool {
    let l = name.to_lowercase();
    l.contains("uninstall")
        || l.contains("アンインストール")
        || l.contains("readme")
        || l.contains("お読みください")
        || l.contains("license")
        || l.contains("ライセンス")
        || l.ends_with(" help")
        || l.contains("マニュアル")
        || l.contains("website")
        || l.contains("ドキュメント")
        || l.starts_with("desktop.ini")
}

unsafe fn scan_shell_apps_folder(out: &mut Vec<AppEntry>, seen: &mut HashSet<String>) {
    let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(
        PCWSTR(w("shell:AppsFolder").as_ptr()),
        None,
    ) else {
        return;
    };
    let Ok(enum_items): windows::core::Result<IEnumShellItems> =
        folder.BindToHandler(None, &BHID_EnumItems)
    else {
        return;
    };
    loop {
        let mut arr: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        if enum_items.Next(&mut arr, Some(&mut fetched)).is_err() || fetched == 0 {
            break;
        }
        let Some(item) = arr[0].take() else {
            break;
        };
        let name = match item.GetDisplayName(SIGDN_NORMALDISPLAY) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0.cast()));
                s
            }
            Err(_) => continue,
        };
        let rel = match item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0.cast()));
                s
            }
            Err(_) => continue,
        };
        let trimmed = name.trim();
        if trimmed.is_empty() || rel.is_empty() || is_clutter_name(trimmed) {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(AppEntry::new(
                trimmed.to_owned(),
                format!("shell:AppsFolder\\{rel}"),
            ));
        }
    }
}

fn walk_start_menu(
    dir: &PathBuf,
    depth: usize,
    out: &mut Vec<AppEntry>,
    seen: &mut HashSet<String>,
) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_start_menu(&path, depth + 1, out, seen);
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !ext.eq_ignore_ascii_case("lnk") && !ext.eq_ignore_ascii_case("appref-ms") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let trimmed = stem.trim();
        if trimmed.is_empty() || is_clutter_name(trimmed) {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(AppEntry::new(
                trimmed.to_owned(),
                path.to_string_lossy().into_owned(),
            ));
        }
    }
}

fn discover_start_menu_fast() -> Vec<AppEntry> {
    let mut out = Vec::with_capacity(128);
    let mut seen = HashSet::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let p = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs");
        walk_start_menu(&p, 0, &mut out, &mut seen);
    }
    if let Some(progdata) = std::env::var_os("ProgramData") {
        let p = PathBuf::from(progdata).join(r"Microsoft\Windows\Start Menu\Programs");
        walk_start_menu(&p, 0, &mut out, &mut seen);
    }
    out.sort_by(|a, b| a.lower.cmp(&b.lower));
    out
}

/// Disposable CLI worker entry (`macui.exe --scan-uwp`).
/// Enumerates `shell:AppsFolder` in a short-lived process so heavy WinRT/XAML/StateRepository
/// DLLs (~60-80 MB commit + 30+ threads) never enter the long-running `macui.exe` process!
pub unsafe fn run_scan_uwp_helper() {
    use std::io::{BufWriter, Write};
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
    let mut apps = Vec::with_capacity(128);
    let mut seen = HashSet::new();
    scan_shell_apps_folder(&mut apps, &mut seen);
    CoUninitialize();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for app in apps {
        let name = app.name.replace(['\t', '\r', '\n'], " ");
        let launch = app.launch.replace(['\t', '\r', '\n'], "");
        let _ = writeln!(out, "{name}\t{launch}");
    }
    let _ = out.flush();
}

unsafe fn scan_shell_apps_isolated() -> Vec<AppEntry> {
    if !cfg!(test) {
        if let Ok(exe) = std::env::current_exe() {
            use std::os::windows::process::CommandExt;
            use std::process::{Command, Stdio};
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            if let Ok(output) = Command::new(&exe)
                .arg("--scan-uwp")
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    let mut apps = Vec::with_capacity(128);
                    let mut seen = HashSet::new();
                    for line in text.lines() {
                        if let Some((name, launch)) = line.split_once('\t') {
                            let trimmed = name.trim();
                            if !trimmed.is_empty()
                                && !launch.is_empty()
                                && !is_clutter_name(trimmed)
                                && seen.insert(trimmed.to_lowercase())
                            {
                                apps.push(AppEntry::new(trimmed.to_owned(), launch.to_owned()));
                            }
                        }
                    }
                    return apps;
                }
            }
        }
    }
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
    let mut out = Vec::with_capacity(128);
    let mut seen = HashSet::new();
    scan_shell_apps_folder(&mut out, &mut seen);
    CoUninitialize();
    out
}

pub unsafe fn discover_apps() -> Vec<AppEntry> {
    let mut out = discover_start_menu_fast();
    let mut seen: HashSet<String> = out.iter().map(|a| a.lower.clone()).collect();
    for app in scan_shell_apps_isolated() {
        if seen.insert(app.lower.clone()) {
            out.push(app);
        }
    }
    out.sort_by(|a, b| a.lower.cmp(&b.lower));
    out
}

unsafe fn spawn_catalog_scan(hwnd: HWND, full_replace: bool) {
    if CATALOG_LOADING.swap(true, Ordering::SeqCst) {
        return;
    }
    let hwnd_usize = hwnd as usize;
    std::thread::spawn(move || {
        let apps = if full_replace {
            tasks::invalidate_start_links_cache();
            discover_apps()
        } else {
            scan_shell_apps_isolated()
        };
        CATALOG_UWP_DONE.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = PENDING_CATALOG.lock() {
            *slot = Some(CatalogBatch {
                replace: full_replace,
                apps,
            });
        }
        CATALOG_LOADING.store(false, Ordering::SeqCst);
        PostMessageW(hwnd_usize as HWND, WM_LAUNCHER_CATALOG, 0, 0);
    });
}

fn ensure_icon_worker() -> &'static Mutex<mpsc::Sender<IconRequest>> {
    ICON_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<IconRequest>();
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
            while let Ok(mut req) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    req = newer;
                }
                let vis_n = req.visible_count.min(req.paths.len());
                let (vis_slice, pre_slice) = req.paths.split_at(vis_n);

                // Phase 1 (Visible page): decode local .lnk/.exe immediately, then batch UWP icons
                // into a single disposable `--uwp-icons` subprocess so visible tiles pop in fast
                // without ever loading Shell/XAML DLLs into resident macui.exe!
                for (phase_idx, slice) in [vis_slice, pre_slice].into_iter().enumerate() {
                    if slice.is_empty() || ICON_GEN.load(Ordering::Relaxed) != req.gen {
                        break;
                    }
                    let flush_step = if phase_idx == 0 { 4 } else { 8 };
                    let mut batch_count = 0usize;
                    let mut uwp_pending: Vec<String> = Vec::new();

                    for path in slice {
                        if ICON_GEN.load(Ordering::Relaxed) != req.gen {
                            break;
                        }
                        if let Some(raw) = tasks::extract_clean_icon_fast(path, LAUNCHER_ICON_SIZE)
                        {
                            if let Ok(mut pending) = PENDING_ICONS.lock() {
                                pending.push((path.clone(), Some(raw)));
                            }
                            batch_count += 1;
                            if batch_count >= flush_step {
                                batch_count = 0;
                                PostMessageW(req.hwnd as HWND, WM_LAUNCHER_ICONS, 0, 0);
                            }
                        } else {
                            uwp_pending.push(path.clone());
                        }
                    }
                    if batch_count > 0 {
                        PostMessageW(req.hwnd as HWND, WM_LAUNCHER_ICONS, 0, 0);
                    }
                    if !uwp_pending.is_empty() && ICON_GEN.load(Ordering::Relaxed) == req.gen {
                        let decoded =
                            tasks::extract_uwp_icons_batch(&uwp_pending, LAUNCHER_ICON_SIZE);
                        if let Ok(mut pending) = PENDING_ICONS.lock() {
                            for (p, raw) in uwp_pending.into_iter().zip(decoded) {
                                pending.push((p, raw));
                            }
                        }
                        PostMessageW(req.hwnd as HWND, WM_LAUNCHER_ICONS, 0, 0);
                    }
                }
            }
            CoUninitialize();
        });
        Mutex::new(tx)
    })
}

/// Returns true if the current cursor position is outside a popup's active glass modules
/// AND not on the top Menu Bar or Dock (whose own click handlers manage toggles cleanly).
pub unsafe fn is_outside_click_target(popup_hwnd: HWND, module_rects: &[D2D_RECT_F]) -> bool {
    let mut pt_s: POINT = zeroed();
    if GetCursorPos(&mut pt_s) == 0 {
        return false;
    }
    // Exclude top Menu Bar and Dock hitbox so their own toggle/blank handlers run cleanly on LBUTTONUP
    for cls in ["MacUI.Status", "MacUI.GlassDock.1"] {
        let h = FindWindowW(w(cls).as_ptr(), null());
        if !h.is_null() && IsWindowVisible(h) != 0 {
            let mut r: RECT = zeroed();
            if GetWindowRect(h, &mut r) != 0
                && pt_s.x >= r.left
                && pt_s.x < r.right
                && pt_s.y >= r.top
                && pt_s.y < r.bottom
            {
                return false;
            }
        }
    }
    let mut pr: RECT = zeroed();
    if GetWindowRect(popup_hwnd, &mut pr) == 0 {
        return true;
    }
    if pt_s.x < pr.left || pt_s.x >= pr.right || pt_s.y < pr.top || pt_s.y >= pr.bottom {
        return true;
    }
    let lx = (pt_s.x - pr.left) as f32;
    let ly = (pt_s.y - pr.top) as f32;
    !module_rects.iter().any(|r| pt_in(lx, ly, *r))
}

fn cache_touch_and_insert(path: String, raw: Option<RawIcon>) {
    let tick = ICON_TICK.get().wrapping_add(1);
    ICON_TICK.set(tick);
    ICON_CACHE.with_borrow_mut(|cache| {
        cache.insert(path, CachedIcon { raw, tick });
        if cache.len() > MAX_RAW_CACHE {
            let mut keys_by_age: Vec<(u64, String)> =
                cache.iter().map(|(k, v)| (v.tick, k.clone())).collect();
            keys_by_age.sort_unstable_by_key(|(t, _)| *t);
            let remove_n = cache.len().saturating_sub(MAX_RAW_CACHE);
            for (_, k) in keys_by_age.into_iter().take(remove_n) {
                cache.remove(&k);
            }
        }
    });
}

fn trim_icon_cache(max_entries: usize) {
    ICON_CACHE.with_borrow_mut(|cache| {
        if cache.len() > max_entries {
            let mut keys_by_age: Vec<(u64, String)> =
                cache.iter().map(|(k, v)| (v.tick, k.clone())).collect();
            keys_by_age.sort_unstable_by_key(|(t, _)| *t);
            let remove_n = cache.len().saturating_sub(max_entries);
            for (_, k) in keys_by_age.into_iter().take(remove_n) {
                cache.remove(&k);
            }
        }
    });
}

/// Computes slice ranges for the current page and its adjacent preload pages (next first, then prev).
fn collect_page_and_adjacent_ranges(
    total: usize,
    page: usize,
    per_page: usize,
) -> (std::ops::Range<usize>, Vec<std::ops::Range<usize>>) {
    if total == 0 || per_page == 0 {
        return (0..0, Vec::new());
    }
    let total_pages = total.div_ceil(per_page);
    let cur = page.min(total_pages - 1);
    let range_of = |p: usize| {
        let s = p * per_page;
        s..(s + per_page).min(total)
    };
    let mut adj = Vec::with_capacity(2);
    if cur + 1 < total_pages {
        adj.push(range_of(cur + 1));
    }
    if cur > 0 {
        adj.push(range_of(cur - 1));
    }
    (range_of(cur), adj)
}

fn queue_missing_icons(hwnd: HWND, visible_paths: &[String], preload_paths: &[String]) {
    let base_tick = ICON_TICK.get().wrapping_add(1);
    ICON_TICK.set(base_tick.wrapping_add(1));

    let (req_paths, visible_count) = ICON_CACHE.with_borrow_mut(|cache| {
        ICON_INFLIGHT.with_borrow_mut(|inflight| {
            // Touch adjacent pages first, then current page with higher tick so current page stays freshest
            let mut has_new = false;
            for p in preload_paths {
                if let Some(entry) = cache.get_mut(p) {
                    entry.tick = base_tick;
                } else if !inflight.contains(p) {
                    has_new = true;
                }
            }
            for p in visible_paths {
                if let Some(entry) = cache.get_mut(p) {
                    entry.tick = base_tick.wrapping_add(1);
                } else if !inflight.contains(p) {
                    has_new = true;
                }
            }
            if !has_new {
                return (Vec::new(), 0);
            }

            // New page/query needs icons: cancel stale in-flight preloads and queue current -> next -> prev!
            inflight.clear();
            let mut seen = HashSet::new();
            let mut ordered = Vec::with_capacity(visible_paths.len() + preload_paths.len());
            for p in visible_paths {
                if !cache.contains_key(p) && seen.insert(p.as_str()) {
                    inflight.insert(p.clone());
                    ordered.push(p.clone());
                }
            }
            let vis_n = ordered.len();
            for p in preload_paths {
                if !cache.contains_key(p) && seen.insert(p.as_str()) {
                    inflight.insert(p.clone());
                    ordered.push(p.clone());
                }
            }
            (ordered, vis_n)
        })
    });

    if req_paths.is_empty() {
        return;
    }
    let gen = ICON_GEN.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    if let Ok(tx) = ensure_icon_worker().lock() {
        let _ = tx.send(IconRequest {
            gen,
            hwnd: hwnd as usize,
            visible_count,
            paths: req_paths,
        });
    }
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut n = needle.chars();
    let Some(mut cur) = n.next() else {
        return true;
    };
    for h in haystack.chars() {
        if h == cur {
            match n.next() {
                Some(next_c) => cur = next_c,
                None => return true,
            }
        }
    }
    false
}

fn alias_matches(app_lower: &str, stem_lower: &str, q: &str) -> bool {
    const ALIASES: &[(&str, &[&str])] = &[
        ("電卓", &["calc", "calculator"]),
        ("メモ帳", &["notepad", "memo"]),
        ("設定", &["settings", "config", "settei"]),
        ("ペイント", &["paint", "mspaint"]),
        ("エクスプローラー", &["explorer", "finder", "files"]),
        ("ターミナル", &["terminal", "wt", "shell"]),
        ("コマンド プロンプト", &["cmd", "command", "prompt"]),
        (
            "タスク マネージャー",
            &["taskmgr", "task manager", "activity"],
        ),
        ("切り取り", &["snip", "snipping", "screenshot"]),
        ("コントロール パネル", &["control", "control panel"]),
        ("フォト", &["photos", "gallery"]),
        ("クロック", &["clock", "timer", "alarm"]),
        ("カメラ", &["camera"]),
        ("カレンダー", &["calendar"]),
        ("天気", &["weather"]),
        ("メール", &["mail", "outlook"]),
        ("ストア", &["store", "microsoft store"]),
    ];
    for (jp, en_list) in ALIASES {
        if app_lower.contains(jp) || stem_lower.contains(jp) {
            if en_list.iter().any(|e| e.starts_with(q) || q.starts_with(e)) {
                return true;
            }
        }
    }
    false
}

fn match_score(app: &AppEntry, q: &str) -> Option<u8> {
    if q.is_empty() {
        return Some(0);
    }
    if app.lower == q {
        return Some(0);
    }
    if app.lower.starts_with(q) {
        return Some(1);
    }
    if app
        .lower
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '.' | '(' | ')'))
        .any(|tok| !tok.is_empty() && tok.starts_with(q))
    {
        return Some(2);
    }
    if app.lower.contains(q)
        || (!app.stem_lower.is_empty() && app.stem_lower.contains(q))
        || alias_matches(&app.lower, &app.stem_lower, q)
    {
        return Some(3);
    }
    if q.chars().count() >= 2 && is_subsequence(q, &app.lower) {
        return Some(4);
    }
    None
}

fn effective_query(st: &LauncherState) -> String {
    if st.ime_comp.is_empty() {
        st.query.clone()
    } else {
        format!("{}{}", st.query, st.ime_comp)
    }
}

fn rebuild_filter(st: &mut LauncherState) {
    let raw_q = effective_query(st);
    let q = raw_q.trim().replace('　', " ").to_lowercase();
    st.filtered = CATALOG.with_borrow(|cat| {
        if q.is_empty() {
            return (0..cat.len()).collect();
        }
        let mut scored: Vec<(u8, usize)> = cat
            .iter()
            .enumerate()
            .filter_map(|(i, a)| match_score(a, &q).map(|sc| (sc, i)))
            .collect();
        scored.sort_by(|(sa, ia), (sb, ib)| {
            sa.cmp(sb).then_with(|| cat[*ia].lower.cmp(&cat[*ib].lower))
        });
        scored.into_iter().map(|(_, i)| i).collect()
    });
    st.page = 0;
    st.selected = 0;
    st.hovered_tile = None;
    st.pressed_tile = None;
    st.page_slide = 0.0;
    st.tile_scales = [1.0; PER_PAGE];
}

pub unsafe fn open(owner: HWND) {
    panel::close();
    close();

    let need_uwp = CATALOG.with_borrow_mut(|cat| {
        if cat.is_empty() {
            *cat = discover_start_menu_fast();
            true
        } else {
            !CATALOG_UWP_DONE.load(Ordering::Relaxed)
        }
    });

    panel::register("MacUI.Launcher", Some(proc));
    let dpi = GetDpiForWindow(owner).max(96) as f32 / 96.0;
    let screen_w = GetSystemMetrics(SM_CXSCREEN).max(640);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(480);
    let bar_h = bar::height().max(26);

    let max_w = (screen_w - (32.0 * dpi).round() as i32).max(440);
    let max_h = (screen_h - bar_h - (64.0 * dpi).round() as i32).max(360);
    let content_w = ((684.0 * dpi).round() as i32).min(max_w);
    let content_h = ((536.0 * dpi).round() as i32).min(max_h);
    let margin = (22.0 * dpi).round() as i32;
    let total_w = content_w + margin * 2;
    let total_h = content_h + margin * 2;

    let cx = (screen_w - content_w) / 2;
    let min_top = bar_h + (10.0 * dpi).round() as i32;
    let cy = ((screen_h - content_h) / 2 - (10.0 * dpi).round() as i32).max(min_top);
    let wx = cx - margin;
    let wy = cy - margin;

    let tone = system::sample_screen_tone(cx, cy, content_w, content_h);

    let h = CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        w("MacUI.Launcher").as_ptr(),
        w("MacUI アプリ ランチャー").as_ptr(),
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
    // 3 independent floating Liquid Glass modules (Search capsule + Refresh capsule + Main Grid)
    // using SurfaceRole::Dock crystal-clear optical refraction (ZERO Glass-on-Glass!).
    let surf = GlassSurface::new(
        h,
        SurfaceRole::Dock,
        &[SurfaceRole::Dock, SurfaceRole::Dock],
        style,
    );

    let mut st = LauncherState {
        hwnd: h,
        owner,
        surface: surf,
        tone,
        opened_at: Instant::now(),
        open_p: 0.02,
        closing: false,
        page_slide: 0.0,
        tile_scales: [1.0; PER_PAGE],
        smooth_cursor: None,
        anim_running: false,
        last_frame: Instant::now(),
        query: String::new(),
        ime_comp: String::new(),
        filtered: Vec::new(),
        page: 0,
        selected: 0,
        hovered_tile: None,
        pressed_tile: None,
        hovered_btn: None,
        tiles: Vec::new(),
        dots: Vec::new(),
        prev_btn: None,
        next_btn: None,
        clear_btn: None,
        refresh_btn: None,
        search_rect: d2d_rect(0.0, 0.0, 0.0, 0.0),
        module_rects: [d2d_rect(0.0, 0.0, 0.0, 0.0); 3],
        cursor_pos: None,
        tracking_leave: false,
        prev_mouse_down: (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0
            || (GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0,
        wheel_accum: 0,
        status_hint: "クリック / Enter で起動 • 右クリックで Dock に追加".into(),
        width: total_w,
        height: total_h,
        margin,
        d2d_icons: HashMap::new(),
    };
    rebuild_filter(&mut st);
    apply_root_transform(&st);

    STATE.with_borrow_mut(|slot| *slot = Some(st));
    HANDLE.set(h);
    paint();
    ShowWindow(h, SW_SHOW);
    force_foreground(h);
    SetTimer(h, OUTSIDE_TIMER, 75, None);
    ensure_animating(h);
    bar::repaint();

    if need_uwp {
        spawn_catalog_scan(h, false);
    }
}

#[inline]
fn pt_in(x: f32, y: f32, r: D2D_RECT_F) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

unsafe fn position_ime_window(h: HWND) {
    let pos = STATE.with_borrow(|slot| {
        let st = slot.as_ref()?;
        let dpi = GetDpiForWindow(h).max(96) as f32 / 96.0;
        let typed_w = measure_text_width(&st.query, 14.0 * dpi, TextWeight::Medium);
        let x = (st.search_rect.left + 42.0 * dpi + typed_w)
            .min((st.search_rect.right - 90.0 * dpi).max(st.search_rect.left + 42.0 * dpi));
        let y = st.search_rect.bottom + 4.0 * dpi;
        Some((x.round() as i32, y.round() as i32))
    });
    let Some((x, y)) = pos else { return };
    let himc = ImmGetContext(h);
    if !himc.is_null() {
        let mut cf: COMPOSITIONFORM = zeroed();
        cf.dwStyle = CFS_POINT;
        cf.ptCurrentPos = POINT { x, y };
        ImmSetCompositionWindow(himc, &cf);
        ImmReleaseContext(h, himc);
    }
}

unsafe fn read_ime_comp_string(h: HWND) -> String {
    let himc = ImmGetContext(h);
    if himc.is_null() {
        return String::new();
    }
    let bytes = ImmGetCompositionStringW(himc, GCS_COMPSTR, null_mut(), 0);
    let out = if bytes > 0 {
        let mut buf = vec![0u16; (bytes as usize) / 2];
        let copied =
            ImmGetCompositionStringW(himc, GCS_COMPSTR, buf.as_mut_ptr().cast(), bytes as u32);
        if copied > 0 {
            String::from_utf16_lossy(&buf[..(copied as usize) / 2])
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    ImmReleaseContext(h, himc);
    out
}

unsafe fn drain_pending_icons() -> bool {
    let batch = match PENDING_ICONS.lock() {
        Ok(mut g) if !g.is_empty() => std::mem::take(&mut *g),
        _ => return false,
    };
    let visible_set: HashSet<String> = STATE.with_borrow(|slot| {
        let Some(st) = slot.as_ref() else {
            return HashSet::new();
        };
        let (cur_range, _) = collect_page_and_adjacent_ranges(st.filtered.len(), st.page, PER_PAGE);
        CATALOG.with_borrow(|cat| {
            st.filtered[cur_range]
                .iter()
                .filter_map(|&idx| cat.get(idx).map(|a| a.launch.clone()))
                .collect()
        })
    });
    let mut visible_updated = false;
    ICON_INFLIGHT.with_borrow_mut(|inflight| {
        for (path, raw) in batch {
            inflight.remove(&path);
            if visible_set.contains(&path) {
                visible_updated = true;
            }
            cache_touch_and_insert(path, raw);
        }
    });
    visible_updated
}

unsafe fn drain_pending_catalog() -> bool {
    let batch = match PENDING_CATALOG.lock() {
        Ok(mut g) => g.take(),
        _ => return false,
    };
    let Some(batch) = batch else { return false };
    CATALOG.with_borrow_mut(|cat| {
        if batch.replace {
            *cat = batch.apps;
        } else {
            let mut seen: HashSet<String> = cat.iter().map(|a| a.lower.clone()).collect();
            for app in batch.apps {
                if seen.insert(app.lower.clone()) {
                    cat.push(app);
                }
            }
            cat.sort_by(|a, b| a.lower.cmp(&b.lower));
        }
    });
    STATE.with_borrow_mut(|slot| {
        if let Some(st) = slot.as_mut() {
            let prev_page = st.page;
            if batch.replace {
                st.d2d_icons.clear();
                st.status_hint = "アプリ一覧を更新しました".into();
            }
            rebuild_filter(st);
            let total_pages = st.filtered.len().div_ceil(PER_PAGE).max(1);
            st.page = prev_page.min(total_pages - 1);
        }
    });
    true
}

unsafe fn on_anim_timer(h: HWND) {
    let now = Instant::now();
    let mut need_d2d_repaint = false;
    let mut finish_close = false;

    STATE.with_borrow_mut(|slot| {
        let Some(st) = slot.as_mut() else {
            KillTimer(h, ANIM_TIMER);
            return;
        };
        let dt = now
            .duration_since(st.last_frame)
            .as_secs_f32()
            .clamp(0.001, 0.05);
        st.last_frame = now;

        let mut still_active = false;

        // 1. Hardware-composited open/close zoom + lift + fade animation
        if st.closing {
            st.open_p = (st.open_p - dt * 7.8).max(0.0);
            apply_root_transform(st);
            if st.open_p <= 0.005 {
                finish_close = true;
            } else {
                still_active = true;
            }
        } else if st.open_p < 1.0 {
            st.open_p = (st.open_p + dt * 5.4).min(1.0);
            apply_root_transform(st);
            if st.open_p < 0.999 {
                still_active = true;
            } else {
                st.open_p = 1.0;
                apply_root_transform(st);
            }
        }

        // 2. Horizontal Launchpad page-glide spring
        if st.page_slide.abs() > 0.002 {
            let k_slide = 1.0 - (-dt * 19.0).exp();
            st.page_slide *= 1.0 - k_slide;
            if st.page_slide.abs() <= 0.004 {
                st.page_slide = 0.0;
            } else {
                still_active = true;
            }
            need_d2d_repaint = true;
        }

        // 3. Per-tile silky hover/press scale springs (matching Dock physics!)
        let k_tile = 1.0 - (-dt * 22.0).exp();
        for i in 0..PER_PAGE {
            let target = if st.pressed_tile == Some(i) {
                0.92
            } else if st.hovered_tile == Some(i) {
                1.12
            } else {
                1.0
            };
            let diff = target - st.tile_scales[i];
            if diff.abs() > 0.0025 {
                st.tile_scales[i] += diff * k_tile;
                still_active = true;
                need_d2d_repaint = true;
            } else if st.tile_scales[i] != target {
                st.tile_scales[i] = target;
                need_d2d_repaint = true;
            }
        }

        // 4. Smooth cursor-tracking Liquid Glass specular shimmer
        if let Some((tx, ty)) = st.cursor_pos {
            let (sx, sy) = st.smooth_cursor.unwrap_or((tx, ty));
            let dx = tx - sx;
            let dy = ty - sy;
            if dx * dx + dy * dy > 1.5 {
                let k_cur = 1.0 - (-dt * 20.0).exp();
                st.smooth_cursor = Some((sx + dx * k_cur, sy + dy * k_cur));
                still_active = true;
                need_d2d_repaint = true;
            } else {
                st.smooth_cursor = Some((tx, ty));
            }
        } else if st.smooth_cursor.take().is_some() {
            need_d2d_repaint = true;
        }

        if !still_active && st.anim_running {
            KillTimer(h, ANIM_TIMER);
            st.anim_running = false;
        }
    });

    if finish_close {
        DestroyWindow(h);
        return;
    }
    if need_d2d_repaint {
        paint();
    }
}

unsafe fn paint() {
    let style = MODEL.with_borrow(|s| s.glass);

    let (hwnd_opt, visible_paths, preload_paths): (Option<HWND>, Vec<String>, Vec<String>) = STATE
        .with_borrow(|slot| {
            let Some(st) = slot.as_ref() else {
                return (None, Vec::new(), Vec::new());
            };
            let (cur_range, adj_ranges) =
                collect_page_and_adjacent_ranges(st.filtered.len(), st.page, PER_PAGE);
            CATALOG.with_borrow(|cat| {
                let vis: Vec<String> = st.filtered[cur_range]
                    .iter()
                    .filter_map(|&idx| cat.get(idx).map(|a| a.launch.clone()))
                    .collect();
                let mut adj = Vec::with_capacity(PER_PAGE * 2);
                for r in adj_ranges {
                    for &idx in &st.filtered[r] {
                        if let Some(a) = cat.get(idx) {
                            adj.push(a.launch.clone());
                        }
                    }
                }
                (Some(st.hwnd), vis, adj)
            })
        });
    if let Some(h) = hwnd_opt {
        queue_missing_icons(h, &visible_paths, &preload_paths);
    }

    STATE.with_borrow_mut(|slot| {
        let Some(st) = slot.as_mut() else { return };
        let dpi = GetDpiForWindow(st.hwnd).max(96) as f32 / 96.0;
        let w = st.width;
        let h = st.height;
        let m = st.margin as f32;
        let cr = d2d_rect(m, m, w as f32 - m, h as f32 - m);

        // 3 Independent Floating Liquid Glass Modules (Apple HIG: Avoid Glass-on-Glass!)
        let header_h = 44.0 * dpi;
        let mod_gap = 12.0 * dpi;
        let refresh_w = 66.0 * dpi;
        let sb_rect = d2d_rect(
            cr.left,
            cr.top,
            cr.right - refresh_w - 10.0 * dpi,
            cr.top + header_h,
        );
        let rf_rect = d2d_rect(cr.right - refresh_w, cr.top, cr.right, cr.top + header_h);
        let grid_card = d2d_rect(cr.left, cr.top + header_h + mod_gap, cr.right, cr.bottom);
        let sb_rad = header_h * 0.5;
        let grid_rad = 26.0 * dpi;

        st.module_rects = [sb_rect, rf_rect, grid_card];
        st.search_rect = sb_rect;

        let tone = st.tone;
        let is_light = tone.is_light(style);
        let txt_sh = !is_light;
        let ink_primary = if is_light {
            rgba(0.05, 0.07, 0.11, 0.95)
        } else {
            rgba(1.0, 1.0, 1.0, 0.98)
        };
        let ink_secondary = if is_light {
            rgba(0.09, 0.11, 0.17, 0.74)
        } else {
            rgba(1.0, 1.0, 1.0, 0.80)
        };
        let ink_muted = if is_light {
            rgba(0.11, 0.13, 0.19, 0.54)
        } else {
            rgba(1.0, 1.0, 1.0, 0.58)
        };
        let chip_bg = if is_light {
            rgba(0.0, 0.0, 0.0, 0.08)
        } else {
            rgba(1.0, 1.0, 1.0, 0.12)
        };
        let btn_hover_bg = if is_light {
            rgba(0.0, 0.0, 0.0, 0.16)
        } else {
            rgba(1.0, 1.0, 1.0, 0.24)
        };
        let accent = rgba(0.26, 0.64, 1.0, 0.98);

        let cur_shimmer = st.smooth_cursor.or(st.cursor_pos);
        let shimmer_for = |rect: D2D_RECT_F, boost: bool| -> Option<(f32, f32, f32)> {
            let (cx, cy) = cur_shimmer?;
            if pt_in(cx, cy, rect) {
                Some((cx, cy, if boost { 0.85 } else { 0.62 }))
            } else {
                None
            }
        };

        let mut tiles: Vec<ClickTile> = Vec::with_capacity(PER_PAGE);
        let mut dots: Vec<PageDot> = Vec::new();
        let mut prev_btn: Option<D2D_RECT_F> = None;
        let mut next_btn: Option<D2D_RECT_F> = None;
        let mut clear_btn: Option<D2D_RECT_F> = None;
        let refresh_btn = Some(rf_rect);

        let total_filtered = st.filtered.len();
        let total_pages = total_filtered.div_ceil(PER_PAGE).max(1);
        if st.page >= total_pages {
            st.page = total_pages - 1;
        }
        let start = st.page * PER_PAGE;
        let end = (start + PER_PAGE).min(total_filtered);

        let page_apps: Vec<(usize, String, String)> = CATALOG.with_borrow(|cat| {
            st.filtered[start..end]
                .iter()
                .filter_map(|&idx| {
                    cat.get(idx)
                        .map(|a| (idx, a.name.clone(), a.launch.clone()))
                })
                .collect()
        });

        let query = st.query.clone();
        let ime_comp = st.ime_comp.clone();
        let hovered_tile = st.hovered_tile;
        let pressed_tile = st.pressed_tile;
        let hovered_btn = st.hovered_btn;
        let selected = st.selected;
        let cur_page = st.page;
        let status_hint = st.status_hint.clone();
        let page_slide = st.page_slide;
        let tile_scales = st.tile_scales;

        if st.d2d_icons.len() > MAX_GPU_CACHE {
            let keep_gpu: HashSet<&str> = page_apps.iter().map(|(_, _, p)| p.as_str()).collect();
            st.d2d_icons.retain(|k, _| keep_gpu.contains(k.as_str()));
        }
        let d2d_icons = &mut st.d2d_icons;

        let Some(surf) = st.surface.as_mut() else {
            return;
        };
        // Sync all 3 live GPU Dual-Scale Chromatic Liquid Glass backdrops in exact lockstep!
        surf.main_backdrop.set_rect(
            grid_card.left,
            grid_card.top,
            grid_card.right - grid_card.left,
            grid_card.bottom - grid_card.top,
            grid_rad,
            1.0,
        );
        if let Some((sb_bg, _)) = surf.extra_backdrops.first() {
            sb_bg.set_rect(
                sb_rect.left,
                sb_rect.top,
                sb_rect.right - sb_rect.left,
                sb_rect.bottom - sb_rect.top,
                sb_rad,
                1.0,
            );
        }
        if let Some((rf_bg, _)) = surf.extra_backdrops.get(1) {
            rf_bg.set_rect(
                rf_rect.left,
                rf_rect.top,
                rf_rect.right - rf_rect.left,
                rf_rect.bottom - rf_rect.top,
                sb_rad,
                1.0,
            );
        }

        surf.draw(w, h, |ctx, _, _| {
            // Module 1: Floating Liquid Glass Search Bar Capsule (crystal-clear is_panel=false!)
            draw_liquid_glass_pill(
                ctx,
                sb_rect,
                sb_rad,
                style,
                tone,
                false,
                shimmer_for(sb_rect, false),
                true,
                1.0,
            );
            draw_search_icon(
                ctx,
                sb_rect.left + 21.0 * dpi,
                (sb_rect.top + sb_rect.bottom) * 0.5,
                15.5 * dpi,
                ink_secondary,
            );

            let has_text = !query.is_empty() || !ime_comp.is_empty();
            let count_txt = if !has_text {
                format!("{total_filtered} アプリ")
            } else {
                format!("{total_filtered} 件")
            };
            let right_reserve = if has_text { 112.0 * dpi } else { 86.0 * dpi };
            let text_box = d2d_rect(
                sb_rect.left + 40.0 * dpi,
                sb_rect.top,
                sb_rect.right - right_reserve,
                sb_rect.bottom,
            );
            if !has_text {
                draw_text(
                    ctx,
                    "アプリを検索…（日本語・英語どちらでも絞り込み）",
                    text_box,
                    13.5 * dpi,
                    TextWeight::Regular,
                    ink_muted,
                    TextAlign::Left,
                    txt_sh,
                );
            } else {
                let shown = if ime_comp.is_empty() {
                    format!("{query}│")
                } else {
                    format!("{query}{ime_comp}")
                };
                draw_text(
                    ctx,
                    &shown,
                    text_box,
                    14.5 * dpi,
                    TextWeight::Medium,
                    ink_primary,
                    TextAlign::Left,
                    txt_sh,
                );
                if !ime_comp.is_empty() {
                    let prefix_w = if query.is_empty() {
                        0.0
                    } else {
                        measure_text_width(&query, 14.5 * dpi, TextWeight::Medium)
                    };
                    let comp_w = measure_text_width(&ime_comp, 14.5 * dpi, TextWeight::Medium);
                    let ul_l = (text_box.left + prefix_w).min(text_box.right);
                    let ul_r = (ul_l + comp_w).min(text_box.right);
                    if ul_r > ul_l + 2.0 {
                        if let Some(ab) = solid(ctx, accent) {
                            ctx.FillRectangle(
                                &d2d_rect(
                                    ul_l,
                                    sb_rect.bottom - 9.0 * dpi,
                                    ul_r,
                                    sb_rect.bottom - 7.0 * dpi,
                                ),
                                &ab,
                            );
                        }
                    }
                }
                let cb = d2d_rect(
                    sb_rect.right - 102.0 * dpi,
                    sb_rect.top + 11.0 * dpi,
                    sb_rect.right - 80.0 * dpi,
                    sb_rect.bottom - 11.0 * dpi,
                );
                clear_btn = Some(cb);
                let cb_bg = if hovered_btn == Some(HoverBtn::Clear) {
                    btn_hover_bg
                } else {
                    chip_bg
                };
                if let Some(b) = solid(ctx, cb_bg) {
                    ctx.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: pt((cb.left + cb.right) * 0.5, (cb.top + cb.bottom) * 0.5),
                            radiusX: 10.5 * dpi,
                            radiusY: 10.5 * dpi,
                        },
                        &b,
                    );
                }
                draw_text(
                    ctx,
                    "×",
                    cb,
                    12.5 * dpi,
                    TextWeight::Bold,
                    ink_primary,
                    TextAlign::Center,
                    false,
                );
            }
            draw_text(
                ctx,
                &count_txt,
                d2d_rect(
                    sb_rect.right - 78.0 * dpi,
                    sb_rect.top,
                    sb_rect.right - 16.0 * dpi,
                    sb_rect.bottom,
                ),
                12.0 * dpi,
                TextWeight::Medium,
                ink_secondary,
                TextAlign::Right,
                txt_sh,
            );

            // Module 2: Floating Liquid Glass Refresh Pill
            draw_liquid_glass_pill(
                ctx,
                rf_rect,
                sb_rad,
                style,
                tone,
                false,
                shimmer_for(rf_rect, hovered_btn == Some(HoverBtn::Refresh)),
                true,
                1.0,
            );
            draw_text(
                ctx,
                "更新",
                rf_rect,
                12.5 * dpi,
                if hovered_btn == Some(HoverBtn::Refresh) {
                    TextWeight::SemiBold
                } else {
                    TextWeight::Medium
                },
                if hovered_btn == Some(HoverBtn::Refresh) {
                    ink_primary
                } else {
                    ink_secondary
                },
                TextAlign::Center,
                txt_sh,
            );

            // Module 3: Main Floating Liquid Glass Launchpad App Grid Surface
            draw_liquid_glass_pill(
                ctx,
                grid_card,
                grid_rad,
                style,
                tone,
                false,
                shimmer_for(grid_card, false),
                true,
                1.0,
            );

            let pad = 18.0 * dpi;
            let inner_l = grid_card.left + pad;
            let inner_r = grid_card.right - pad;
            let grid_t = grid_card.top + 14.0 * dpi;
            let grid_b = grid_card.bottom - 44.0 * dpi;
            let cell_w = (inner_r - inner_l) / COLS as f32;
            let cell_h = (grid_b - grid_t) / ROWS as f32;
            let icon_sz = (48.0 * dpi).min(cell_h * 0.52);

            let slide_px = page_slide * 115.0 * dpi;
            let grid_alpha = (1.0 - page_slide.abs() * 0.45).clamp(0.25, 1.0);

            if page_apps.is_empty() {
                draw_text(
                    ctx,
                    "一致するアプリが見つかりません",
                    d2d_rect(inner_l, grid_t, inner_r, grid_b),
                    14.5 * dpi,
                    TextWeight::Medium,
                    ink_secondary,
                    TextAlign::Center,
                    txt_sh,
                );
            } else {
                let clip_rect = d2d_rect(
                    grid_card.left + 8.0 * dpi,
                    grid_t - 4.0 * dpi,
                    grid_card.right - 8.0 * dpi,
                    grid_b + 4.0 * dpi,
                );
                ctx.PushAxisAlignedClip(&clip_rect, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

                for (slot_i, (app_idx, name, launch_path)) in page_apps.iter().enumerate() {
                    let col = slot_i % COLS;
                    let row = slot_i / COLS;
                    let base_tx = inner_l + col as f32 * cell_w + 4.0 * dpi;
                    let ty = grid_t + row as f32 * cell_h + 3.0 * dpi;
                    let hit_tr = d2d_rect(
                        base_tx,
                        ty,
                        base_tx + cell_w - 8.0 * dpi,
                        ty + cell_h - 6.0 * dpi,
                    );
                    let tr = d2d_rect(
                        hit_tr.left + slide_px,
                        hit_tr.top,
                        hit_tr.right + slide_px,
                        hit_tr.bottom,
                    );
                    let is_hov = hovered_tile == Some(slot_i);
                    let is_pressed = pressed_tile == Some(slot_i);
                    let is_sel = selected == slot_i && hovered_tile.is_none();
                    let t_scale = tile_scales[slot_i];

                    if is_hov || is_sel || is_pressed {
                        let rr = D2D1_ROUNDED_RECT {
                            rect: tr,
                            radiusX: 15.0 * dpi,
                            radiusY: 15.0 * dpi,
                        };
                        let (t_col, b_col, rim_col) = if is_light {
                            (
                                rgba(
                                    1.0,
                                    1.0,
                                    1.0,
                                    (if is_pressed {
                                        0.76
                                    } else if is_hov {
                                        0.56
                                    } else {
                                        0.38
                                    }) * grid_alpha,
                                ),
                                rgba(
                                    0.94,
                                    0.97,
                                    1.0,
                                    (if is_pressed {
                                        0.58
                                    } else if is_hov {
                                        0.38
                                    } else {
                                        0.24
                                    }) * grid_alpha,
                                ),
                                rgba(1.0, 1.0, 1.0, 0.72 * grid_alpha),
                            )
                        } else {
                            (
                                rgba(
                                    1.0,
                                    1.0,
                                    1.0,
                                    (if is_pressed {
                                        0.28
                                    } else if is_hov {
                                        0.22
                                    } else {
                                        0.14
                                    }) * grid_alpha,
                                ),
                                rgba(
                                    0.92,
                                    0.96,
                                    1.0,
                                    (if is_pressed {
                                        0.16
                                    } else if is_hov {
                                        0.11
                                    } else {
                                        0.06
                                    }) * grid_alpha,
                                ),
                                rgba(
                                    1.0,
                                    1.0,
                                    1.0,
                                    (if is_hov || is_pressed { 0.48 } else { 0.30 }) * grid_alpha,
                                ),
                            )
                        };
                        if let Some(g) = linear_grad(
                            ctx,
                            pt(tr.left, tr.top),
                            pt(tr.left, tr.bottom),
                            &[
                                D2D1_GRADIENT_STOP {
                                    position: 0.0,
                                    color: t_col,
                                },
                                D2D1_GRADIENT_STOP {
                                    position: 1.0,
                                    color: b_col,
                                },
                            ],
                        ) {
                            ctx.FillRoundedRectangle(&rr, &g);
                        }
                        if let Some(rim) = solid(ctx, rim_col) {
                            ctx.DrawRoundedRectangle(&rr, &rim, 0.95, None);
                        }
                    }

                    let draw_sz = icon_sz * t_scale;
                    let lift = (t_scale - 1.0).max(0.0) * 14.0 * dpi;
                    let icx = (tr.left + tr.right) * 0.5;
                    let icy = tr.top + 9.0 * dpi + icon_sz * 0.5 - lift;
                    let dest = d2d_rect(
                        icx - draw_sz * 0.5,
                        icy - draw_sz * 0.5,
                        icx + draw_sz * 0.5,
                        icy + draw_sz * 0.5,
                    );

                    // Luminous Liquid Glass hover halo behind scaled icon
                    if t_scale > 1.015 {
                        let halo_k = ((t_scale - 1.0) / 0.12).clamp(0.0, 1.0) * grid_alpha;
                        let hr = draw_sz * 0.66;
                        if let Some(halo) = radial_grad(
                            ctx,
                            pt(icx, icy),
                            hr,
                            hr,
                            &[
                                D2D1_GRADIENT_STOP {
                                    position: 0.0,
                                    color: rgba(1.0, 1.0, 1.0, 0.20 * halo_k),
                                },
                                D2D1_GRADIENT_STOP {
                                    position: 1.0,
                                    color: rgba(1.0, 1.0, 1.0, 0.0),
                                },
                            ],
                        ) {
                            ctx.FillEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(icx, icy),
                                    radiusX: hr,
                                    radiusY: hr,
                                },
                                &halo,
                            );
                        }
                    }

                    if !d2d_icons.contains_key(launch_path) {
                        let bmp_opt = ICON_CACHE.with_borrow(|cache| {
                            cache
                                .get(launch_path)
                                .and_then(|c| c.raw.as_ref())
                                .and_then(|raw| raw.to_d2d(ctx).map(|bmp| (bmp, raw.badge)))
                        });
                        if let Some(pair) = bmp_opt {
                            d2d_icons.insert(launch_path.clone(), pair);
                        }
                    }
                    if let Some((bmp, badge)) = d2d_icons.get(launch_path) {
                        draw_icon_with_badge(ctx, bmp, badge.as_ref(), dest, grid_alpha);
                    } else {
                        let rr = D2D1_ROUNDED_RECT {
                            rect: dest,
                            radiusX: draw_sz * 0.225,
                            radiusY: draw_sz * 0.225,
                        };
                        let hue_idx = name
                            .bytes()
                            .fold(0usize, |acc, b| acc.wrapping_add(b as usize))
                            % 6;
                        let pal = [
                            rgba(0.24, 0.54, 0.96, 0.80 * grid_alpha),
                            rgba(0.36, 0.46, 0.94, 0.80 * grid_alpha),
                            rgba(0.18, 0.68, 0.56, 0.80 * grid_alpha),
                            rgba(0.86, 0.44, 0.30, 0.80 * grid_alpha),
                            rgba(0.68, 0.38, 0.88, 0.80 * grid_alpha),
                            rgba(0.22, 0.64, 0.84, 0.80 * grid_alpha),
                        ][hue_idx];
                        if let Some(b) = solid(ctx, pal) {
                            ctx.FillRoundedRectangle(&rr, &b);
                        }
                        let ch: String = name.chars().take(1).collect();
                        draw_text(
                            ctx,
                            &ch,
                            dest,
                            draw_sz * 0.44,
                            TextWeight::Bold,
                            rgba(1.0, 1.0, 1.0, 0.95 * grid_alpha),
                            TextAlign::Center,
                            false,
                        );
                    }

                    let lbl_col = rgba(
                        ink_primary.r,
                        ink_primary.g,
                        ink_primary.b,
                        ink_primary.a * grid_alpha,
                    );
                    draw_text(
                        ctx,
                        name,
                        d2d_rect(
                            tr.left + 4.0 * dpi,
                            tr.top + icon_sz + 13.0 * dpi,
                            tr.right - 4.0 * dpi,
                            tr.bottom - 3.0 * dpi,
                        ),
                        11.5 * dpi,
                        if is_hov || is_sel {
                            TextWeight::SemiBold
                        } else {
                            TextWeight::Regular
                        },
                        lbl_col,
                        TextAlign::Center,
                        txt_sh,
                    );

                    tiles.push(ClickTile {
                        rect: hit_tr,
                        app_idx: *app_idx,
                    });
                }

                ctx.PopAxisAlignedClip();
            }

            // Bottom Bar inside Grid Glass Surface: Sliding Page Dots + Prev/Next + Hint
            let footer_y = grid_card.bottom - 40.0 * dpi;
            if total_pages > 1 {
                let max_dots = 9usize;
                let win_start = if total_pages <= max_dots {
                    0
                } else {
                    cur_page
                        .saturating_sub(max_dots / 2)
                        .min(total_pages - max_dots)
                };
                let win_end = (win_start + max_dots).min(total_pages);
                let shown = win_end - win_start;
                let dot_pitch = 15.0 * dpi;
                let dots_w = shown as f32 * dot_pitch;
                let nav_w = 24.0 * dpi;
                let mid_x = (grid_card.left + grid_card.right) * 0.5;
                let dcy = footer_y + 10.0 * dpi;

                let pr = d2d_rect(
                    mid_x - dots_w * 0.5 - nav_w - 6.0 * dpi,
                    dcy - 10.0 * dpi,
                    mid_x - dots_w * 0.5 - 6.0 * dpi,
                    dcy + 10.0 * dpi,
                );
                if cur_page > 0 {
                    prev_btn = Some(pr);
                    let bg_c = if hovered_btn == Some(HoverBtn::Prev) {
                        btn_hover_bg
                    } else {
                        chip_bg
                    };
                    if let Some(b) = solid(ctx, bg_c) {
                        ctx.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: pr,
                                radiusX: 10.0 * dpi,
                                radiusY: 10.0 * dpi,
                            },
                            &b,
                        );
                    }
                    draw_text(
                        ctx,
                        "‹",
                        pr,
                        13.0 * dpi,
                        TextWeight::Bold,
                        ink_primary,
                        TextAlign::Center,
                        false,
                    );
                }

                let start_x = mid_x - dots_w * 0.5;
                for (offset, p_idx) in (win_start..win_end).enumerate() {
                    let dcx = start_x + (offset as f32 + 0.5) * dot_pitch;
                    let active = p_idx == cur_page;
                    let hov_d = hovered_btn == Some(HoverBtn::Dot(p_idx));
                    let col = if active {
                        ink_primary
                    } else if hov_d {
                        ink_secondary
                    } else if is_light {
                        rgba(0.0, 0.0, 0.0, 0.22)
                    } else {
                        rgba(1.0, 1.0, 1.0, 0.34)
                    };
                    if let Some(b) = solid(ctx, col) {
                        let r = if active {
                            4.0 * dpi
                        } else if hov_d {
                            3.4 * dpi
                        } else {
                            2.8 * dpi
                        };
                        ctx.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(dcx, dcy),
                                radiusX: r,
                                radiusY: r,
                            },
                            &b,
                        );
                    }
                    dots.push(PageDot {
                        rect: d2d_rect(
                            dcx - 7.0 * dpi,
                            dcy - 8.0 * dpi,
                            dcx + 7.0 * dpi,
                            dcy + 8.0 * dpi,
                        ),
                        page: p_idx,
                    });
                }

                let nr = d2d_rect(
                    mid_x + dots_w * 0.5 + 6.0 * dpi,
                    dcy - 10.0 * dpi,
                    mid_x + dots_w * 0.5 + 6.0 * dpi + nav_w,
                    dcy + 10.0 * dpi,
                );
                if cur_page + 1 < total_pages {
                    next_btn = Some(nr);
                    let bg_c = if hovered_btn == Some(HoverBtn::Next) {
                        btn_hover_bg
                    } else {
                        chip_bg
                    };
                    if let Some(b) = solid(ctx, bg_c) {
                        ctx.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: nr,
                                radiusX: 10.0 * dpi,
                                radiusY: 10.0 * dpi,
                            },
                            &b,
                        );
                    }
                    draw_text(
                        ctx,
                        "›",
                        nr,
                        13.0 * dpi,
                        TextWeight::Bold,
                        ink_primary,
                        TextAlign::Center,
                        false,
                    );
                }

                let pg_txt = format!("{} / {}", cur_page + 1, total_pages);
                draw_text(
                    ctx,
                    &pg_txt,
                    d2d_rect(
                        inner_r - 72.0 * dpi,
                        dcy - 9.0 * dpi,
                        inner_r,
                        dcy + 9.0 * dpi,
                    ),
                    10.5 * dpi,
                    TextWeight::Medium,
                    ink_secondary,
                    TextAlign::Right,
                    txt_sh,
                );
            }
            draw_text(
                ctx,
                &status_hint,
                d2d_rect(
                    inner_l,
                    footer_y + 20.0 * dpi,
                    inner_r,
                    grid_card.bottom - 4.0 * dpi,
                ),
                10.5 * dpi,
                TextWeight::Regular,
                ink_muted,
                TextAlign::Center,
                txt_sh,
            );
        });

        st.tiles = tiles;
        st.dots = dots;
        st.prev_btn = prev_btn;
        st.next_btn = next_btn;
        st.clear_btn = clear_btn;
        st.refresh_btn = refresh_btn;
    });
}

unsafe fn launch_selected(h: HWND, app_idx: usize) {
    let info = STATE.with_borrow(|slot| {
        let st = slot.as_ref()?;
        let launch = CATALOG.with_borrow(|cat| cat.get(app_idx).map(|a| a.launch.clone()))?;
        Some((st.owner, launch))
    });
    if let Some((owner, path)) = info {
        close_animated();
        sync::launch(owner, &path);
        let _ = h;
    }
}

fn hit_hover_btn(st: &LauncherState, x: f32, y: f32) -> Option<HoverBtn> {
    if st.clear_btn.is_some_and(|r| pt_in(x, y, r)) {
        return Some(HoverBtn::Clear);
    }
    if st.refresh_btn.is_some_and(|r| pt_in(x, y, r)) {
        return Some(HoverBtn::Refresh);
    }
    if st.prev_btn.is_some_and(|r| pt_in(x, y, r)) {
        return Some(HoverBtn::Prev);
    }
    if st.next_btn.is_some_and(|r| pt_in(x, y, r)) {
        return Some(HoverBtn::Next);
    }
    if let Some(d) = st.dots.iter().find(|d| pt_in(x, y, d.rect)) {
        return Some(HoverBtn::Dot(d.page));
    }
    None
}

unsafe fn switch_page(h: HWND, new_page: usize) {
    let changed = STATE.with_borrow_mut(|slot| {
        let Some(st) = slot.as_mut() else {
            return false;
        };
        let total_pages = st.filtered.len().div_ceil(PER_PAGE).max(1);
        let target = new_page.min(total_pages - 1);
        if target == st.page {
            return false;
        }
        let dir = if target > st.page { 1.0 } else { -1.0 };
        st.page = target;
        st.page_slide = dir;
        st.selected = 0;
        st.hovered_tile = None;
        st.pressed_tile = None;
        st.tile_scales = [1.0; PER_PAGE];
        true
    });
    if changed {
        paint();
        ensure_animating(h);
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
        WM_TIMER if wp == ANIM_TIMER => {
            on_anim_timer(h);
        }
        WM_TIMER if wp == OUTSIDE_TIMER => {
            let down = (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0;
            let dismiss = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return false;
                };
                let fresh = down && !st.prev_mouse_down;
                st.prev_mouse_down = down;
                if fresh && !st.closing && st.opened_at.elapsed() >= Duration::from_millis(85) {
                    is_outside_click_target(h, &st.module_rects)
                } else {
                    false
                }
            });
            if dismiss {
                close_animated();
            }
        }
        WM_LAUNCHER_ICONS => {
            if drain_pending_icons() {
                paint();
            }
        }
        WM_LAUNCHER_CATALOG => {
            if drain_pending_catalog() {
                paint();
            }
        }
        WM_IME_STARTCOMPOSITION => {
            position_ime_window(h);
            return DefWindowProcW(h, msg, wp, lp);
        }
        WM_IME_COMPOSITION => {
            if (lp as u32) & GCS_COMPSTR != 0 {
                let comp = read_ime_comp_string(h);
                STATE.with_borrow_mut(|slot| {
                    if let Some(st) = slot.as_mut() {
                        st.ime_comp = comp;
                        rebuild_filter(st);
                    }
                });
                paint();
            }
            position_ime_window(h);
            return DefWindowProcW(h, msg, wp, lp);
        }
        WM_IME_ENDCOMPOSITION => {
            let changed = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return false;
                };
                if !st.ime_comp.is_empty() {
                    st.ime_comp.clear();
                    rebuild_filter(st);
                    true
                } else {
                    false
                }
            });
            if changed {
                paint();
            }
            return DefWindowProcW(h, msg, wp, lp);
        }
        WM_MOUSEMOVE => {
            let x = lp as i16 as i32 as f32;
            let y = (lp >> 16) as i16 as i32 as f32;
            let (changed_instant, need_anim, arm) = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return (false, false, false);
                };
                if st.closing {
                    return (false, false, false);
                }
                let arm = !st.tracking_leave;
                if arm {
                    st.tracking_leave = true;
                }
                st.cursor_pos = Some((x, y));
                let hit_tile = st.tiles.iter().position(|t| pt_in(x, y, t.rect));
                let hit_btn = hit_hover_btn(st, x, y);
                let btn_changed = st.hovered_btn != hit_btn;
                st.hovered_tile = hit_tile;
                st.hovered_btn = hit_btn;
                (btn_changed, true, arm)
            });
            if arm {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
            }
            if changed_instant {
                paint();
            }
            if need_anim {
                ensure_animating(h);
            }
        }
        0x02a3 => {
            let changed = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return false;
                };
                st.tracking_leave = false;
                let a = st.cursor_pos.take().is_some();
                let b = st.hovered_tile.take().is_some();
                let c = st.pressed_tile.take().is_some();
                let d = st.hovered_btn.take().is_some();
                a || b || c || d
            });
            if changed {
                ensure_animating(h);
            }
        }
        WM_MOUSEWHEEL => {
            let delta = (wp >> 16) as i16 as i32;
            let target_page = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return None;
                };
                if st.closing {
                    return None;
                }
                if (st.wheel_accum > 0 && delta < 0) || (st.wheel_accum < 0 && delta > 0) {
                    st.wheel_accum = 0;
                }
                st.wheel_accum += delta;
                let steps = st.wheel_accum / 120;
                if steps == 0 {
                    return None;
                }
                st.wheel_accum -= steps * 120;
                let total_pages = st.filtered.len().div_ceil(PER_PAGE).max(1);
                let next = if steps < 0 {
                    (st.page + (-steps) as usize).min(total_pages - 1)
                } else {
                    st.page.saturating_sub(steps as usize)
                };
                (next != st.page).then_some(next)
            });
            if let Some(p) = target_page {
                switch_page(h, p);
            }
        }
        WM_LBUTTONDOWN => {
            let x = lp as i16 as i32 as f32;
            let y = (lp >> 16) as i16 as i32 as f32;
            let changed = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return false;
                };
                if st.closing {
                    return false;
                }
                let hit = st.tiles.iter().position(|t| pt_in(x, y, t.rect));
                if st.pressed_tile != hit {
                    st.pressed_tile = hit;
                    true
                } else {
                    false
                }
            });
            if changed {
                ensure_animating(h);
            }
        }
        WM_LBUTTONUP => {
            let x = lp as i16 as i32 as f32;
            let y = (lp >> 16) as i16 as i32 as f32;

            enum ClickResult {
                Launch(usize),
                Clear,
                Refresh,
                Page(usize),
                Outside,
                None,
            }

            let (res, had_press) = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return (ClickResult::Outside, false);
                };
                if st.closing {
                    return (ClickResult::None, false);
                }
                let pressed = st.pressed_tile.take();
                let had_press = pressed.is_some();
                if st.clear_btn.is_some_and(|r| pt_in(x, y, r)) {
                    return (ClickResult::Clear, had_press);
                }
                if st.refresh_btn.is_some_and(|r| pt_in(x, y, r)) {
                    return (ClickResult::Refresh, had_press);
                }
                if st.prev_btn.is_some_and(|r| pt_in(x, y, r)) {
                    return (ClickResult::Page(st.page.saturating_sub(1)), had_press);
                }
                if st.next_btn.is_some_and(|r| pt_in(x, y, r)) {
                    let max_p = st.filtered.len().div_ceil(PER_PAGE).saturating_sub(1);
                    return (ClickResult::Page((st.page + 1).min(max_p)), had_press);
                }
                if let Some(d) = st.dots.iter().find(|d| pt_in(x, y, d.rect)) {
                    return (ClickResult::Page(d.page), had_press);
                }
                if let Some((slot_i, t)) = st
                    .tiles
                    .iter()
                    .enumerate()
                    .find(|(_, t)| pt_in(x, y, t.rect))
                {
                    if pressed.is_none() || pressed == Some(slot_i) {
                        return (ClickResult::Launch(t.app_idx), had_press);
                    }
                    return (ClickResult::None, had_press);
                }
                let inside_module = st.module_rects.iter().any(|r| pt_in(x, y, *r));
                if !inside_module {
                    (ClickResult::Outside, had_press)
                } else {
                    (ClickResult::None, had_press)
                }
            });

            match res {
                ClickResult::Launch(idx) => launch_selected(h, idx),
                ClickResult::Clear => {
                    STATE.with_borrow_mut(|slot| {
                        if let Some(st) = slot.as_mut() {
                            st.query.clear();
                            st.ime_comp.clear();
                            rebuild_filter(st);
                        }
                    });
                    paint();
                }
                ClickResult::Refresh => {
                    ICON_CACHE.with_borrow_mut(|c| c.clear());
                    ICON_INFLIGHT.with_borrow_mut(|s| s.clear());
                    STATE.with_borrow_mut(|slot| {
                        if let Some(st) = slot.as_mut() {
                            st.status_hint = "アプリ一覧を更新しています…".into();
                        }
                    });
                    paint();
                    spawn_catalog_scan(h, true);
                }
                ClickResult::Page(p) => switch_page(h, p),
                ClickResult::Outside => close_animated(),
                ClickResult::None => {
                    if had_press {
                        ensure_animating(h);
                    }
                }
            }
        }
        WM_RBUTTONUP => {
            let x = lp as i16 as i32 as f32;
            let y = (lp >> 16) as i16 as i32 as f32;
            let pin_info = STATE.with_borrow(|slot| {
                let st = slot.as_ref()?;
                if st.closing {
                    return None;
                }
                let tile = st.tiles.iter().find(|t| pt_in(x, y, t.rect))?;
                let app = CATALOG.with_borrow(|cat| cat.get(tile.app_idx).cloned())?;
                Some((st.owner, app))
            });
            if let Some((owner, app)) = pin_info {
                let added = MODEL.with_borrow_mut(|s| s.pin_named(&app.name, &app.launch));
                if added {
                    let _ = MODEL.with_borrow(|s| s.save());
                    surface::layout(owner);
                }
                STATE.with_borrow_mut(|slot| {
                    if let Some(st) = slot.as_mut() {
                        st.status_hint = if added {
                            format!("「{}」を Dock に追加しました", app.name)
                        } else {
                            format!("「{}」は追加済み、または上限（6個）です", app.name)
                        };
                    }
                });
                paint();
            }
        }
        WM_CHAR => {
            let ch_u32 = wp as u32;
            if ch_u32 == 0x08 {
                let changed = STATE.with_borrow_mut(|slot| {
                    let Some(st) = slot.as_mut() else {
                        return false;
                    };
                    if !st.closing && st.query.pop().is_some() {
                        rebuild_filter(st);
                        true
                    } else {
                        false
                    }
                });
                if changed {
                    paint();
                }
            } else if ch_u32 == 0x7F {
                let changed = STATE.with_borrow_mut(|slot| {
                    let Some(st) = slot.as_mut() else {
                        return false;
                    };
                    if !st.closing && !st.query.is_empty() {
                        st.query.clear();
                        rebuild_filter(st);
                        true
                    } else {
                        false
                    }
                });
                if changed {
                    paint();
                }
            } else if ch_u32 >= 0x20 && ch_u32 != 0x7F {
                if let Some(ch) = char::from_u32(ch_u32) {
                    if !ch.is_control() {
                        let changed = STATE.with_borrow_mut(|slot| {
                            if let Some(st) = slot.as_mut() {
                                if !st.closing && st.query.chars().count() < 40 {
                                    st.query.push(ch);
                                    rebuild_filter(st);
                                    return true;
                                }
                            }
                            false
                        });
                        if changed {
                            paint();
                        }
                    }
                }
            }
        }
        WM_KEYDOWN => {
            let (composing, closing) = STATE.with_borrow(|slot| {
                slot.as_ref()
                    .map(|st| (!st.ime_comp.is_empty(), st.closing))
                    .unwrap_or((false, false))
            });
            if closing {
                return 0;
            }
            if composing {
                return DefWindowProcW(h, msg, wp, lp);
            }
            let vk = wp as u16;
            if vk == VK_ESCAPE {
                let cleared = STATE.with_borrow_mut(|slot| {
                    let Some(st) = slot.as_mut() else {
                        return false;
                    };
                    if !st.query.is_empty() {
                        st.query.clear();
                        rebuild_filter(st);
                        true
                    } else {
                        false
                    }
                });
                if cleared {
                    paint();
                } else {
                    close_animated();
                }
                return 0;
            }
            if vk == VK_DELETE {
                let cleared = STATE.with_borrow_mut(|slot| {
                    let Some(st) = slot.as_mut() else {
                        return false;
                    };
                    if !st.query.is_empty() {
                        st.query.clear();
                        rebuild_filter(st);
                        true
                    } else {
                        false
                    }
                });
                if cleared {
                    paint();
                }
                return 0;
            }
            if vk == VK_RETURN {
                let target = STATE.with_borrow(|slot| {
                    let st = slot.as_ref()?;
                    let idx = st.page * PER_PAGE + st.selected;
                    st.filtered.get(idx).copied()
                });
                if let Some(app_idx) = target {
                    launch_selected(h, app_idx);
                }
                return 0;
            }
            let shift_down = (GetKeyState(VK_SHIFT as i32) as u16 & 0x8000) != 0;
            let (changed, slided) = STATE.with_borrow_mut(|slot| {
                let Some(st) = slot.as_mut() else {
                    return (false, false);
                };
                let total = st.filtered.len();
                if total == 0 {
                    return (false, false);
                }
                let total_pages = total.div_ceil(PER_PAGE).max(1);
                let page_count = ((st.page + 1) * PER_PAGE).min(total) - st.page * PER_PAGE;
                let prev_sel = st.selected;
                let prev_page = st.page;
                match vk {
                    VK_RIGHT | VK_TAB if vk == VK_RIGHT || !shift_down => {
                        if st.selected + 1 < page_count {
                            st.selected += 1;
                        } else if st.page + 1 < total_pages {
                            st.page += 1;
                            st.selected = 0;
                        }
                    }
                    VK_LEFT | VK_TAB => {
                        if st.selected > 0 {
                            st.selected -= 1;
                        } else if st.page > 0 {
                            st.page -= 1;
                            st.selected = PER_PAGE - 1;
                        }
                    }
                    VK_DOWN => {
                        if st.selected + COLS < page_count {
                            st.selected += COLS;
                        }
                    }
                    VK_UP => {
                        if st.selected >= COLS {
                            st.selected -= COLS;
                        }
                    }
                    VK_NEXT => {
                        if st.page + 1 < total_pages {
                            st.page += 1;
                            st.selected = 0;
                        }
                    }
                    VK_PRIOR => {
                        if st.page > 0 {
                            st.page -= 1;
                            st.selected = 0;
                        }
                    }
                    VK_HOME => {
                        st.page = 0;
                        st.selected = 0;
                    }
                    VK_END => {
                        st.page = total_pages - 1;
                        let last_count = total - st.page * PER_PAGE;
                        st.selected = last_count.saturating_sub(1);
                    }
                    _ => return (false, false),
                }
                let page_moved = st.page != prev_page;
                if page_moved {
                    st.page_slide = if st.page > prev_page { 1.0 } else { -1.0 };
                    st.tile_scales = [1.0; PER_PAGE];
                }
                st.hovered_tile = None;
                st.pressed_tile = None;
                (st.selected != prev_sel || page_moved, page_moved)
            });
            if changed {
                paint();
                if slided {
                    ensure_animating(h);
                }
            }
        }
        WM_ACTIVATE if wp as u16 == WA_INACTIVE as u16 => {
            let should_close = STATE.with_borrow(|slot| {
                slot.as_ref().is_none_or(|st| {
                    !st.closing && st.opened_at.elapsed() >= Duration::from_millis(85)
                })
            });
            if should_close {
                close_animated();
            }
        }
        WM_DESTROY => {
            KillTimer(h, ANIM_TIMER);
            KillTimer(h, OUTSIDE_TIMER);
            ICON_GEN.fetch_add(1, Ordering::SeqCst);
            HANDLE.set(null_mut());
            STATE.with_borrow_mut(|slot| *slot = None);
            ICON_INFLIGHT.with_borrow_mut(|s| s.clear());
            trim_icon_cache(40);
            system::trim_working_set();
            bar::repaint();
            surface::poke_autohide();
        }
        _ => return DefWindowProcW(h, msg, wp, lp),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_discovers_installed_apps() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
            let apps = discover_apps();
            println!("Discovered {} launcher apps", apps.len());
            assert!(
                !apps.is_empty(),
                "Expected at least one installed Windows app"
            );
            assert!(
                apps.iter()
                    .all(|a| !a.name.is_empty() && !a.launch.is_empty()),
                "All discovered apps must have non-empty name and launch path"
            );
            let mut names = HashSet::new();
            for a in &apps {
                assert!(names.insert(a.lower.clone()), "Duplicate app: {}", a.name);
                assert!(!is_clutter_name(&a.name), "Clutter leaked: {}", a.name);
            }
            CoUninitialize();
        }
    }

    #[test]
    fn launcher_search_ranking_and_aliases_work() {
        let edge = AppEntry::new("Microsoft Edge".into(), "C:\\msedge.exe".into());
        let editor = AppEntry::new("3D Model Editor".into(), "C:\\edit.exe".into());
        let vscode = AppEntry::new("Visual Studio Code".into(), "C:\\Code.exe".into());
        let calc = AppEntry::new("電卓".into(), "shell:AppsFolder\\Calc".into());

        let s_edge = match_score(&edge, "edge").unwrap();
        assert_eq!(s_edge, 2);
        assert!(match_score(&editor, "edge").is_none());

        assert_eq!(match_score(&vscode, "visual studio code"), Some(0));
        assert_eq!(match_score(&vscode, "vis"), Some(1));
        assert_eq!(match_score(&vscode, "code"), Some(2));
        assert_eq!(match_score(&vscode, "vsc"), Some(4));
        assert_eq!(match_score(&calc, "calc"), Some(3));
    }

    #[test]
    fn adjacent_page_preload_ranges_prioritize_next_then_prev() {
        // Page 0 of 3 (52 items, 20/page): current 0..20, preload next 20..40 only
        let (cur0, adj0) = collect_page_and_adjacent_ranges(52, 0, 20);
        assert_eq!(cur0, 0..20);
        assert_eq!(adj0, vec![20..40]);

        // Page 1 of 3: current 20..40, preload next 40..52 then prev 0..20
        let (cur1, adj1) = collect_page_and_adjacent_ranges(52, 1, 20);
        assert_eq!(cur1, 20..40);
        assert_eq!(adj1, vec![40..52, 0..20]);

        // Last page (Page 2 of 3): current 40..52, preload prev 20..40 only
        let (cur2, adj2) = collect_page_and_adjacent_ranges(52, 2, 20);
        assert_eq!(cur2, 40..52);
        assert_eq!(adj2, vec![20..40]);

        // Single page: no preload needed
        let (cur_s, adj_s) = collect_page_and_adjacent_ranges(12, 0, 20);
        assert_eq!(cur_s, 0..12);
        assert!(adj_s.is_empty());
    }
}
