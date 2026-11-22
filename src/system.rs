use super::*;
use crate::glass::AmbientTone;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use windows::{
    core::{implement, GUID, PCWSTR},
    Win32::{
        Foundation::HANDLE as WHANDLE,
        Media::Audio::{
            eConsole, eRender,
            Endpoints::{
                IAudioEndpointVolume, IAudioEndpointVolumeCallback,
                IAudioEndpointVolumeCallback_Impl,
            },
            IMMDeviceEnumerator, MMDeviceEnumerator, AUDIO_VOLUME_NOTIFICATION_DATA,
        },
        NetworkManagement::{
            IpHelper::{
                GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
                GAA_FLAG_SKIP_MULTICAST, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
                IF_TYPE_PROP_VIRTUAL, IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL,
                IP_ADAPTER_ADDRESSES_LH,
            },
            WiFi::{
                dot11_BSS_type_any, wlan_connection_mode_profile, wlan_interface_state_connected,
                wlan_intf_opcode_current_connection, WLAN_AVAILABLE_NETWORK_LIST,
                WLAN_CONNECTION_ATTRIBUTES, WLAN_CONNECTION_PARAMETERS, WLAN_INTERFACE_INFO_LIST,
                WLAN_INTF_OPCODE, WLAN_OPCODE_VALUE_TYPE,
            },
        },
        Networking::WinSock::{AF_INET, SOCKADDR_IN},
        System::Com::{CoCreateInstance, CLSCTX_ALL},
    },
};
use windows_sys::Win32::{
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS},
    },
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        Input::{Ime::ImmGetDefaultIMEWnd, KeyboardAndMouse::GetKeyboardLayout},
    },
};

// Dynamically load wlanapi.dll on demand so macui.exe has ZERO hard static dependency on
// wlanapi.dll (runs cleanly even on Windows Server / LTSC / stripped desktops without WLAN).
struct WlanApi {
    open_handle:
        unsafe extern "system" fn(u32, *const std::ffi::c_void, *mut u32, *mut WHANDLE) -> u32,
    close_handle: unsafe extern "system" fn(WHANDLE, *const std::ffi::c_void) -> u32,
    enum_interfaces: unsafe extern "system" fn(
        WHANDLE,
        *const std::ffi::c_void,
        *mut *mut WLAN_INTERFACE_INFO_LIST,
    ) -> u32,
    query_interface: unsafe extern "system" fn(
        WHANDLE,
        *const GUID,
        WLAN_INTF_OPCODE,
        *const std::ffi::c_void,
        *mut u32,
        *mut *mut std::ffi::c_void,
        *mut WLAN_OPCODE_VALUE_TYPE,
    ) -> u32,
    get_available_network_list: unsafe extern "system" fn(
        WHANDLE,
        *const GUID,
        u32,
        *const std::ffi::c_void,
        *mut *mut WLAN_AVAILABLE_NETWORK_LIST,
    ) -> u32,
    free_memory: unsafe extern "system" fn(*mut std::ffi::c_void),
    connect: unsafe extern "system" fn(
        WHANDLE,
        *const GUID,
        *const WLAN_CONNECTION_PARAMETERS,
        *const std::ffi::c_void,
    ) -> u32,
}

static WLAN_API: std::sync::OnceLock<Option<WlanApi>> = std::sync::OnceLock::new();

unsafe fn wlan_api() -> Option<&'static WlanApi> {
    WLAN_API
        .get_or_init(|| {
            let hmod = LoadLibraryW(w("wlanapi.dll").as_ptr());
            if hmod.is_null() {
                return None;
            }
            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let p = GetProcAddress(hmod, concat!($name, "\0").as_ptr())?;
                    std::mem::transmute::<_, $ty>(p)
                }};
            }
            Some(WlanApi {
                open_handle: sym!(
                    "WlanOpenHandle",
                    unsafe extern "system" fn(
                        u32,
                        *const std::ffi::c_void,
                        *mut u32,
                        *mut WHANDLE,
                    ) -> u32
                ),
                close_handle: sym!(
                    "WlanCloseHandle",
                    unsafe extern "system" fn(WHANDLE, *const std::ffi::c_void) -> u32
                ),
                enum_interfaces: sym!(
                    "WlanEnumInterfaces",
                    unsafe extern "system" fn(
                        WHANDLE,
                        *const std::ffi::c_void,
                        *mut *mut WLAN_INTERFACE_INFO_LIST,
                    ) -> u32
                ),
                query_interface: sym!(
                    "WlanQueryInterface",
                    unsafe extern "system" fn(
                        WHANDLE,
                        *const GUID,
                        WLAN_INTF_OPCODE,
                        *const std::ffi::c_void,
                        *mut u32,
                        *mut *mut std::ffi::c_void,
                        *mut WLAN_OPCODE_VALUE_TYPE,
                    ) -> u32
                ),
                get_available_network_list: sym!(
                    "WlanGetAvailableNetworkList",
                    unsafe extern "system" fn(
                        WHANDLE,
                        *const GUID,
                        u32,
                        *const std::ffi::c_void,
                        *mut *mut WLAN_AVAILABLE_NETWORK_LIST,
                    ) -> u32
                ),
                free_memory: sym!(
                    "WlanFreeMemory",
                    unsafe extern "system" fn(*mut std::ffi::c_void)
                ),
                connect: sym!(
                    "WlanConnect",
                    unsafe extern "system" fn(
                        WHANDLE,
                        *const GUID,
                        *const WLAN_CONNECTION_PARAMETERS,
                        *const std::ffi::c_void,
                    ) -> u32
                ),
            })
        })
        .as_ref()
}

pub const WM_SYS_UPDATE: u32 = WM_APP + 51;

const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
const EVENT_OBJECT_NAMECHANGE: u32 = 0x800C;
const WM_IME_CONTROL: u32 = 0x0283;
const IMC_GETCONVERSIONMODE: usize = 0x0001;
const IMC_GETOPENSTATUS: usize = 0x0005;
const IMC_SETOPENSTATUS: usize = 0x0006;

static VOL_PERCENT: AtomicU8 = AtomicU8::new(50);
static VOL_MUTED: AtomicBool = AtomicBool::new(false);
static VOL_VALID: AtomicBool = AtomicBool::new(false);
static NOTIFY_TARGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetKind {
    Ethernet,
    Wifi,
    Vpn,
    Other,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NetAdapterInfo {
    pub name: String,
    pub description: String,
    pub kind: NetKind,
    pub up: bool,
    pub has_gateway: bool,
    pub ipv4: String,
    pub speed_label: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal: u8, // 0..=100
    pub secured: bool,
    pub connected: bool,
    pub has_profile: bool,
    pub iface_guid: GUID,
}

#[derive(Clone, PartialEq)]
pub struct SysState {
    pub active_app: String,
    pub active_hwnd: usize,
    pub volume: u8,
    pub muted: bool,
    pub volume_valid: bool,
    pub wifi_ssid: String,
    pub wifi_signal: u8, // 0=off, 1..=3=wifi bars, 4=ethernet
    pub net_title: String,
    pub net_detail: String,
    pub net_probed: bool,
    pub wifi_hw_present: bool,
    pub adapters: Vec<NetAdapterInfo>,
    pub wifi_networks: Vec<WifiNetwork>,
    pub ime_badge: String,
    pub battery_present: bool,
    pub battery_percent: u8,
    pub battery_charging: bool,
    pub bar_tone: AmbientTone,
    pub dock_tone: AmbientTone,
}

impl Default for SysState {
    fn default() -> Self {
        Self {
            active_app: "デスクトップ".into(),
            active_hwnd: 0,
            volume: 50,
            muted: false,
            volume_valid: false,
            wifi_ssid: String::new(),
            wifi_signal: 0,
            net_title: "ネットワーク".into(),
            net_detail: "確認中…".into(),
            net_probed: false,
            wifi_hw_present: false,
            adapters: Vec::new(),
            wifi_networks: Vec::new(),
            ime_badge: "A".into(),
            battery_present: false,
            battery_percent: 100,
            battery_charging: false,
            bar_tone: AmbientTone::default(),
            dock_tone: AmbientTone::default(),
        }
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolCallback;

impl IAudioEndpointVolumeCallback_Impl for VolCallback_Impl {
    fn OnNotify(&self, pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if !pnotify.is_null() {
            unsafe {
                let d = &*pnotify;
                VOL_PERCENT.store(
                    (d.fMasterVolume * 100.0).round().clamp(0.0, 100.0) as u8,
                    Ordering::Relaxed,
                );
                VOL_MUTED.store(d.bMuted.as_bool(), Ordering::Relaxed);
                VOL_VALID.store(true, Ordering::Relaxed);
            }
            let h = NOTIFY_TARGET.load(Ordering::Relaxed);
            if h != 0 {
                unsafe {
                    PostMessageW(h as HWND, WM_SYS_UPDATE, 1, 0);
                }
            }
        }
        Ok(())
    }
}

struct AudioHolder {
    endpoint: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
}

thread_local! {
    static STATE: RefCell<SysState> = RefCell::new(SysState::default());
    static AUDIO: RefCell<Option<AudioHolder>> = const { RefCell::new(None) };
    static FG_HOOK: Cell<HWINEVENTHOOK> = const { Cell::new(null_mut()) };
    static MOVE_HOOK: Cell<HWINEVENTHOOK> = const { Cell::new(null_mut()) };
    static LAST_NET_POLL: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
}

pub unsafe fn trim_working_set() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetProcessWorkingSetSize(hProcess: HANDLE, min: usize, max: usize) -> i32;
    }
    SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
}

unsafe extern "system" fn fg_event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _: u32,
    _: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND
        || event == EVENT_SYSTEM_MOVESIZEEND
        || (event == EVENT_OBJECT_NAMECHANGE
            && id_object == 0
            && id_child == 0
            && hwnd == GetForegroundWindow())
    {
        let changed = if event == EVENT_SYSTEM_MOVESIZEEND {
            refresh_ambient()
        } else {
            refresh_foreground()
        };
        if changed {
            bar::repaint();
            if !surface::is_hidden() {
                surface::repaint_now();
            }
        }
    }
}

pub unsafe fn start(owner: HWND) {
    NOTIFY_TARGET.store(owner as usize, Ordering::Relaxed);
    init_audio();
    refresh_all();
    let hook = SetWinEventHook(
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_FOREGROUND,
        null_mut(),
        Some(fg_event),
        0,
        0,
        WINEVENT_OUTOFCONTEXT,
    );
    FG_HOOK.set(hook);
    let mhook = SetWinEventHook(
        EVENT_SYSTEM_MOVESIZEEND,
        EVENT_SYSTEM_MOVESIZEEND,
        null_mut(),
        Some(fg_event),
        0,
        0,
        WINEVENT_OUTOFCONTEXT,
    );
    MOVE_HOOK.set(mhook);
}

pub unsafe fn stop() {
    let hook = FG_HOOK.replace(null_mut());
    if !hook.is_null() {
        UnhookWinEvent(hook);
    }
    let mhook = MOVE_HOOK.replace(null_mut());
    if !mhook.is_null() {
        UnhookWinEvent(mhook);
    }
    AUDIO.with_borrow_mut(|slot| {
        if let Some(a) = slot.take() {
            let _ = a.endpoint.UnregisterControlChangeNotify(&a.callback);
        }
    });
    NOTIFY_TARGET.store(0, Ordering::Relaxed);
}

unsafe fn init_audio() {
    let res: windows::core::Result<()> = (|| {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
        let vol = endpoint.GetMasterVolumeLevelScalar().unwrap_or(0.5);
        let muted = endpoint.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        VOL_PERCENT.store(
            (vol * 100.0).round().clamp(0.0, 100.0) as u8,
            Ordering::Relaxed,
        );
        VOL_MUTED.store(muted, Ordering::Relaxed);
        VOL_VALID.store(true, Ordering::Relaxed);
        let callback: IAudioEndpointVolumeCallback = VolCallback.into();
        let _ = endpoint.RegisterControlChangeNotify(&callback);
        AUDIO.with_borrow_mut(|slot| {
            *slot = Some(AudioHolder { endpoint, callback });
        });
        Ok(())
    })();
    let _ = res;
}

pub unsafe fn set_volume(percent: f32) {
    let v = (percent / 100.0).clamp(0.0, 1.0);
    AUDIO.with_borrow(|slot| {
        if let Some(a) = slot {
            let _ = a.endpoint.SetMasterVolumeLevelScalar(v, null());
            if v > 0.005 {
                let _ = a.endpoint.SetMute(false, null());
            }
        }
    });
    VOL_PERCENT.store(percent.round().clamp(0.0, 100.0) as u8, Ordering::Relaxed);
    if percent > 0.5 {
        VOL_MUTED.store(false, Ordering::Relaxed);
    }
    sync_atomic_audio();
}

pub unsafe fn adjust_volume(delta: i32) {
    let cur = state().volume as i32;
    let next = (cur + delta).clamp(0, 100) as f32;
    set_volume(next);
}

pub unsafe fn toggle_mute() {
    let next = !VOL_MUTED.load(Ordering::Relaxed);
    AUDIO.with_borrow(|slot| {
        if let Some(a) = slot {
            let _ = a.endpoint.SetMute(next, null());
        }
    });
    VOL_MUTED.store(next, Ordering::Relaxed);
    sync_atomic_audio();
}

pub fn sync_atomic_audio() {
    STATE.with_borrow_mut(|s| {
        if VOL_VALID.load(Ordering::Relaxed) {
            s.volume = VOL_PERCENT.load(Ordering::Relaxed);
            s.muted = VOL_MUTED.load(Ordering::Relaxed);
            s.volume_valid = true;
        }
    });
}

pub fn state() -> SysState {
    STATE.with_borrow(Clone::clone)
}

pub unsafe fn refresh_all() {
    sync_atomic_audio();
    refresh_foreground();
    refresh_ambient();
    refresh_battery();
    refresh_network_inner(false);
    LAST_NET_POLL.set(Some(std::time::Instant::now()));
}

/// Fast (~0.1ms), zero-polling ambient color & luminance sampler for Liquid Glass adaptation.
/// Downsamples the given desktop rectangle into a tiny 16x4 DIB and computes average RGB & luma.
pub unsafe fn sample_screen_tone(x: i32, y: i32, w: i32, h: i32) -> AmbientTone {
    let sw = GetSystemMetrics(SM_CXSCREEN).max(1);
    let sh = GetSystemMetrics(SM_CYSCREEN).max(1);
    let sx = x.clamp(0, sw - 1);
    let sy = y.clamp(0, sh - 1);
    let cw = w.clamp(1, sw - sx);
    let ch = h.clamp(1, sh - sy);

    let screen_dc = GetDC(null_mut());
    if screen_dc.is_null() {
        return AmbientTone::default();
    }
    let mem_dc = CreateCompatibleDC(screen_dc);
    if mem_dc.is_null() {
        ReleaseDC(null_mut(), screen_dc);
        return AmbientTone::default();
    }

    const DW: i32 = 16;
    const DH: i32 = 4;
    let mut bmi: BITMAPINFO = zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = DW;
    bmi.bmiHeader.biHeight = -DH;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;

    let mut bits: *mut std::ffi::c_void = null_mut();
    let hbmp = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    let mut result = AmbientTone::default();
    if !hbmp.is_null() && !bits.is_null() {
        let old = SelectObject(mem_dc, hbmp);
        SetStretchBltMode(mem_dc, HALFTONE as i32);
        if StretchBlt(mem_dc, 0, 0, DW, DH, screen_dc, sx, sy, cw, ch, SRCCOPY) != 0 {
            let px = std::slice::from_raw_parts(bits as *const u8, (DW * DH * 4) as usize);
            let mut sum_r = 0.0f32;
            let mut sum_g = 0.0f32;
            let mut sum_b = 0.0f32;
            let count = (DW * DH) as f32;
            for chunk in px.chunks_exact(4) {
                sum_b += chunk[0] as f32 / 255.0;
                sum_g += chunk[1] as f32 / 255.0;
                sum_r += chunk[2] as f32 / 255.0;
            }
            let r = sum_r / count;
            let g = sum_g / count;
            let b = sum_b / count;
            // Guard against pure-zero headless/locked-desktop captures
            if r + g + b > 0.01 {
                result = AmbientTone::from_rgb(r, g, b);
            }
        }
        SelectObject(mem_dc, old);
        DeleteObject(hbmp);
    }
    DeleteDC(mem_dc);
    ReleaseDC(null_mut(), screen_dc);
    result
}

fn tone_differs(a: AmbientTone, b: AmbientTone) -> bool {
    (a.r - b.r).abs() > 0.012
        || (a.g - b.g).abs() > 0.012
        || (a.b - b.b).abs() > 0.012
        || (a.luma - b.luma).abs() > 0.012
}

/// Samples background strips right outside the Menu Bar and Dock (zero self-occlusion, event-driven).
/// Returns `true` when either sampled ambient tone visibly changed.
pub unsafe fn refresh_ambient() -> bool {
    let sw = GetSystemMetrics(SM_CXSCREEN).max(640);
    let sh = GetSystemMetrics(SM_CYSCREEN).max(480);
    let bar_h = bar::height().max(26);
    // Strip immediately below Menu Bar reflects either the wallpaper top or maximized window title bar
    let bar_tone = sample_screen_tone(0, bar_h + 2, sw, (bar_h / 2).max(12));
    // Strip centered just above the Dock (DPI-scaled offset) reflects the lower desktop / active window behind the Dock
    let dock_strip_y = (sh - (bar_h * 3 + 14)).max(bar_h + 16);
    let dock_tone = sample_screen_tone(sw / 4, dock_strip_y, sw / 2, (bar_h / 2).max(14));
    STATE.with_borrow_mut(|s| {
        let changed = tone_differs(s.bar_tone, bar_tone) || tone_differs(s.dock_tone, dock_tone);
        s.bar_tone = bar_tone;
        s.dock_tone = dock_tone;
        changed
    })
}

pub unsafe fn refresh_battery() {
    let mut power: SYSTEM_POWER_STATUS = zeroed();
    if GetSystemPowerStatus(&mut power) != 0 {
        let present = power.BatteryFlag != 128 && power.BatteryLifePercent <= 100;
        STATE.with_borrow_mut(|s| {
            s.battery_present = present;
            if present {
                s.battery_percent = power.BatteryLifePercent;
                s.battery_charging = power.ACLineStatus == 1 || (power.BatteryFlag & 8) != 0;
            }
        });
    }
}

fn format_link_speed(bps: u64) -> String {
    if bps == 0 || bps == u64::MAX {
        return String::new();
    }
    if bps >= 1_000_000_000 {
        let gbps = bps as f64 / 1_000_000_000.0;
        if (gbps - gbps.round()).abs() < 0.05 {
            format!("{} Gbps", gbps.round() as u64)
        } else {
            format!("{gbps:.1} Gbps")
        }
    } else if bps >= 1_000_000 {
        format!("{} Mbps", (bps + 500_000) / 1_000_000)
    } else {
        String::new()
    }
}

fn is_virtual_noise_adapter(name: &str, desc: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let d = desc.to_ascii_lowercase();
    d.contains("hyper-v")
        || d.contains("virtual")
        || d.contains("wi-fi direct")
        || d.contains("xbox wireless adapter")
        || d.contains("bluetooth")
        || d.contains("vmware")
        || d.contains("virtualbox")
        || d.contains("loopback")
        || d.contains("wsl")
        || n.starts_with("vethernet")
        || name.starts_with("ローカル エリア接続*")
        || n.starts_with("local area connection*")
}

fn is_vpn_adapter(name: &str, desc: &str, if_type: u32) -> bool {
    let n = name.to_ascii_lowercase();
    let d = desc.to_ascii_lowercase();
    if_type == IF_TYPE_TUNNEL
        || if_type == IF_TYPE_PROP_VIRTUAL
        || n.contains("tailscale")
        || d.contains("tailscale")
        || d.contains("wireguard")
        || d.contains("wintun")
        || d.contains("openvpn")
        || d.contains("tap-windows")
        || d.contains("vpn")
}

/// Native, zero-subprocess network state query using Windows IP Helper (`GetAdaptersAddresses`)
/// and Native Wi-Fi (`WlanEnumInterfaces`, plus `WlanGetAvailableNetworkList` only when requested).
/// Accurately distinguishes active Ethernet vs. Wi-Fi vs. VPN vs. Offline!
unsafe fn refresh_network_inner(scan_nearby_wifi: bool) -> bool {
    let mut adapters: Vec<NetAdapterInfo> = Vec::new();
    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut buf_len = 16384u32;
    let mut buf = vec![0u8; buf_len as usize];
    let mut ret = GetAdaptersAddresses(
        AF_INET.0 as u32,
        flags,
        None,
        Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
        &mut buf_len,
    );
    if ret == 111 {
        // ERROR_BUFFER_OVERFLOW
        buf.resize(buf_len as usize, 0);
        ret = GetAdaptersAddresses(
            AF_INET.0 as u32,
            flags,
            None,
            Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
            &mut buf_len,
        );
    }

    if ret == 0 {
        let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !cur.is_null() {
            let a = &*cur;
            if a.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let name = a.FriendlyName.to_string().unwrap_or_default();
                let desc = a.Description.to_string().unwrap_or_default();
                let up = a.OperStatus.0 == 1; // IfOperStatusUp
                let has_gateway = !a.FirstGatewayAddress.is_null();

                let mut ipv4 = String::new();
                let mut u = a.FirstUnicastAddress;
                while !u.is_null() {
                    let ua = &*u;
                    if !ua.Address.lpSockaddr.is_null()
                        && (*ua.Address.lpSockaddr).sa_family == AF_INET
                    {
                        let sin = &*(ua.Address.lpSockaddr as *const SOCKADDR_IN);
                        let b = sin.sin_addr.S_un.S_un_b;
                        // Skip link-local APIPA 169.254.x.x if a better IP exists
                        let cand = format!("{}.{}.{}.{}", b.s_b1, b.s_b2, b.s_b3, b.s_b4);
                        if !(b.s_b1 == 169 && b.s_b2 == 254) {
                            ipv4 = cand;
                            break;
                        } else if ipv4.is_empty() {
                            ipv4 = cand;
                        }
                    }
                    u = ua.Next;
                }

                let kind = if is_vpn_adapter(&name, &desc, a.IfType) {
                    NetKind::Vpn
                } else if is_virtual_noise_adapter(&name, &desc) {
                    NetKind::Other
                } else if a.IfType == IF_TYPE_IEEE80211 {
                    NetKind::Wifi
                } else if a.IfType == IF_TYPE_ETHERNET_CSMACD {
                    NetKind::Ethernet
                } else {
                    NetKind::Other
                };

                // Include physical Ethernet/Wi-Fi adapters and active VPN tunnels cleanly
                if matches!(kind, NetKind::Ethernet | NetKind::Wifi) || (kind == NetKind::Vpn && up)
                {
                    let speed_label = if kind == NetKind::Vpn {
                        if !ipv4.is_empty() {
                            format!("VPN • {ipv4}")
                        } else {
                            "VPN".into()
                        }
                    } else {
                        format_link_speed(a.TransmitLinkSpeed)
                    };
                    adapters.push(NetAdapterInfo {
                        name,
                        description: desc,
                        kind,
                        up,
                        has_gateway,
                        ipv4,
                        speed_label,
                    });
                }
            }
            cur = a.Next;
        }
    }

    // Sort adapters: connected gateway first -> connected Ethernet -> connected Wi-Fi -> VPN -> disconnected
    adapters.sort_by_key(|a| {
        (
            !a.up,
            !a.has_gateway,
            match a.kind {
                NetKind::Ethernet => 0,
                NetKind::Wifi => 1,
                NetKind::Vpn => 2,
                NetKind::Other => 3,
            },
        )
    });

    // Determine active Ethernet first: during lightweight background ticks we can skip WLAN RPC
    // completely when wired Ethernet is already active and no nearby Wi-Fi scan was requested!
    let active_eth = adapters
        .iter()
        .find(|a| a.kind == NetKind::Ethernet && a.up && a.has_gateway)
        .or_else(|| {
            adapters
                .iter()
                .find(|a| a.kind == NetKind::Ethernet && a.up && !a.ipv4.is_empty())
        });
    let active_wifi = adapters
        .iter()
        .find(|a| a.kind == NetKind::Wifi && a.up && (a.has_gateway || !a.ipv4.is_empty()));

    // Query Native Wi-Fi current connection (and available networks ONLY when opening Network Panel)
    let mut wifi_connected_ssid = String::new();
    let mut wifi_connected_bars = 0u8;
    let mut wifi_hw_present = adapters.iter().any(|a| a.kind == NetKind::Wifi);
    let mut wifi_networks: Vec<WifiNetwork> = Vec::new();
    let need_wlan =
        scan_nearby_wifi || (active_eth.is_none() && (wifi_hw_present || active_wifi.is_some()));

    if need_wlan {
        if let Some(wlan) = wlan_api() {
            let mut negotiated = 0u32;
            let mut handle = WHANDLE::default();
            if (wlan.open_handle)(2, null(), &mut negotiated, &mut handle) == 0 {
                let mut list_ptr: *mut WLAN_INTERFACE_INFO_LIST = null_mut();
                if (wlan.enum_interfaces)(handle, null(), &mut list_ptr) == 0 && !list_ptr.is_null()
                {
                    let count = (*list_ptr).dwNumberOfItems as usize;
                    if count > 0 {
                        wifi_hw_present = true;
                    }
                    let items =
                        std::slice::from_raw_parts((*list_ptr).InterfaceInfo.as_ptr(), count);
                    for info in items {
                        if info.isState == wlan_interface_state_connected {
                            let mut data_size = 0u32;
                            let mut data_ptr: *mut std::ffi::c_void = null_mut();
                            if (wlan.query_interface)(
                                handle,
                                &info.InterfaceGuid,
                                wlan_intf_opcode_current_connection,
                                null(),
                                &mut data_size,
                                &mut data_ptr,
                                null_mut(),
                            ) == 0
                                && !data_ptr.is_null()
                            {
                                let attr = &*(data_ptr as *const WLAN_CONNECTION_ATTRIBUTES);
                                let ssid_len =
                                    (attr.wlanAssociationAttributes.dot11Ssid.uSSIDLength as usize)
                                        .min(attr.wlanAssociationAttributes.dot11Ssid.ucSSID.len());
                                wifi_connected_ssid = String::from_utf8_lossy(
                                    &attr.wlanAssociationAttributes.dot11Ssid.ucSSID[..ssid_len],
                                )
                                .into_owned();
                                let q = attr.wlanAssociationAttributes.wlanSignalQuality;
                                wifi_connected_bars = if q >= 68 {
                                    3
                                } else if q >= 35 {
                                    2
                                } else if q > 0 {
                                    1
                                } else {
                                    0
                                };
                                (wlan.free_memory)(data_ptr);
                            }
                        }

                        if scan_nearby_wifi {
                            // Collect available nearby Wi-Fi networks ONLY when Network Panel asks for them
                            let mut avail_ptr: *mut WLAN_AVAILABLE_NETWORK_LIST = null_mut();
                            if (wlan.get_available_network_list)(
                                handle,
                                &info.InterfaceGuid,
                                0,
                                null(),
                                &mut avail_ptr,
                            ) == 0
                                && !avail_ptr.is_null()
                            {
                                let n_cnt = (*avail_ptr).dwNumberOfItems as usize;
                                let nets = std::slice::from_raw_parts(
                                    (*avail_ptr).Network.as_ptr(),
                                    n_cnt,
                                );
                                for net in nets {
                                    let slen = (net.dot11Ssid.uSSIDLength as usize)
                                        .min(net.dot11Ssid.ucSSID.len());
                                    if slen == 0 {
                                        continue;
                                    }
                                    let ssid =
                                        String::from_utf8_lossy(&net.dot11Ssid.ucSSID[..slen])
                                            .trim()
                                            .to_owned();
                                    if ssid.is_empty() {
                                        continue;
                                    }
                                    let connected = (net.dwFlags & 1) != 0; // WLAN_AVAILABLE_NETWORK_CONNECTED
                                    let has_profile = (net.dwFlags & 2) != 0; // WLAN_AVAILABLE_NETWORK_HAS_PROFILE
                                    let signal = net.wlanSignalQuality.min(100) as u8;
                                    let secured = net.bSecurityEnabled.as_bool();
                                    if let Some(existing) =
                                        wifi_networks.iter_mut().find(|w| w.ssid == ssid)
                                    {
                                        existing.connected |= connected;
                                        existing.has_profile |= has_profile;
                                        existing.signal = existing.signal.max(signal);
                                    } else {
                                        wifi_networks.push(WifiNetwork {
                                            ssid,
                                            signal,
                                            secured,
                                            connected,
                                            has_profile,
                                            iface_guid: info.InterfaceGuid,
                                        });
                                    }
                                }
                                (wlan.free_memory)(avail_ptr.cast());
                            }
                        }
                    }
                    (wlan.free_memory)(list_ptr.cast());
                }
                let _ = (wlan.close_handle)(handle, null());
            }
        }
    }

    if scan_nearby_wifi {
        wifi_networks.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.has_profile.cmp(&a.has_profile))
                .then(b.signal.cmp(&a.signal))
        });
    }

    let (sig, title, detail, ssid_out) = if let Some(eth) = active_eth {
        let det = if !eth.speed_label.is_empty() {
            format!("接続済み • {}", eth.speed_label)
        } else if !eth.ipv4.is_empty() {
            eth.ipv4.clone()
        } else {
            "接続済み".into()
        };
        let t = if eth.name.is_empty() {
            "イーサネット".to_owned()
        } else {
            eth.name.clone()
        };
        (4u8, t, det.clone(), det)
    } else if !wifi_connected_ssid.is_empty() || active_wifi.is_some() {
        let bars = if wifi_connected_bars > 0 {
            wifi_connected_bars
        } else {
            3
        };
        let label = if !wifi_connected_ssid.is_empty() {
            wifi_connected_ssid.clone()
        } else {
            "接続済み".into()
        };
        (bars, "Wi-Fi".to_owned(), label.clone(), label)
    } else if let Some(any_up) = adapters.iter().find(|a| a.up && a.has_gateway) {
        let det = if !any_up.ipv4.is_empty() {
            any_up.ipv4.clone()
        } else {
            "接続済み".into()
        };
        (4u8, any_up.name.clone(), det.clone(), det)
    } else {
        (
            0u8,
            "ネットワーク".to_owned(),
            "未接続".into(),
            String::new(),
        )
    };

    STATE.with_borrow_mut(|s| {
        let changed = s.wifi_signal != sig
            || s.net_title != title
            || s.net_detail != detail
            || s.adapters != adapters;
        s.wifi_signal = sig;
        s.net_title = title;
        s.net_detail = detail;
        s.wifi_ssid = ssid_out;
        s.net_probed = true;
        s.wifi_hw_present = wifi_hw_present;
        s.adapters = adapters;
        if scan_nearby_wifi {
            s.wifi_networks = wifi_networks;
        }
        changed
    })
}

/// Full network + nearby Wi-Fi refresh (used when opening the Network Panel or clicking Refresh).
pub unsafe fn refresh_network() {
    refresh_network_inner(true);
    LAST_NET_POLL.set(Some(std::time::Instant::now()));
}

/// Lightweight background network check throttled to once every 45s (zero nearby Wi-Fi scanning).
pub unsafe fn refresh_network_periodic() -> bool {
    let now = std::time::Instant::now();
    if let Some(last) = LAST_NET_POLL.get() {
        if now.duration_since(last) < std::time::Duration::from_secs(45)
            && STATE.with_borrow(|s| s.net_probed)
        {
            return false;
        }
    }
    LAST_NET_POLL.set(Some(now));
    refresh_network_inner(false)
}

/// Attempts native connection to a saved Windows Wi-Fi profile via WlanConnect.
pub unsafe fn connect_wifi_profile(iface_guid: GUID, profile_name: &str) -> bool {
    let Some(wlan) = wlan_api() else {
        return false;
    };
    let mut negotiated = 0u32;
    let mut handle = WHANDLE::default();
    if (wlan.open_handle)(2, null(), &mut negotiated, &mut handle) != 0 {
        return false;
    }
    let wide = w(profile_name);
    let params = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: PCWSTR(wide.as_ptr()),
        pDot11Ssid: null_mut(),
        pDesiredBssidList: null_mut(),
        dot11BssType: dot11_BSS_type_any,
        dwFlags: 0,
    };
    let ok = (wlan.connect)(handle, &iface_guid, &params, null()) == 0;
    let _ = (wlan.close_handle)(handle, null());
    ok
}

/// Toggles Japanese MS-IME open status on the last active foreground window (or switches layout).
pub unsafe fn toggle_ime() {
    let fg = STATE.with_borrow(|s| s.active_hwnd as HWND);
    if !fg.is_null() && IsWindow(fg) != 0 {
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(fg, &mut pid);
        let lang_id = (GetKeyboardLayout(tid) as usize & 0xFFFF) as u16;
        if lang_id == 0x0411 {
            let ime_wnd = ImmGetDefaultIMEWnd(fg);
            if !ime_wnd.is_null() {
                let mut open = 0usize;
                if SendMessageTimeoutW(
                    ime_wnd,
                    WM_IME_CONTROL,
                    IMC_GETOPENSTATUS,
                    0,
                    SMTO_ABORTIFHUNG,
                    40,
                    &mut open,
                ) != 0
                {
                    let next = if open == 0 { 1usize } else { 0usize };
                    let mut res = 0usize;
                    let _ = SendMessageTimeoutW(
                        ime_wnd,
                        WM_IME_CONTROL,
                        IMC_SETOPENSTATUS,
                        next as isize,
                        SMTO_ABORTIFHUNG,
                        40,
                        &mut res,
                    );
                    if let Some(badge) = detect_ime(fg, tid) {
                        STATE.with_borrow_mut(|s| s.ime_badge = badge);
                    }
                    return;
                }
            }
        }
    }
    bar::shortcut(VK_SPACE);
}

pub fn merge_uia_status(uia: &tasks::Status) -> bool {
    STATE.with_borrow_mut(|s| {
        let mut changed = false;
        if !s.volume_valid && !uia.volume.is_empty() {
            if uia.volume.contains("ミュート") || uia.volume.to_lowercase().contains("mute") {
                if !s.muted {
                    s.muted = true;
                    changed = true;
                }
            } else if let Some(v) = uia.volume.trim_end_matches('%').parse::<u8>().ok() {
                if s.volume != v || s.muted {
                    s.volume = v;
                    s.muted = false;
                    changed = true;
                }
            }
        }
        // Only fall back to UIA tray text if native IP Helper / WLAN hasn't probed adapters yet
        if !s.net_probed && !uia.network.is_empty() {
            let lower = uia.network.to_lowercase();
            let (sig, title, detail) = if lower.contains("未接続")
                || lower.contains("not connected")
                || lower.contains("no internet")
            {
                (0u8, "ネットワーク".to_owned(), "未接続".to_owned())
            } else if lower.contains("イーサネット")
                || lower.contains("ethernet")
                || lower.contains("有線")
            {
                (4u8, "イーサネット".to_owned(), "接続済み".to_owned())
            } else {
                (3u8, "Wi-Fi".to_owned(), uia.network.clone())
            };
            if s.wifi_signal != sig || s.net_title != title || s.net_detail != detail {
                s.wifi_signal = sig;
                s.net_title = title;
                s.net_detail = detail;
                changed = true;
            }
        }
        if !uia.input.is_empty() {
            let u = uia.input.as_str();
            let next_ime = if u.contains("あ") || u.contains("ひらがな") {
                Some("あ")
            } else if u.contains("ア") || u.contains("カタカナ") {
                Some("ア")
            } else if u.contains("A") || u.contains("英") || u.contains("ENG") || u.contains("US")
            {
                Some("A")
            } else if u.contains("日本") && s.ime_badge.is_empty() {
                Some("あ")
            } else {
                None
            };
            if let Some(badge) = next_ime {
                if s.ime_badge != badge {
                    s.ime_badge = badge.into();
                    changed = true;
                }
            }
        }
        changed
    })
}

fn is_fullscreen_bounds(r: RECT, mon: RECT, has_caption: bool, zoomed: bool) -> bool {
    // Normal captioned maximized desktop apps (Chrome, VS Code, Explorer, etc.) are NEVER fullscreen!
    if has_caption && zoomed {
        return false;
    }
    if has_caption {
        return false;
    }
    r.left <= mon.left && r.top <= mon.top && r.right >= mon.right && r.bottom >= mon.bottom
}

unsafe fn detect_true_fullscreen(fg: HWND, class_name: &str) -> bool {
    if fg.is_null() || IsWindowVisible(fg) == 0 || IsIconic(fg) != 0 {
        return false;
    }
    if matches!(
        class_name,
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "TopLevelWindowForOverflowXamlIsland"
            | "NotifyIconOverflowWindow"
            | "XamlExplorerHostIslandWindow"
            | "Windows.UI.Core.CoreWindow"
            | "ForegroundStaging"
            | "MultitaskingViewFrame"
            | "TaskListThumbnailWnd"
    ) {
        return false;
    }
    let hmon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
    if hmon.is_null() {
        return false;
    }
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(hmon, &mut mi) == 0 || (mi.dwFlags & 1/* MONITORINFOF_PRIMARY */) == 0 {
        // Window is on a secondary monitor — never hide the primary monitor's Dock/Menu Bar!
        return false;
    }
    let style = GetWindowLongW(fg, GWL_STYLE) as u32;
    let has_caption = (style & WS_CAPTION) == WS_CAPTION;
    let zoomed = IsZoomed(fg) != 0;
    let mut r: RECT = zeroed();
    let got_ext = windows_sys::Win32::Graphics::Dwm::DwmGetWindowAttribute(
        fg,
        windows_sys::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS as u32,
        (&mut r as *mut RECT).cast(),
        size_of::<RECT>() as u32,
    ) == 0;
    if !got_ext && GetWindowRect(fg, &mut r) == 0 {
        return false;
    }
    is_fullscreen_bounds(r, mi.rcMonitor, has_caption, zoomed)
}

fn friendly_exe_name(stem: &str) -> String {
    match stem.to_ascii_lowercase().as_str() {
        "explorer" => "Finder".into(),
        "chrome" => "Google Chrome".into(),
        "msedge" => "Microsoft Edge".into(),
        "brave" => "Brave".into(),
        "firefox" => "Firefox".into(),
        "code" => "Visual Studio Code".into(),
        "windowsterminal" => "ターミナル".into(),
        "discord" => "Discord".into(),
        "taskmgr" => "タスク マネージャー".into(),
        "notepad" => "メモ帳".into(),
        "applicationframehost" => "設定".into(),
        _ => {
            let mut chars = stem.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => "デスクトップ".into(),
            }
        }
    }
}

pub unsafe fn refresh_foreground() -> bool {
    let fg = GetForegroundWindow();
    if fg.is_null() {
        return false;
    }
    let mut pid = 0u32;
    let tid = GetWindowThreadProcessId(fg, &mut pid);
    // Don't switch the active app name when our own MacUI panel/bar/dock briefly takes focus!
    if pid == std::process::id() {
        return false;
    }

    // Check class name for Desktop / Explorer shell
    let mut cls = [0u16; 64];
    let cn = GetClassNameW(fg, cls.as_mut_ptr(), 64);
    let class_name = String::from_utf16_lossy(&cls[..cn as usize]);

    let name = if matches!(class_name.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
        "Finder".to_owned()
    } else {
        // First look up clean display name from our synced taskbar apps
        let matched = MODEL.with_borrow(|s| {
            s.items.iter().find_map(|item| {
                let app = item.app.as_ref()?;
                if app.windows.iter().any(|(h, _)| *h == fg as usize) {
                    Some(if item.name == "エクスプローラー" {
                        "Finder".to_owned()
                    } else {
                        item.name.clone()
                    })
                } else {
                    None
                }
            })
        });

        if let Some(n) = matched {
            n
        } else {
            // Query process image name fallback
            let mut exe_stem = String::new();
            let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !proc.is_null() {
                let mut buf = [0u16; 2048];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len) != 0 {
                    let full = String::from_utf16_lossy(&buf[..len as usize]);
                    if let Some(st) = std::path::Path::new(&full)
                        .file_stem()
                        .and_then(|v| v.to_str())
                    {
                        exe_stem = st.to_owned();
                    }
                }
                CloseHandle(proc);
            }
            if !exe_stem.is_empty() {
                friendly_exe_name(&exe_stem)
            } else {
                let mut title = [0u16; 256];
                let tn = GetWindowTextW(fg, title.as_mut_ptr(), 256);
                let t = String::from_utf16_lossy(&title[..tn as usize]);
                if t.is_empty() {
                    "Finder".to_owned()
                } else {
                    t.rsplit(" - ").next().unwrap_or(&t).to_owned()
                }
            }
        }
    };

    // Also query live IME / keyboard layout on the foreground thread (instant, zero polling!)
    let ime_badge = detect_ime(fg, tid);

    // Detect true borderless fullscreen app on the primary monitor (never false-triggers on maximized apps!)
    let is_fullscreen = detect_true_fullscreen(fg, &class_name);
    bar::set_fullscreen(is_fullscreen);
    surface::set_fullscreen(is_fullscreen);
    surface::on_foreground_change();

    let (fg_hwnd_changed, state_changed) = STATE.with_borrow_mut(|s| {
        let hwnd_ch = s.active_hwnd != fg as usize;
        let mut ch = hwnd_ch || s.active_app != name;
        s.active_app = name;
        s.active_hwnd = fg as usize;
        if let Some(badge) = ime_badge {
            if s.ime_badge != badge {
                s.ime_badge = badge;
                ch = true;
            }
        }
        (hwnd_ch, ch)
    });
    // Only sample desktop ambient tone when the foreground window actually switched!
    let tone_changed = if fg_hwnd_changed {
        refresh_ambient()
    } else {
        false
    };
    state_changed || tone_changed
}

unsafe fn detect_ime(fg: HWND, tid: u32) -> Option<String> {
    let hkl = GetKeyboardLayout(tid);
    let lang_id = (hkl as usize & 0xFFFF) as u16;
    if lang_id == 0x0411 {
        // Japanese layout — check if IME is open & Hiragana/Katakana
        let ime_wnd = ImmGetDefaultIMEWnd(fg);
        if !ime_wnd.is_null() {
            let mut open = 0usize;
            let ok = SendMessageTimeoutW(
                ime_wnd,
                WM_IME_CONTROL,
                IMC_GETOPENSTATUS,
                0,
                SMTO_ABORTIFHUNG,
                40,
                &mut open,
            );
            if ok != 0 {
                if open == 0 {
                    return Some("A".into());
                }
                let mut conv = 0usize;
                let ok2 = SendMessageTimeoutW(
                    ime_wnd,
                    WM_IME_CONTROL,
                    IMC_GETCONVERSIONMODE,
                    0,
                    SMTO_ABORTIFHUNG,
                    40,
                    &mut conv,
                );
                if ok2 != 0 {
                    // IME_CMODE_NATIVE = 0x1, IME_CMODE_KATAKANA = 0x2
                    if conv & 0x1 == 0 {
                        return Some("A".into());
                    } else if conv & 0x2 != 0 {
                        return Some("ア".into());
                    } else {
                        return Some("あ".into());
                    }
                }
                return Some("あ".into());
            }
        }
        None
    } else if lang_id == 0x0409 || lang_id == 0x0809 {
        Some("A".into())
    } else if lang_id == 0x0412 {
        Some("한".into())
    } else if lang_id == 0x0804 || lang_id == 0x0404 {
        Some("中".into())
    } else {
        Some("A".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fullscreen_detection_ignores_maximized_desktop_windows() {
        let mon = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        let maximized_with_border = RECT {
            left: -8,
            top: -8,
            right: 2568,
            bottom: 1448,
        };
        assert!(!is_fullscreen_bounds(
            maximized_with_border,
            mon,
            true,
            true
        ));
        assert!(is_fullscreen_bounds(mon, mon, false, false));
        let partial = RECT {
            left: 0,
            top: 30,
            right: 2560,
            bottom: 1440,
        };
        assert!(!is_fullscreen_bounds(partial, mon, false, false));
    }

    #[test]
    fn native_network_probes_real_adapters() {
        unsafe {
            refresh_network();
            let s = state();
            assert!(s.net_probed, "refresh_network did not complete");
            println!(
                "Detected network: sig={} title='{}' detail='{}' adapters={} wifi_nets={}",
                s.wifi_signal,
                s.net_title,
                s.net_detail,
                s.adapters.len(),
                s.wifi_networks.len()
            );
            for a in &s.adapters {
                println!(
                    "  adapter: name='{}' kind={:?} up={} gw={} ip='{}' speed='{}'",
                    a.name, a.kind, a.up, a.has_gateway, a.ipv4, a.speed_label
                );
            }
            // If a physical Ethernet adapter is Up with an IP/gateway, it MUST report signal==4 (Ethernet), never Wi-Fi!
            let has_up_eth = s.adapters.iter().any(|a| {
                a.kind == NetKind::Ethernet && a.up && (a.has_gateway || !a.ipv4.is_empty())
            });
            if has_up_eth {
                assert_eq!(
                    s.wifi_signal, 4,
                    "Active Ethernet must report signal=4 (Ethernet), got {}",
                    s.wifi_signal
                );
            }
        }
    }
}
