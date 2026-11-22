use super::*;
use crate::glass::{GlassStyle, RawIcon};
use std::path::{Path, PathBuf};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

pub struct Item {
    pub name: String,
    pub path: String,
    pub fallback_icon_idx: i32,
    pub custom: bool,
    pub app: Option<tasks::App>,
    pub raw_icon: Option<RawIcon>,
    pub d2d_bitmap: Option<ID2D1Bitmap>,
    pub icon_loaded_size: i32,
}

impl Item {
    // Avoids calling SHGetFileInfoW in-process (which permanently loads 30+ MB of third-party
    // shell icon overlay DLLs like Google Drive / OneDrive and holds idle GDI HICON handles).
    // Crisp high-res icons are loaded cleanly in surface::layout() with shell32 fallback on demand.
    fn new(name: String, path: String, fallback: i32, custom: bool) -> Self {
        Self {
            app: None,
            raw_icon: None,
            d2d_bitmap: None,
            icon_loaded_size: 0,
            name,
            path,
            fallback_icon_idx: fallback,
            custom,
        }
    }
}

pub struct Settings {
    pub size: i32,         // 46 (Small), 54 (Standard), 64 (Large)
    pub glass: GlassStyle, // Clear (Liquid), Standard, Dark
    pub magnify: u8,       // 0=Off, 1=Subtle(1.30x), 2=Standard(1.55x), 3=Strong(1.80x)
    pub autohide: bool,
    pub items: Vec<Item>,
}

impl Settings {
    pub fn mag_boost(&self) -> f32 {
        match self.magnify {
            0 => 0.0,
            1 => 0.30,
            2 => 0.55,
            _ => 0.80,
        }
    }
}

fn location() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("MacUI")
            .join("dock.txt"),
    )
}

fn parse_settings_text(data: &str) -> (i32, GlassStyle, u8, bool, Vec<(Option<String>, String)>) {
    let mut size = 54;
    let mut glass = GlassStyle::Clear;
    let mut magnify = 2u8;
    let mut autohide = false;
    let mut pins = Vec::new();

    let has_glass_key = data.lines().any(|l| l.starts_with("glass="));
    for line in data.lines() {
        if let Some(value) = line.strip_prefix("size=") {
            if let Ok(v) = value.parse::<i32>() {
                size = match v {
                    46 | 52 => 46,
                    54 => 54,
                    64 | 66 | 80 => 64,
                    _ => size,
                };
            }
        }
        if let Some(value) = line.strip_prefix("glass=") {
            if let Ok(v) = value.parse::<usize>() {
                glass = GlassStyle::from_index(v);
            }
        } else if !has_glass_key {
            if let Some(value) = line.strip_prefix("alpha=") {
                if let Ok(v) = value.parse::<u8>() {
                    // Migrate legacy default alpha=222 cleanly to Clear Liquid Glass
                    glass = if v >= 240 {
                        GlassStyle::Dark
                    } else {
                        GlassStyle::Clear
                    };
                }
            }
        }
        if let Some(value) = line.strip_prefix("magnify=") {
            if let Ok(v @ 0..=3) = value.parse::<u8>() {
                magnify = v;
            }
        }
        if let Some(value) = line.strip_prefix("autohide=") {
            autohide = value == "1" || value == "true";
        }
        if let Some(raw) = line.strip_prefix("pin=") {
            if let Some((name, path)) = raw.split_once('\t') {
                pins.push((Some(name.to_owned()), path.to_owned()));
            } else {
                pins.push((None, raw.to_owned()));
            }
        }
    }
    (size, glass, magnify, autohide, pins)
}

impl Settings {
    pub unsafe fn load() -> Self {
        let mut s = Self {
            size: 54,
            glass: GlassStyle::Clear,
            magnify: 2,
            autohide: false,
            items: Vec::new(),
        };
        if let Some(data) = location().and_then(|p| std::fs::read_to_string(p).ok()) {
            let (size, glass, magnify, autohide, pins) = parse_settings_text(&data);
            s.size = size;
            s.glass = glass;
            s.magnify = magnify;
            s.autohide = autohide;
            for (name, path) in pins {
                match name {
                    Some(n) if !n.is_empty() => {
                        s.pin_named(&n, &path);
                    }
                    _ => {
                        s.pin(&path);
                    }
                }
            }
        }
        s
    }

    pub unsafe fn pin(&mut self, path: &str) -> bool {
        let name = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        self.pin_named(&name, path)
    }

    pub unsafe fn pin_named(&mut self, display_name: &str, path: &str) -> bool {
        if path.is_empty()
            || self.items.iter().filter(|i| i.custom).count() >= 6
            || self.items.iter().any(|i| i.path.eq_ignore_ascii_case(path))
        {
            return false;
        }
        let clean = display_name.replace(['\t', '\r', '\n'], " ");
        let name = if clean.trim().is_empty() {
            Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(path)
                .to_string()
        } else {
            clean.trim().to_owned()
        };
        self.items.push(Item::new(name, path.into(), 0, true));
        true
    }

    pub unsafe fn sync(&mut self, apps: Vec<tasks::App>) -> bool {
        if self
            .items
            .iter()
            .filter_map(|i| i.app.as_ref())
            .eq(apps.iter())
        {
            return false;
        }
        let mut old = std::mem::take(&mut self.items);
        for app in apps {
            let mut item = if let Some(index) = old.iter().position(|i| {
                i.app
                    .as_ref()
                    .is_some_and(|a| a.id == app.id && a.image == app.image)
            }) {
                old.remove(index)
            } else {
                Item::new(app.name.clone(), app.image.clone(), 2, false)
            };
            item.name = app.name.clone();
            item.path = app.launch.clone();
            item.app = Some(app);
            self.items.push(item);
        }
        self.items.extend(old.into_iter().filter(|i| i.custom));
        true
    }

    pub fn save(&self) -> bool {
        let Some(file) = location() else { return false };
        let temp = file.with_extension("tmp");
        let mut data = format!(
            "size={}\nglass={}\nalpha={}\nmagnify={}\nautohide={}\n",
            self.size,
            self.glass.index(),
            self.glass.legacy_alpha(),
            self.magnify,
            if self.autohide { 1 } else { 0 }
        );
        for item in self.items.iter().filter(|i| i.custom) {
            let clean_name = item.name.replace(['\t', '\r', '\n'], " ");
            if !clean_name.is_empty() {
                data.push_str(&format!("pin={}\t{}\n", clean_name, item.path));
            } else {
                data.push_str(&format!("pin={}\n", item.path));
            }
        }
        std::fs::create_dir_all(file.parent().unwrap()).is_ok()
            && std::fs::write(&temp, data).is_ok()
            && unsafe {
                MoveFileExW(
                    w(&temp.to_string_lossy()).as_ptr(),
                    w(&file.to_string_lossy()).as_ptr(),
                    MOVEFILE_REPLACE_EXISTING,
                ) != 0
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parser_preserves_size_64_and_named_uwp_pins() {
        let raw = "\
size=64
glass=1
magnify=3
autohide=1
pin=電卓\tshell:AppsFolder\\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App
pin=C:\\Windows\\notepad.exe
";
        let (size, glass, magnify, autohide, pins) = parse_settings_text(raw);
        assert_eq!(size, 64, "Dock size 大 (64) must persist across reloads");
        assert_eq!(glass, GlassStyle::Standard);
        assert_eq!(magnify, 3);
        assert!(autohide);
        assert_eq!(pins.len(), 2);
        assert_eq!(
            pins[0],
            (
                Some("電卓".to_owned()),
                "shell:AppsFolder\\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_owned()
            )
        );
        assert_eq!(pins[1], (None, "C:\\Windows\\notepad.exe".to_owned()));

        let (sz_s, ..) = parse_settings_text("size=46\n");
        let (sz_m, ..) = parse_settings_text("size=54\n");
        assert_eq!(sz_s, 46);
        assert_eq!(sz_m, 54);
    }
}
