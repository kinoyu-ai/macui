use super::*;
use std::path::{Path, PathBuf};
use windows::{
    core::{Interface, PCWSTR},
    Win32::{
        Foundation::{HWND as WH, SIZE},
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::Com::*,
        UI::{
            Accessibility::*,
            Shell::{
                IShellItemImageFactory, IShellLinkW,
                PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
                SHCreateItemFromParsingName, ShellLink, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT,
            },
        },
    },
};
use windows_sys::Win32::{Graphics::Dwm::*, Storage::Packaging::Appx::GetApplicationUserModelId};

#[derive(Clone, PartialEq)]
pub struct App {
    pub id: String,
    pub name: String,
    pub launch: String,
    pub image: String,
    pub windows: Vec<(usize, String)>,
    pub running: bool,
}
#[derive(Default, PartialEq, Clone)]
pub struct Status {
    pub network: String,
    pub volume: String,
    pub notifications: String,
    pub input: String,
}
#[derive(Default)]
pub struct Snapshot {
    pub apps: Vec<App>,
    pub status: Status,
}
struct Window {
    h: usize,
    pid: u32,
    title: String,
    path: String,
    app: String,
}
unsafe extern "system" fn enumerate(h: HWND, data: LPARAM) -> i32 {
    if IsWindowVisible(h) == 0 || GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0 {
        return 1;
    }
    // Only skip owned windows whose owner is ITSELF visible (child dialogs/tool popups).
    // Apps like Steam (SDL/CEF), Discord, Qt, and tray-resident apps often use an invisible
    // hidden message-window owner — Windows Taskbar still shows those as top-level apps!
    let owner = GetWindow(h, GW_OWNER);
    if !owner.is_null()
        && IsWindowVisible(owner) != 0
        && GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_APPWINDOW == 0
    {
        return 1;
    }
    let mut cloaked = 0u32;
    DwmGetWindowAttribute(
        h,
        DWMWA_CLOAKED as u32,
        (&mut cloaked as *mut u32).cast(),
        4,
    );
    if cloaked != 0 {
        return 1;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(h, &mut pid);
    if pid == std::process::id() || h == GetShellWindow() {
        return 1;
    }
    let mut title = [0; 1024];
    let n = GetWindowTextW(h, title.as_mut_ptr(), 1024);
    if n == 0 {
        return 1;
    }
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    let (mut path, mut app) = (String::new(), String::new());
    if !process.is_null() {
        let mut buf = [0; 32768];
        let mut len = buf.len() as u32;
        if QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) != 0 {
            path = String::from_utf16_lossy(&buf[..len as usize]);
        }
        len = buf.len() as u32;
        if GetApplicationUserModelId(process, &mut len, buf.as_mut_ptr()) == 0 && len > 0 {
            app = String::from_utf16_lossy(&buf[..len as usize - 1]);
        }
        CloseHandle(process);
    }
    if let Ok(store) = SHGetPropertyStoreForWindow::<_, IPropertyStore>(WH(h)) {
        if let Ok(v) = store.GetValue(&PKEY_AppUserModel_ID) {
            if let Ok(id) = windows::core::BSTR::try_from(&v) {
                if !id.is_empty() {
                    app = id.to_string();
                }
            }
        }
    }
    (*(data as *mut Vec<Window>)).push(Window {
        h: h as usize,
        pid,
        title: String::from_utf16_lossy(&title[..n as usize]),
        path,
        app,
    });
    1
}
unsafe fn expanded(id: &str) -> String {
    if let Some((guid, tail)) = id.split_once("}\\") {
        if guid.len() == 37
            && guid.starts_with('{')
            && guid[1..].bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
        {
            let g = windows::core::GUID::from(&guid[1..]);
            let mut p = null_mut();
            if SHGetKnownFolderPath(
                (&g as *const windows::core::GUID).cast(),
                0,
                null_mut(),
                &mut p,
            ) >= 0
            {
                let dir = windows::core::PWSTR(p).to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.cast()));
                return format!("{dir}\\{tail}");
            }
        }
    }
    id.to_owned()
}
fn name_status(text: &str) -> (String, bool) {
    let mut label = text
        .trim_end_matches(" 固定済み")
        .trim_end_matches("（ピン留め済み）")
        .trim_end_matches(" - Pinned")
        .trim_end_matches(" Pinned")
        .trim_end_matches(", pinned");
    let mut running = false;
    if let Some((left, right)) = label.rsplit_once(" - ") {
        if right.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            label = left;
            running = true
        }
    }
    (label.to_owned(), running)
}
fn stem(p: &str) -> &str {
    Path::new(p)
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("")
}
fn link_dir() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar"),
    )
}

static START_LINKS_CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<PathBuf>)>> =
    std::sync::Mutex::new(None);
const START_LINKS_TTL: std::time::Duration = std::time::Duration::from_secs(300);

pub fn invalidate_start_links_cache() {
    if let Ok(mut guard) = START_LINKS_CACHE.lock() {
        *guard = None;
    }
}

fn start_menu_links() -> Vec<PathBuf> {
    if let Ok(guard) = START_LINKS_CACHE.lock() {
        if let Some((ts, cached)) = guard.as_ref() {
            if ts.elapsed() < START_LINKS_TTL {
                return cached.clone();
            }
        }
    }
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 3 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, depth + 1, out);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                let s = stem(&p.to_string_lossy()).to_lowercase();
                if !s.contains("uninstall") && !s.contains("アンインストール") {
                    out.push(p);
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        walk(
            &PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs"),
            0,
            &mut out,
        );
    }
    if let Some(progdata) = std::env::var_os("ProgramData") {
        walk(
            &PathBuf::from(progdata).join(r"Microsoft\Windows\Start Menu\Programs"),
            0,
            &mut out,
        );
    }
    if let Ok(mut guard) = START_LINKS_CACHE.lock() {
        *guard = Some((std::time::Instant::now(), out.clone()));
    }
    out
}

/// Normalizes shortcut/app names so "LINE (2)" matches "LINE", "SteelSeries GG Client" matches
/// "SteelSeries GG", and "T3 Code (Beta)" matches cleanly.
fn normalize_app_label(s: &str) -> String {
    let mut t = s.trim().to_lowercase();
    for suffix in [" (2)", " (3)", " (4)", " (beta)", " client", " launcher"] {
        if let Some(stripped) = t.strip_suffix(suffix) {
            if !stripped.trim().is_empty() {
                t = stripped.trim().to_owned();
            }
        }
    }
    t.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '_'))
        .collect()
}

fn find_matching_link<'a>(links: &'a [PathBuf], name: &str, id: &str) -> Option<&'a PathBuf> {
    if let Some(p) = links
        .iter()
        .find(|p| stem(&p.to_string_lossy()).eq_ignore_ascii_case(name))
    {
        return Some(p);
    }
    let norm_name = normalize_app_label(name);
    if !norm_name.is_empty() {
        if let Some(p) = links
            .iter()
            .find(|p| normalize_app_label(stem(&p.to_string_lossy())) == norm_name)
        {
            return Some(p);
        }
    }
    let id_tail = id
        .rsplit(['\\', '.'])
        .next()
        .map(|s| s.trim_end_matches(".exe").trim_end_matches(".EXE"))
        .unwrap_or("");
    let norm_tail = normalize_app_label(id_tail);
    if norm_tail.len() >= 3 {
        if let Some(p) = links
            .iter()
            .find(|p| normalize_app_label(stem(&p.to_string_lossy())) == norm_tail)
        {
            return Some(p);
        }
    }
    None
}

fn is_shared_root_dir(dir: &Path) -> bool {
    let s = dir.to_string_lossy().to_lowercase();
    let trimmed = s.trim_end_matches('\\');
    trimmed.len() <= 3
        || trimmed.ends_with("\\windows")
        || trimmed.ends_with("\\windows\\system32")
        || trimmed.ends_with("\\windows\\syswow64")
        || trimmed.ends_with("\\program files")
        || trimmed.ends_with("\\program files (x86)")
        || trimmed.ends_with("\\appdata\\local")
        || trimmed.ends_with("\\appdata\\local\\programs")
        || trimmed.ends_with("\\appdata\\roaming")
        || trimmed.ends_with("\\programdata")
}

/// Computes an application's dedicated install root folder (e.g. `C:\Program Files (x86)\Steam`
/// or `C:\Users\...\AppData\Local\Discord`) so multi-process CEF/Electron/Squirrel helpers like
/// `steamwebhelper.exe`, `EpicWebHelper.exe`, and `app-1.0.x\Discord.exe` group cleanly with their app.
pub fn app_install_root(exe_or_path: &str) -> Option<String> {
    let p = Path::new(exe_or_path);
    let mut cur = p.parent()?;
    if is_shared_root_dir(cur) {
        return None;
    }
    // Walk up past nested helper/version subdirs (`bin`, `cef`, `cef.win64`, `win64`, `app-1.x`, `current`)
    while let Some(parent) = cur.parent() {
        if is_shared_root_dir(parent) {
            break;
        }
        let seg = cur
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();
        let nested_runtime = matches!(
            seg.as_str(),
            "bin"
                | "binaries"
                | "cef"
                | "cef.win64"
                | "cef.win32"
                | "win64"
                | "win32"
                | "portal"
                | "engine"
                | "current"
                | "application"
                | "resources"
        ) || seg.starts_with("app-");
        if nested_runtime {
            cur = parent;
        } else {
            break;
        }
    }
    if is_shared_root_dir(cur) {
        return None;
    }
    Some(cur.to_string_lossy().to_lowercase())
}

pub fn same_app_install_tree(path_a: &str, path_b: &str) -> bool {
    let (Some(ra), Some(rb)) = (app_install_root(path_a), app_install_root(path_b)) else {
        return false;
    };
    ra == rb
}

/// Redirects headless CEF/Electron helper subprocesses (e.g. `steamwebhelper.exe`, `EpicWebHelper.exe`)
/// back to their real parent launcher executable so clicking them never runs a dead subprocess!
pub fn resolve_helper_to_launcher(path: &str) -> String {
    let lower = path.to_lowercase();
    let file = Path::new(&lower)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let candidate_names: &[&str] = match file {
        "steamwebhelper.exe" | "steamservice.exe" => &["steam.exe"],
        "epicwebhelper.exe" => &["EpicGamesLauncher.exe"],
        "linecall.exe" | "linemediaplayer.exe" => &["LineLauncher.exe", "LINE.exe"],
        _ => return path.to_owned(),
    };
    let mut dir = Path::new(path).parent();
    for _ in 0..6 {
        let Some(d) = dir else { break };
        if is_shared_root_dir(d) {
            break;
        }
        for cand in candidate_names {
            let direct = d.join(cand);
            if direct.is_file() {
                return direct.to_string_lossy().into_owned();
            }
            let portal_bin = d.join(r"Portal\Binaries\Win64").join(cand);
            if portal_bin.is_file() {
                return portal_bin.to_string_lossy().into_owned();
            }
        }
        dir = d.parent();
    }
    path.to_owned()
}

/// Checks if a launch path / AppID / name represents Steam (which often runs `-silent` in tray
/// and responds reliably to `steam://open/main` to open and focus its main window).
pub fn is_steam_target(path_or_id: &str) -> bool {
    let lower = path_or_id.to_lowercase();
    let fname = Path::new(&lower)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    matches!(fname, "steam.exe" | "steam.lnk" | "steamwebhelper.exe")
        || lower == "valve.steam.client"
        || lower.ends_with("\\steam\\steam.exe")
}

/// Finds a Start Menu `.lnk` fallback matching a broken `shell:AppsFolder\<id>` or app label.
pub fn find_start_menu_shortcut(name_or_id: &str) -> Option<String> {
    let links = start_menu_links();
    let trimmed = name_or_id
        .strip_prefix("shell:AppsFolder\\")
        .unwrap_or(name_or_id);
    find_matching_link(&links, trimmed, trimmed).map(|p| p.to_string_lossy().into_owned())
}

pub unsafe fn read(uia: &IUIAutomation) -> windows::core::Result<Snapshot> {
    let mut rows = Vec::new();
    let mut status = Status::default();
    let bar = FindWindowW(w("Shell_TrayWnd").as_ptr(), null());
    if !bar.is_null() {
        let _: windows::core::Result<()> = (|| {
            let element = uia.ElementFromHandle(WH(bar))?;
            let cache = uia.CreateCacheRequest()?;
            cache.AddProperty(UIA_NamePropertyId)?;
            cache.AddProperty(UIA_AutomationIdPropertyId)?;
            let cond = (|| -> windows::core::Result<IUIAutomationCondition> {
                let btn = uia.CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &windows::core::VARIANT::from(UIA_ButtonControlTypeId.0),
                )?;
                let tray = uia.CreatePropertyCondition(
                    UIA_AutomationIdPropertyId,
                    &windows::core::VARIANT::from("SystemTrayIcon"),
                )?;
                uia.CreateOrCondition(&btn, &tray)
            })()
            .or_else(|_| uia.CreateTrueCondition())?;
            let els = element.FindAllBuildCache(TreeScope_Descendants, &cond, &cache)?;
            for i in 0..els.Length()? {
                let e = els.GetElement(i)?;
                let id = e.CachedAutomationId()?.to_string();
                let text = e.CachedName()?.to_string();
                if let Some(key) = id.strip_prefix("Appid: ") {
                    rows.push((key.to_owned(), name_status(&text)));
                    continue;
                }
                if id != "SystemTrayIcon" {
                    continue;
                }
                let line = text.lines().next().unwrap_or("").trim();
                let lower = line.to_lowercase();
                if lower.contains("ネットワーク") || lower.starts_with("network") {
                    status.network = line
                        .trim_start_matches("ネットワーク")
                        .trim_start_matches("Network")
                        .trim()
                        .to_owned();
                }
                if lower.contains("ボリューム")
                    || lower.contains("volume")
                    || lower.contains("音量")
                {
                    status.volume = if lower.contains("mute") || lower.contains("ミュート") {
                        "ミュート".into()
                    } else {
                        line.split_whitespace().last().unwrap_or("").to_owned()
                    };
                }
                if lower.starts_with("通知") || lower.starts_with("notification") {
                    status.notifications = line
                        .split(|c: char| !c.is_ascii_digit())
                        .find(|s| !s.is_empty())
                        .unwrap_or("0")
                        .into();
                }
                if lower.contains("トレイ入力インジケーター") || lower.contains("input indicator")
                {
                    let trimmed = line.trim_start_matches("トレイ入力インジケーター").trim();
                    if !trimmed.is_empty()
                        && (status.input.is_empty()
                            || trimmed.contains("ひらがな")
                            || trimmed.contains("半角"))
                    {
                        status.input = trimmed.to_owned();
                    }
                }
            }
            Ok(())
        })();
    }
    let mut wins: Vec<Window> = Vec::new();
    EnumWindows(Some(enumerate), (&mut wins as *mut Vec<Window>) as isize);
    let links: Vec<_> = link_dir()
        .and_then(|p| p.read_dir().ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|v| v.path())
        .filter(|p| p.extension().is_some_and(|v| v.eq_ignore_ascii_case("lnk")))
        .collect();
    if rows.is_empty() {
        for link in &links {
            let path = link.to_string_lossy().into_owned();
            rows.push((path.clone(), (stem(&path).into(), false)));
        }
        for win in &wins {
            if !win.path.is_empty()
                && !rows
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case(&win.path))
            {
                rows.push((win.path.clone(), (stem(&win.path).into(), true)));
            }
        }
    }
    let start_links = start_menu_links();
    let identities: std::collections::HashSet<_> =
        rows.iter().map(|(id, _)| id.to_lowercase()).collect();
    let mut apps = Vec::new();
    for (id, (name, running)) in rows {
        let path = expanded(&id);
        let pid = id
            .strip_prefix("*PID")
            .and_then(|v| u32::from_str_radix(v, 16).ok());
        let label = name.to_lowercase();
        let norm_label = normalize_app_label(&name);
        let matches = |win: &Window| {
            let win_app_lower = win.app.to_lowercase();
            if !win_app_lower.is_empty() && identities.contains(&win_app_lower) {
                return win.app.eq_ignore_ascii_case(&id);
            }
            (!win.path.is_empty()
                && id
                    .rsplit('.')
                    .next()
                    .is_some_and(|v| v.eq_ignore_ascii_case(stem(&win.path))))
                || Some(win.pid) == pid
                || (!win.path.is_empty() && win.path.eq_ignore_ascii_case(&path))
                || (!win.path.is_empty() && same_app_install_tree(&win.path, &path))
                || (id == "Microsoft.Windows.Explorer"
                    && stem(&win.path).eq_ignore_ascii_case("explorer"))
                || (id == "Chrome"
                    && win.app.is_empty()
                    && stem(&win.path).eq_ignore_ascii_case("chrome"))
                || (is_steam_target(&id)
                    && (stem(&win.path).eq_ignore_ascii_case("steam")
                        || stem(&win.path).eq_ignore_ascii_case("steamwebhelper")))
                || (!win.title.is_empty()
                    && (win.title.eq_ignore_ascii_case(&name)
                        || win.title.to_lowercase().ends_with(&format!(" - {label}"))))
                || (win.app.is_empty()
                    && (stem(&win.path).eq_ignore_ascii_case(&name)
                        || (!norm_label.is_empty()
                            && normalize_app_label(stem(&win.path)) == norm_label)))
        };
        let group: Vec<_> = wins.iter().filter(|v| matches(v)).collect();
        let link = find_matching_link(&links, &name, &id)
            .or_else(|| find_matching_link(&start_links, &name, &id));
        let executable = group.first().map(|v| v.path.as_str()).unwrap_or("");
        let safe_path = resolve_helper_to_launcher(&path);
        let safe_exec = resolve_helper_to_launcher(executable);
        let launch = if let Some(p) = link {
            // Prefer exact Taskbar / Start Menu .lnk first (preserves CLI args like --app-id, --processStart, -dataPath & WorkingDir!)
            p.to_string_lossy().into_owned()
        } else if Path::new(&safe_path).is_file() {
            safe_path.clone()
        } else if id == "Microsoft.Windows.Explorer" {
            std::env::var("SystemRoot")
                .map(|p| format!("{p}\\explorer.exe"))
                .unwrap_or_else(|_| "explorer.exe".into())
        } else if !safe_exec.is_empty() && Path::new(&safe_exec).is_file() {
            safe_exec.clone()
        } else {
            format!("shell:AppsFolder\\{id}")
        };
        let image = if !safe_exec.is_empty()
            && !id.contains('!')
            && link.is_none()
            && Path::new(&safe_exec).is_file()
        {
            safe_exec
        } else {
            launch.clone()
        };
        apps.push(App {
            id,
            name,
            launch,
            image,
            windows: group.iter().map(|v| (v.h, v.title.clone())).collect(),
            running: running || !group.is_empty(),
        });
    }
    Ok(Snapshot { apps, status })
}

/// Invokes a real Windows 11 Taskbar button (`Appid: <app_id>`) natively via UI Automation.
/// Guarantees activation/launch even for elevated apps, CEF/tray-resident apps, and custom AUMIDs!
pub unsafe fn invoke_taskbar_app(uia: &IUIAutomation, app_id: &str) -> bool {
    let h = FindWindowW(w("Shell_TrayWnd").as_ptr(), null());
    if h.is_null() || app_id.is_empty() {
        return false;
    }
    let Ok(root) = uia.ElementFromHandle(WH(h)) else {
        return false;
    };
    let target_id = format!("Appid: {app_id}");
    let Ok(cond) = uia.CreatePropertyCondition(
        UIA_AutomationIdPropertyId,
        &windows::core::VARIANT::from(target_id.as_str()),
    ) else {
        return false;
    };
    let Ok(el) = root.FindFirst(TreeScope_Descendants, &cond) else {
        return false;
    };
    AllowSetForegroundWindow(ASFW_ANY);
    if let Ok(inv) = el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
        if inv.Invoke().is_ok() {
            return true;
        }
    }
    if let Ok(leg) = el.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
        UIA_LegacyIAccessiblePatternId,
    ) {
        if leg.DoDefaultAction().is_ok() {
            return true;
        }
    }
    false
}

pub fn is_overflow_chevron(name: &str, auto_id: &str, class_name: &str) -> bool {
    let n = name.to_lowercase();
    let id = auto_id.to_lowercase();
    let cls = class_name.to_lowercase();
    if id.contains("chevron")
        || id.contains("overflow")
        || id.contains("showhiddenicons")
        || cls.contains("chevron")
        || cls.contains("overflow")
    {
        return true;
    }
    n.contains("hidden icons")
        || n.contains("show hidden")
        || n.contains("system tray overflow")
        || n.contains("非表示のアイコン")
        || n.contains("隠れているインジケーター")
        || n.contains("オーバーフロー")
}

pub unsafe fn overflow(uia: &IUIAutomation) -> windows::core::Result<()> {
    let h = FindWindowW(w("Shell_TrayWnd").as_ptr(), null());
    if h.is_null() {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
            0x80070490u32 as i32,
        )));
    }
    let root = uia.ElementFromHandle(WH(h))?;
    let cache = uia.CreateCacheRequest()?;
    cache.AddProperty(UIA_NamePropertyId)?;
    cache.AddProperty(UIA_AutomationIdPropertyId)?;
    cache.AddProperty(UIA_ClassNamePropertyId)?;
    let elements =
        root.FindAllBuildCache(TreeScope_Descendants, &uia.CreateTrueCondition()?, &cache)?;
    for i in 0..elements.Length()? {
        let e = elements.GetElement(i)?;
        let name = e.CachedName().map(|b| b.to_string()).unwrap_or_default();
        let auto_id = e
            .CachedAutomationId()
            .map(|b| b.to_string())
            .unwrap_or_default();
        let class_name = e
            .CachedClassName()
            .map(|b| b.to_string())
            .unwrap_or_default();
        if !is_overflow_chevron(&name, &auto_id, &class_name) {
            continue;
        }
        if let Ok(inv) = e.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
            if inv.Invoke().is_ok() {
                return Ok(());
            }
        }
        if let Ok(tog) = e.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
            if tog.Toggle().is_ok() {
                return Ok(());
            }
        }
        if let Ok(leg) = e.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
            UIA_LegacyIAccessiblePatternId,
        ) {
            if leg.DoDefaultAction().is_ok() {
                return Ok(());
            }
        }
    }
    Err(windows::core::Error::from_hresult(windows::core::HRESULT(
        0x80070490u32 as i32,
    )))
}
pub unsafe fn automation() -> windows::core::Result<IUIAutomation> {
    CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
}
/// Exact-fit icon fetch for the App Launcher & Dock (avoids decoding 256-512px Appx atlases).
pub unsafe fn bitmap_fit(path: &str, size: i32) -> HBITMAP {
    let src = w(path);
    let item = SHCreateItemFromParsingName::<_, _, windows::Win32::UI::Shell::IShellItem>(
        PCWSTR(src.as_ptr()),
        None,
    )
    .and_then(|item| item.cast::<IShellItemImageFactory>());
    let Ok(factory) = item else {
        return null_mut();
    };
    let sz = SIZE { cx: size, cy: size };
    factory
        .GetImage(sz, SIIGBF_ICONONLY | SIIGBF_RESIZETOFIT)
        .or_else(|_| factory.GetImage(sz, SIIGBF_ICONONLY))
        .map(|v| v.0)
        .unwrap_or(null_mut())
}

unsafe fn expand_env_string(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_owned();
    }
    let wide = w(raw);
    let mut buf = [0u16; 1024];
    let n = windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW(
        wide.as_ptr(),
        buf.as_mut_ptr(),
        buf.len() as u32,
    );
    if n > 0 && (n as usize) <= buf.len() {
        String::from_utf16_lossy(&buf[..(n as usize - 1)])
    } else {
        raw.to_owned()
    }
}

/// Resolves a `.lnk` shortcut's custom icon resource (`GetIconLocation`) and target path (`GetPath`)
/// so icons can be extracted at full high resolution WITHOUT Windows's shortcut arrow overlay!
pub unsafe fn resolve_lnk_icon_source(lnk_path: &str) -> (Option<(String, i32)>, Option<String>) {
    let Ok(link): windows::core::Result<IShellLinkW> =
        CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
    else {
        return (None, None);
    };
    let Ok(persist) = link.cast::<IPersistFile>() else {
        return (None, None);
    };
    let wide = w(lnk_path);
    if persist.Load(PCWSTR(wide.as_ptr()), STGM_READ).is_err() {
        return (None, None);
    }

    let mut icon_buf = [0u16; 1024];
    let mut icon_idx = 0i32;
    let icon_loc = if link.GetIconLocation(&mut icon_buf, &mut icon_idx).is_ok() {
        let end = icon_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(icon_buf.len());
        let s = String::from_utf16_lossy(&icon_buf[..end]).trim().to_owned();
        if !s.is_empty() {
            let expanded = expand_env_string(&s);
            if Path::new(&expanded).is_file() {
                Some((expanded, icon_idx))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let mut target_buf = [0u16; 1024];
    let target = if link.GetPath(&mut target_buf, null_mut(), 0).is_ok() {
        let end = target_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(target_buf.len());
        let s = String::from_utf16_lossy(&target_buf[..end])
            .trim()
            .to_owned();
        if !s.is_empty() {
            let expanded = expand_env_string(&s);
            if Path::new(&expanded).is_file() {
                Some(expanded)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    (icon_loc, target)
}

unsafe fn extract_hicon_sized(file: &str, index: i32, size: i32) -> HICON {
    #[link(name = "user32")]
    extern "system" {
        fn PrivateExtractIconsW(
            szFileName: *const u16,
            nIconIndex: i32,
            cxIcon: i32,
            cyIcon: i32,
            phicon: *mut HICON,
            piconid: *mut u32,
            nIcons: u32,
            flags: u32,
        ) -> u32;
    }
    let wide = w(file);
    let mut hicon: HICON = null_mut();
    let mut id: u32 = 0;
    let got = PrivateExtractIconsW(wide.as_ptr(), index, size, size, &mut hicon, &mut id, 1, 0);
    if got > 0 && got != u32::MAX && !hicon.is_null() {
        hicon
    } else {
        null_mut()
    }
}

/// Lightweight, zero-shell-extension icon extraction via `IShellLinkW` + `PrivateExtractIconsW`.
/// Extracts high-res icons directly from `.lnk` targets / `.ico` / `.exe` / `.dll` WITHOUT
/// triggering `IShellItemImageFactory` or loading Windows XAML / Google Drive / OneDrive DLLs!
/// Returns `None` for `shell:AppsFolder\...` or virtual shell items so callers can batch them
/// cleanly through the disposable `--uwp-icons` subprocess.
pub unsafe fn extract_clean_icon_fast(path: &str, size: i32) -> Option<crate::glass::RawIcon> {
    let size = size.clamp(48, 256);
    if path.to_ascii_lowercase().ends_with(".lnk") {
        let (icon_loc, target) = resolve_lnk_icon_source(path);
        if let Some((icon_file, icon_idx)) = icon_loc {
            let hicon = extract_hicon_sized(&icon_file, icon_idx, size);
            if !hicon.is_null() {
                let raw = crate::glass::RawIcon::from_hicon(hicon);
                DestroyIcon(hicon);
                if raw.is_some() {
                    return raw;
                }
            }
        }
        if let Some(target_file) = target {
            let hicon = extract_hicon_sized(&target_file, 0, size);
            if !hicon.is_null() {
                let raw = crate::glass::RawIcon::from_hicon(hicon);
                DestroyIcon(hicon);
                if raw.is_some() {
                    return raw;
                }
            }
        }
        return None;
    }

    if !path.starts_with("shell:") {
        let hicon = extract_hicon_sized(path, 0, size);
        if !hicon.is_null() {
            let raw = crate::glass::RawIcon::from_hicon(hicon);
            DestroyIcon(hicon);
            if raw.is_some() {
                return raw;
            }
        }
    }
    None
}

/// Fallback crisp icon from `shell32.dll` (zero shell extensions, HICON destroyed immediately).
pub unsafe fn fallback_shell32_icon(index: i32, size: i32) -> Option<crate::glass::RawIcon> {
    let hicon = extract_hicon_sized("shell32.dll", index, size.clamp(48, 256));
    if !hicon.is_null() {
        let raw = crate::glass::RawIcon::from_hicon(hicon);
        DestroyIcon(hicon);
        return raw;
    }
    None
}

/// Full in-process icon extraction including `IShellItemImageFactory` (`bitmap_fit`).
/// Run inside the short-lived `--uwp-icons` subprocess so heavy Shell/XAML/Sync DLLs never
/// stay resident in the main `macui.exe` process!
unsafe fn extract_clean_icon_inproc(path: &str, size: i32) -> Option<crate::glass::RawIcon> {
    let size = size.clamp(48, 256);
    if let Some(fast) = extract_clean_icon_fast(path, size) {
        return Some(fast);
    }
    if path.to_ascii_lowercase().ends_with(".lnk") {
        let (_, target) = resolve_lnk_icon_source(path);
        if let Some(target_file) = target {
            // Fallback on the resolved target .exe itself -> STILL zero shortcut arrow!
            let hbmp = bitmap_fit(&target_file, size);
            if !hbmp.is_null() {
                let raw = crate::glass::RawIcon::from_hbitmap(hbmp);
                DeleteObject(hbmp);
                if raw.is_some() {
                    return raw;
                }
            }
        }
    }
    let hbmp = bitmap_fit(path, size);
    if !hbmp.is_null() {
        let raw = crate::glass::RawIcon::from_hbitmap(hbmp);
        DeleteObject(hbmp);
        raw
    } else {
        None
    }
}

fn write_icon_frame(
    out: &mut impl std::io::Write,
    icon: Option<&crate::glass::RawIcon>,
) -> std::io::Result<()> {
    match icon {
        Some(raw)
            if raw.width > 0
                && raw.height > 0
                && raw.bgra.len() == (raw.width as usize) * (raw.height as usize) * 4 =>
        {
            out.write_all(&raw.width.to_le_bytes())?;
            out.write_all(&raw.height.to_le_bytes())?;
            out.write_all(&raw.bgra)?;
        }
        _ => {
            out.write_all(&0u32.to_le_bytes())?;
            out.write_all(&0u32.to_le_bytes())?;
        }
    }
    Ok(())
}

fn read_icon_frame(inp: &mut impl std::io::Read) -> std::io::Result<Option<crate::glass::RawIcon>> {
    let mut hdr = [0u8; 8];
    inp.read_exact(&mut hdr)?;
    let w = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let h = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    if w == 0 || h == 0 || w > 512 || h > 512 {
        return Ok(None);
    }
    let len = (w as usize) * (h as usize) * 4;
    let mut bgra = vec![0u8; len];
    inp.read_exact(&mut bgra)?;
    Ok(crate::glass::RawIcon::from_premul_bgra(w, h, bgra))
}

/// Disposable CLI worker entry (`macui.exe --uwp-icons <size>`).
/// Reads newline-separated paths from stdin, decodes each via `IShellItemImageFactory`,
/// writes binary BGRA frames to stdout, and exits immediately releasing all Shell/XAML RAM.
pub unsafe fn run_uwp_icons_helper(size: i32) {
    use std::io::{BufRead, BufWriter, Write};
    let size = size.clamp(48, 256);
    // Read all requested paths before writing any stdout to guarantee zero pipe deadlock!
    let paths: Vec<String> = {
        let stdin = std::io::stdin();
        stdin.lock().lines().map_while(Result::ok).collect()
    };
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for path in &paths {
        let icon = if path.trim().is_empty() {
            None
        } else {
            extract_clean_icon_inproc(path, size)
        };
        if write_icon_frame(&mut out, icon.as_ref()).is_err() {
            break;
        }
    }
    let _ = out.flush();
    CoUninitialize();
}

/// Decodes a batch of UWP / virtual-shell icons in ONE short-lived silent subprocess so the
/// resident `macui.exe` process stays completely free of `Windows.UI.Xaml.dll` / shell extensions.
pub unsafe fn extract_uwp_icons_batch(
    paths: &[String],
    size: i32,
) -> Vec<Option<crate::glass::RawIcon>> {
    if paths.is_empty() {
        return Vec::new();
    }
    let size = size.clamp(48, 256);

    if !cfg!(test) {
        if let Ok(exe) = std::env::current_exe() {
            use std::io::{BufReader, Write};
            use std::os::windows::process::CommandExt;
            use std::process::{Command, Stdio};
            const CREATE_NO_WINDOW: u32 = 0x08000000;

            let mut results = Vec::with_capacity(paths.len());
            let mut all_ok = true;
            // Chunk to <= 40 paths so stdin payload stays well below Windows pipe buffer
            for chunk in paths.chunks(40) {
                let Ok(mut child) = Command::new(&exe)
                    .arg("--uwp-icons")
                    .arg(size.to_string())
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    all_ok = false;
                    break;
                };
                if let Some(mut stdin) = child.stdin.take() {
                    let mut payload = String::with_capacity(chunk.len() * 64);
                    for p in chunk {
                        payload.push_str(&p.replace(['\r', '\n'], ""));
                        payload.push('\n');
                    }
                    let _ = stdin.write_all(payload.as_bytes());
                    let _ = stdin.flush();
                    // Dropping stdin sends EOF so the child proceeds immediately
                }
                if let Some(stdout) = child.stdout.take() {
                    let mut reader = BufReader::new(stdout);
                    for _ in chunk {
                        match read_icon_frame(&mut reader) {
                            Ok(icon) => results.push(icon),
                            Err(_) => {
                                results.push(None);
                                all_ok = false;
                            }
                        }
                    }
                } else {
                    all_ok = false;
                }
                let _ = child.wait();
                if !all_ok {
                    break;
                }
            }
            if all_ok && results.len() == paths.len() {
                return results;
            }
        }
    }

    paths
        .iter()
        .map(|p| extract_clean_icon_inproc(p, size))
        .collect()
}

/// Extracts a crisp, high-resolution (`size x size`) icon with ZERO Windows shortcut arrow overlay!
/// Uses lightweight `PrivateExtractIconsW` in-process first, isolating UWP / virtual fallbacks
/// into a short-lived `--uwp-icons` subprocess so heavy Shell/XAML DLLs never bloat main RAM.
#[cfg(test)]
pub unsafe fn extract_clean_icon(path: &str, size: i32) -> Option<crate::glass::RawIcon> {
    let size = size.clamp(48, 256);
    if let Some(raw) = extract_clean_icon_fast(path, size) {
        return Some(raw);
    }
    extract_uwp_icons_batch(&[path.to_owned()], size)
        .into_iter()
        .next()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn taskbar_labels() {
        assert_eq!(
            name_status("電卓 - 1 個の実行中ウィンドウ 固定済み"),
            ("電卓".into(), true)
        );
        assert_eq!(name_status("Music 固定済み"), ("Music".into(), false));
        assert!(is_overflow_chevron(
            "隠れているインジケーターを表示します",
            "SystemTrayIcon",
            ""
        ));
        assert!(is_overflow_chevron(
            "システム トレイのオーバーフロー",
            "",
            "SystemTray.ChevronIconViewModel"
        ));
        assert!(is_overflow_chevron("Show hidden icons", "", ""));
        assert!(!is_overflow_chevron(
            "ネットワーク インターネット アクセス",
            "SystemTrayIcon",
            ""
        ));
    }

    #[test]
    fn multi_process_apps_and_shortcuts_match_properly() {
        // Steam CEF helper matches Steam root & redirects back to steam.exe
        assert!(same_app_install_tree(
            r"C:\Program Files (x86)\Steam\bin\cef\cef.win64\steamwebhelper.exe",
            r"C:\Program Files (x86)\Steam\Steam.exe"
        ));
        assert!(is_steam_target(r"C:\Program Files (x86)\Steam\steam.exe"));
        assert!(is_steam_target(
            r"C:\Program Files (x86)\Steam\bin\cef\cef.win64\steamwebhelper.exe"
        ));
        assert!(is_steam_target("Valve.Steam.Client"));
        if Path::new(r"C:\Program Files (x86)\Steam\steam.exe").is_file() {
            let resolved = resolve_helper_to_launcher(
                r"C:\Program Files (x86)\Steam\bin\cef\cef.win64\steamwebhelper.exe",
            );
            assert!(
                resolved.to_lowercase().ends_with(r"\steam\steam.exe"),
                "Expected steamwebhelper.exe to resolve to steam.exe, got {resolved}"
            );
        }

        // Epic Games WebHelper & Discord Squirrel app-1.0.x & LINE nested current/ all match their app root
        assert!(same_app_install_tree(
            r"C:\Program Files\Epic Games\Launcher\Engine\Binaries\Win64\EpicWebHelper.exe",
            r"C:\Program Files\Epic Games\Launcher\Portal\Binaries\Win64\EpicGamesLauncher.exe"
        ));
        assert!(same_app_install_tree(
            r"C:\Users\u\AppData\Local\Discord\app-1.0.9269\Discord.exe",
            r"C:\Users\u\AppData\Local\Discord\Update.exe"
        ));
        assert!(same_app_install_tree(
            r"C:\Users\u\AppData\Local\LINE\bin\current\LINE.exe",
            r"C:\Users\u\AppData\Local\LINE\bin\LineLauncher.exe"
        ));
        // Unrelated system apps in Windows/System32 must NEVER cross-match!
        assert!(!same_app_install_tree(
            r"C:\Windows\System32\notepad.exe",
            r"C:\Windows\System32\cmd.exe"
        ));

        // Normalized shortcut matching: "LINE (2).lnk" -> "LINE", "SteelSeries GG Client.lnk" -> "SteelSeries GG"
        let lnks = vec![
            PathBuf::from(r"C:\TaskBar\LINE (2).lnk"),
            PathBuf::from(r"C:\TaskBar\SteelSeries GG Client.lnk"),
            PathBuf::from(r"C:\StartMenu\Steam\Steam.lnk"),
        ];
        assert_eq!(
            find_matching_link(&lnks, "LINE", "NAVER.WIN32!LINE"),
            Some(&lnks[0])
        );
        assert_eq!(
            find_matching_link(&lnks, "SteelSeries GG", "SteelSeries.Gg.Main"),
            Some(&lnks[1])
        );
        assert_eq!(
            find_matching_link(&lnks, "Steam", "Valve.Steam.Client"),
            Some(&lnks[2])
        );
    }

    #[test]
    fn clean_icon_extracts_high_res_without_shortcut_arrow() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
            let steam_lnk = r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Steam\Steam.lnk";
            if Path::new(steam_lnk).is_file() {
                let (icon_loc, target) = resolve_lnk_icon_source(steam_lnk);
                assert!(
                    icon_loc.is_some() || target.is_some(),
                    "Expected Steam.lnk to resolve target/icon without shortcut overlay"
                );
                let raw = extract_clean_icon(steam_lnk, 96).expect("Failed to extract Steam icon");
                println!("Extracted clean Steam icon: {}x{}", raw.width, raw.height);
                assert!(
                    raw.width >= 64 && raw.height >= 64,
                    "Icon resolution too low: {}x{}",
                    raw.width,
                    raw.height
                );
            }
            CoUninitialize();
        }
    }
    #[test]
    fn uwp_icon_frame_roundtrip_and_start_menu_cache() {
        // Test binary IPC framing preserves pixels & auto-squircle badge analysis
        let mut bgra = vec![0u8; 32 * 32 * 4];
        for y in 12..20 {
            for x in 12..20 {
                let off = (y * 32 + x) * 4;
                bgra[off] = 240;
                bgra[off + 1] = 240;
                bgra[off + 2] = 240;
                bgra[off + 3] = 255;
            }
        }
        let orig = crate::glass::RawIcon::from_premul_bgra(32, 32, bgra).unwrap();
        assert!(orig.badge.is_some_and(|b| b.dark));

        let mut wire = Vec::new();
        write_icon_frame(&mut wire, Some(&orig)).unwrap();
        write_icon_frame(&mut wire, None).unwrap();

        let mut cursor = std::io::Cursor::new(wire);
        let decoded = read_icon_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded.width, 32);
        assert_eq!(decoded.height, 32);
        assert_eq!(decoded.bgra, orig.bgra);
        assert_eq!(decoded.badge, orig.badge);
        assert!(read_icon_frame(&mut cursor).unwrap().is_none());

        // Start Menu links cache returns consistent results and can be invalidated cleanly
        invalidate_start_links_cache();
        let first = start_menu_links();
        let second = start_menu_links();
        assert_eq!(first, second);
    }

    #[test]
    #[ignore = "requires an interactive Explorer desktop"]
    fn inspect_live_taskbar() {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let snapshot = read(&automation().unwrap()).unwrap();
            assert!(!snapshot.apps.is_empty());
            for app in snapshot
                .apps
                .iter()
                .filter(|a| a.running && a.windows.is_empty())
            {
                println!("no window match: {}", app.name);
            }
            println!(
                "{} shell buttons, {} groups resolved",
                snapshot.apps.len(),
                snapshot
                    .apps
                    .iter()
                    .filter(|a| !a.windows.is_empty())
                    .count()
            );
            CoUninitialize();
        }
    }
}
