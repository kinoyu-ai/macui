use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use windows::{
    core::{implement, Interface, GUID, HSTRING, PCWSTR},
    Foundation::{IPropertyValue, Numerics::*, PropertyValue, Size},
    Graphics::{
        DirectX::{DirectXAlphaMode, DirectXPixelFormat},
        Effects::{
            IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectSource_Impl,
            IGraphicsEffect_Impl,
        },
    },
    System::DispatcherQueueController,
    Win32::{
        Foundation::{E_INVALIDARG, HWND as WH, POINT},
        Graphics::{
            Direct2D::{
                CLSID_D2D1ArithmeticComposite, CLSID_D2D1GaussianBlur, CLSID_D2D1Saturation,
                Common::*, D2D1CreateFactory, ID2D1Bitmap, ID2D1Device, ID2D1DeviceContext,
                ID2D1Factory, ID2D1Factory1, ID2D1LinearGradientBrush, ID2D1RadialGradientBrush,
                ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ARC_SEGMENT,
                D2D1_ARC_SIZE_SMALL, D2D1_BITMAP_PROPERTIES, D2D1_CAP_STYLE_ROUND,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP,
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_GAMMA_2_2,
                D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED,
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES,
                D2D1_LINE_JOIN_ROUND, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
                D2D1_STROKE_STYLE_PROPERTIES, D2D1_SWEEP_DIRECTION_CLOCKWISE,
            },
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_SDK_VERSION,
            },
            DirectWrite::{
                DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
                DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
                DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
                DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
                DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING,
                DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
            },
            Dxgi::{Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice},
            Gdi::{
                CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP,
                BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP as WHBITMAP,
            },
        },
        System::WinRT::{
            Composition::{
                ICompositionDrawingSurfaceInterop, ICompositorDesktopInterop, ICompositorInterop,
            },
            CreateDispatcherQueueController, DispatcherQueueOptions,
            Graphics::Direct2D::{
                IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl,
                GRAPHICS_EFFECT_PROPERTY_MAPPING, GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
            },
            DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
        },
        UI::WindowsAndMessaging::{GetIconInfo, HICON as WHICON},
    },
    UI::Composition::{
        CompositionBrush, CompositionDrawingSurface, CompositionGeometricClip,
        CompositionGraphicsDevice, CompositionRoundedRectangleGeometry, CompositionSurfaceBrush,
        Compositor, ContainerVisual, Desktop::DesktopWindowTarget, SpriteVisual,
    },
};
use windows_sys::Win32::Foundation::HWND;

/// Live ambient background color & luminance sampled behind a Liquid Glass surface.
/// Enables Apple HIG adaptive polarity (light ink on dark/vibrant backgrounds, dark ink
/// on bright backgrounds) and harmonic background-color tinting across glass glazes & edges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientTone {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub luma: f32,
}

impl Default for AmbientTone {
    fn default() -> Self {
        Self {
            r: 0.22,
            g: 0.28,
            b: 0.38,
            luma: 0.28,
        }
    }
}

impl AmbientTone {
    pub fn from_rgb(r: f32, g: f32, b: f32) -> Self {
        let r = r.clamp(0.0, 1.0);
        let g = g.clamp(0.0, 1.0);
        let b = b.clamp(0.0, 1.0);
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        Self { r, g, b, luma }
    }

    pub fn is_light(self, style: GlassStyle) -> bool {
        style != GlassStyle::Dark && self.luma > 0.62
    }

    /// Amplifies background chroma gently toward luminous crystal highlights.
    pub fn light_tint(self) -> (f32, f32, f32) {
        let avg = (self.r + self.g + self.b) / 3.0;
        let boost = 1.45;
        let sr = (avg + (self.r - avg) * boost).clamp(0.0, 1.0);
        let sg = (avg + (self.g - avg) * boost).clamp(0.0, 1.0);
        let sb = (avg + (self.b - avg) * boost).clamp(0.0, 1.0);
        // Lift softly toward crystal brilliance while preserving background hue
        (
            (0.62 + sr * 0.38).min(1.0),
            (0.62 + sg * 0.38).min(1.0),
            (0.62 + sb * 0.38).min(1.0),
        )
    }

    /// Deep chromatic base infused with the background hue (replaces muddy fixed gray).
    pub fn dark_tint(self) -> (f32, f32, f32) {
        let avg = (self.r + self.g + self.b) / 3.0;
        let boost = 1.55;
        let sr = (avg + (self.r - avg) * boost).clamp(0.0, 1.0);
        let sg = (avg + (self.g - avg) * boost).clamp(0.0, 1.0);
        let sb = (avg + (self.b - avg) * boost).clamp(0.0, 1.0);
        (
            (0.05 + sr * 0.32).min(0.45),
            (0.06 + sg * 0.32).min(0.45),
            (0.08 + sb * 0.32).min(0.45),
        )
    }
}

/// Dual-scale chromatic optical pipeline parameters:
/// (detail_sigma, detail_sat, bloom_sigma, bloom_sat, [k1, k2, k3, k4])
#[derive(Clone, Copy)]
struct OpticalSpec {
    detail_sigma: f32,
    detail_sat: f32,
    bloom_sigma: f32,
    bloom_sat: f32,
    coeffs: [f32; 4],
}

/// Apple HIG Liquid Glass variants:
/// - Clear (`.clear`): high-clarity refractive crystal with vivid background color infusion
/// - Standard (`.regular`): balanced translucent Liquid Glass
/// - Dark (`.tinted` / high contrast): deeper smoked glass with subtle background color warmth
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlassStyle {
    Clear = 0,
    Standard = 1,
    Dark = 2,
}

impl GlassStyle {
    pub fn from_index(i: usize) -> Self {
        match i {
            0 => Self::Clear,
            2 => Self::Dark,
            _ => Self::Standard,
        }
    }
    pub fn index(self) -> usize {
        self as usize
    }
    pub fn legacy_alpha(self) -> u8 {
        match self {
            Self::Clear => 185,
            Self::Standard => 222,
            Self::Dark => 250,
        }
    }
    fn dock_spec(self) -> OpticalSpec {
        match self {
            Self::Clear => OpticalSpec {
                detail_sigma: 10.5,
                detail_sat: 1.55,
                bloom_sigma: 46.0,
                bloom_sat: 1.95,
                coeffs: [0.22, 0.56, 0.36, 0.01],
            },
            Self::Standard => OpticalSpec {
                detail_sigma: 18.0,
                detail_sat: 1.45,
                bloom_sigma: 52.0,
                bloom_sat: 1.88,
                coeffs: [0.18, 0.52, 0.38, 0.0],
            },
            Self::Dark => OpticalSpec {
                detail_sigma: 24.0,
                detail_sat: 1.32,
                bloom_sigma: 54.0,
                bloom_sat: 1.65,
                coeffs: [0.12, 0.44, 0.28, -0.04],
            },
        }
    }
    fn panel_spec(self) -> OpticalSpec {
        match self {
            Self::Clear => OpticalSpec {
                detail_sigma: 14.0,
                detail_sat: 1.52,
                bloom_sigma: 48.0,
                bloom_sat: 1.95,
                coeffs: [0.18, 0.52, 0.38, 0.0],
            },
            Self::Standard => OpticalSpec {
                detail_sigma: 22.0,
                detail_sat: 1.44,
                bloom_sigma: 52.0,
                bloom_sat: 1.85,
                coeffs: [0.15, 0.48, 0.38, -0.01],
            },
            Self::Dark => OpticalSpec {
                detail_sigma: 28.0,
                detail_sat: 1.28,
                bloom_sigma: 56.0,
                bloom_sat: 1.60,
                coeffs: [0.10, 0.38, 0.28, -0.05],
            },
        }
    }
    fn bar_spec(self) -> OpticalSpec {
        // Always frosted Liquid Glass across ALL styles (never raw see-through wallpaper!)
        match self {
            Self::Clear => OpticalSpec {
                detail_sigma: 17.0,
                detail_sat: 1.55,
                bloom_sigma: 50.0,
                bloom_sat: 1.95,
                coeffs: [0.18, 0.50, 0.40, 0.01],
            },
            Self::Standard => OpticalSpec {
                detail_sigma: 24.0,
                detail_sat: 1.45,
                bloom_sigma: 54.0,
                bloom_sat: 1.88,
                coeffs: [0.15, 0.46, 0.40, 0.0],
            },
            Self::Dark => OpticalSpec {
                detail_sigma: 28.0,
                detail_sat: 1.30,
                bloom_sigma: 56.0,
                bloom_sat: 1.62,
                coeffs: [0.12, 0.38, 0.28, -0.04],
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    Dock,
    Panel,
    Bar,
}

// ---------- Custom COM D2D1 Effect Nodes for Windows.UI.Composition ----------

#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct BlurEffect {
    name: RefCell<HSTRING>,
    source: IGraphicsEffectSource,
    sigma: f32,
}

impl IGraphicsEffectSource_Impl for BlurEffect_Impl {}
impl IGraphicsEffect_Impl for BlurEffect_Impl {
    fn Name(&self) -> windows::core::Result<HSTRING> {
        Ok(self.name.borrow().clone())
    }
    fn SetName(&self, name: &HSTRING) -> windows::core::Result<()> {
        *self.name.borrow_mut() = name.clone();
        Ok(())
    }
}
impl IGraphicsEffectD2D1Interop_Impl for BlurEffect_Impl {
    fn GetEffectId(&self) -> windows::core::Result<GUID> {
        Ok(CLSID_D2D1GaussianBlur)
    }
    fn GetNamedPropertyMapping(
        &self,
        name: &PCWSTR,
        index: *mut u32,
        mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> windows::core::Result<()> {
        let s = unsafe { name.to_string().unwrap_or_default() };
        if s == "BlurAmount" {
            unsafe {
                *index = 0;
                *mapping = GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT;
            }
            Ok(())
        } else {
            Err(windows::core::Error::from(E_INVALIDARG))
        }
    }
    fn GetPropertyCount(&self) -> windows::core::Result<u32> {
        Ok(3)
    }
    fn GetProperty(&self, index: u32) -> windows::core::Result<IPropertyValue> {
        match index {
            0 => PropertyValue::CreateSingle(self.sigma)?.cast(),
            1 => PropertyValue::CreateUInt32(D2D1_GAUSSIANBLUR_OPTIMIZATION_BALANCED.0 as u32)?
                .cast(),
            2 => PropertyValue::CreateUInt32(1u32)?.cast(), // D2D1_BORDER_MODE_HARD
            _ => Err(windows::core::Error::from(E_INVALIDARG)),
        }
    }
    fn GetSource(&self, index: u32) -> windows::core::Result<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(windows::core::Error::from(E_INVALIDARG))
        }
    }
    fn GetSourceCount(&self) -> windows::core::Result<u32> {
        Ok(1)
    }
}

#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct SaturationEffect {
    name: RefCell<HSTRING>,
    source: IGraphicsEffectSource,
    saturation: f32,
}

impl IGraphicsEffectSource_Impl for SaturationEffect_Impl {}
impl IGraphicsEffect_Impl for SaturationEffect_Impl {
    fn Name(&self) -> windows::core::Result<HSTRING> {
        Ok(self.name.borrow().clone())
    }
    fn SetName(&self, name: &HSTRING) -> windows::core::Result<()> {
        *self.name.borrow_mut() = name.clone();
        Ok(())
    }
}
impl IGraphicsEffectD2D1Interop_Impl for SaturationEffect_Impl {
    fn GetEffectId(&self) -> windows::core::Result<GUID> {
        Ok(CLSID_D2D1Saturation)
    }
    fn GetNamedPropertyMapping(
        &self,
        name: &PCWSTR,
        index: *mut u32,
        mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> windows::core::Result<()> {
        let s = unsafe { name.to_string().unwrap_or_default() };
        if s == "Saturation" {
            unsafe {
                *index = 0;
                *mapping = GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT;
            }
            Ok(())
        } else {
            Err(windows::core::Error::from(E_INVALIDARG))
        }
    }
    fn GetPropertyCount(&self) -> windows::core::Result<u32> {
        Ok(1)
    }
    fn GetProperty(&self, index: u32) -> windows::core::Result<IPropertyValue> {
        match index {
            0 => PropertyValue::CreateSingle(self.saturation)?.cast(),
            _ => Err(windows::core::Error::from(E_INVALIDARG)),
        }
    }
    fn GetSource(&self, index: u32) -> windows::core::Result<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(windows::core::Error::from(E_INVALIDARG))
        }
    }
    fn GetSourceCount(&self) -> windows::core::Result<u32> {
        Ok(1)
    }
}

#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct ArithmeticCompositeEffect {
    name: RefCell<HSTRING>,
    source1: IGraphicsEffectSource,
    source2: IGraphicsEffectSource,
    coeffs: [f32; 4],
}

impl IGraphicsEffectSource_Impl for ArithmeticCompositeEffect_Impl {}
impl IGraphicsEffect_Impl for ArithmeticCompositeEffect_Impl {
    fn Name(&self) -> windows::core::Result<HSTRING> {
        Ok(self.name.borrow().clone())
    }
    fn SetName(&self, name: &HSTRING) -> windows::core::Result<()> {
        *self.name.borrow_mut() = name.clone();
        Ok(())
    }
}
impl IGraphicsEffectD2D1Interop_Impl for ArithmeticCompositeEffect_Impl {
    fn GetEffectId(&self) -> windows::core::Result<GUID> {
        Ok(CLSID_D2D1ArithmeticComposite)
    }
    fn GetNamedPropertyMapping(
        &self,
        _name: &PCWSTR,
        _index: *mut u32,
        _mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_INVALIDARG))
    }
    fn GetPropertyCount(&self) -> windows::core::Result<u32> {
        Ok(2)
    }
    fn GetProperty(&self, index: u32) -> windows::core::Result<IPropertyValue> {
        match index {
            0 => PropertyValue::CreateSingleArray(&self.coeffs)?.cast(),
            1 => PropertyValue::CreateBoolean(true)?.cast(), // clamp output
            _ => Err(windows::core::Error::from(E_INVALIDARG)),
        }
    }
    fn GetSource(&self, index: u32) -> windows::core::Result<IGraphicsEffectSource> {
        match index {
            0 => Ok(self.source1.clone()),
            1 => Ok(self.source2.clone()),
            _ => Err(windows::core::Error::from(E_INVALIDARG)),
        }
    }
    fn GetSourceCount(&self) -> windows::core::Result<u32> {
        Ok(2)
    }
}

pub struct GlassEngine {
    _dq: DispatcherQueueController,
    pub compositor: Compositor,
    _d3d: ID3D11Device,
    pub d2d_factory: ID2D1Factory1,
    _d2d_device: ID2D1Device,
    pub gfx_device: CompositionGraphicsDevice,
    pub dwrite: IDWriteFactory,
    host_backdrop: Option<windows::UI::Composition::CompositionBackdropBrush>,
    dock_brushes: RefCell<[Option<CompositionBrush>; 3]>,
    panel_brushes: RefCell<[Option<CompositionBrush>; 3]>,
    bar_brushes: RefCell<[Option<CompositionBrush>; 3]>,
    text_formats: RefCell<HashMap<(u32, u8, u8), IDWriteTextFormat>>,
    round_stroke: Option<ID2D1StrokeStyle>,
    ui_font: HSTRING,
    ui_locale: HSTRING,
}

impl GlassEngine {
    pub fn brush_for(&self, role: SurfaceRole, style: GlassStyle) -> Option<CompositionBrush> {
        let idx = style.index();
        let slot_cell = match role {
            SurfaceRole::Dock => &self.dock_brushes,
            SurfaceRole::Panel => &self.panel_brushes,
            SurfaceRole::Bar => &self.bar_brushes,
        };
        if let Some(existing) = slot_cell.borrow()[idx].clone() {
            return Some(existing);
        }
        let spec = match role {
            SurfaceRole::Dock => style.dock_spec(),
            SurfaceRole::Panel => style.panel_spec(),
            SurfaceRole::Bar => style.bar_spec(),
        };
        let built = build_optical_brush(&self.compositor, self.host_backdrop.as_ref(), spec);
        if let Some(b) = &built {
            slot_cell.borrow_mut()[idx] = Some(b.clone());
        }
        built
    }
}

thread_local! {
    static ENGINE: RefCell<Option<GlassEngine>> = const { RefCell::new(None) };
}

fn compile_effect_brush(
    compositor: &Compositor,
    effect: &IGraphicsEffect,
    sources: &[(&str, &windows::UI::Composition::CompositionBackdropBrush)],
) -> Option<CompositionBrush> {
    let factory = compositor.CreateEffectFactory(effect).ok()?;
    if !factory
        .ExtendedError()
        .map(|hr| hr.is_ok())
        .unwrap_or(false)
    {
        return None;
    }
    let brush = factory.CreateBrush().ok()?;
    for (name, src) in sources {
        brush.SetSourceParameter(&HSTRING::from(*name), *src).ok()?;
    }
    brush.cast().ok()
}

/// Builds a single seamless GPU-composited Dual-Scale Chromatic Liquid Glass brush:
///   detail = Saturation(detail_sat, GaussianBlur(detail_sigma, BackdropDet))
///   bloom  = Saturation(bloom_sat,  GaussianBlur(bloom_sigma,  BackdropBlm))
///   fused  = ArithmeticComposite(detail, bloom, [k1, k2, k3, k4])
/// Note: Windows.UI.Composition requires strict tree-shaped graphs (no shared DAG nodes),
/// so we bind two separate source parameters ("BackdropDet" & "BackdropBlm") to HostBackdropBrush.
fn build_optical_brush(
    compositor: &Compositor,
    shared_host: Option<&windows::UI::Composition::CompositionBackdropBrush>,
    spec: OpticalSpec,
) -> Option<CompositionBrush> {
    let owned_host;
    let host = if let Some(h) = shared_host {
        h
    } else {
        owned_host = compositor.CreateHostBackdropBrush().ok()?;
        &owned_host
    };
    let param_det = windows::UI::Composition::CompositionEffectSourceParameter::Create(
        &HSTRING::from("BackdropDet"),
    )
    .ok()?;
    let param_blm = windows::UI::Composition::CompositionEffectSourceParameter::Create(
        &HSTRING::from("BackdropBlm"),
    )
    .ok()?;
    let src_det: IGraphicsEffectSource = param_det.cast().ok()?;
    let src_blm: IGraphicsEffectSource = param_blm.cast().ok()?;

    // Primary: Tree-shaped Dual-Scale Chromatic Liquid Glass fusion (detail refraction + wide color bloom)
    let b_det: IGraphicsEffectSource = BlurEffect {
        name: RefCell::new(HSTRING::from("BlurDet")),
        source: src_det.clone(),
        sigma: spec.detail_sigma,
    }
    .into();
    let s_det: IGraphicsEffectSource = SaturationEffect {
        name: RefCell::new(HSTRING::from("SatDet")),
        source: b_det,
        saturation: spec.detail_sat,
    }
    .into();
    let b_blm: IGraphicsEffectSource = BlurEffect {
        name: RefCell::new(HSTRING::from("BlurBlm")),
        source: src_blm,
        sigma: spec.bloom_sigma,
    }
    .into();
    let s_blm: IGraphicsEffectSource = SaturationEffect {
        name: RefCell::new(HSTRING::from("SatBlm")),
        source: b_blm,
        saturation: spec.bloom_sat,
    }
    .into();
    let fused: IGraphicsEffect = ArithmeticCompositeEffect {
        name: RefCell::new(HSTRING::from("LiquidFuse")),
        source1: s_det,
        source2: s_blm,
        coeffs: spec.coeffs,
    }
    .into();
    if let Some(brush) = compile_effect_brush(
        compositor,
        &fused,
        &[("BackdropDet", host), ("BackdropBlm", host)],
    ) {
        return Some(brush);
    }

    // Fallback 1: GaussianBlur -> Saturation
    let b2: IGraphicsEffectSource = BlurEffect {
        name: RefCell::new(HSTRING::from("Blur")),
        source: src_det.clone(),
        sigma: spec.detail_sigma,
    }
    .into();
    let sat_only: IGraphicsEffect = SaturationEffect {
        name: RefCell::new(HSTRING::from("Sat")),
        source: b2,
        saturation: spec.detail_sat,
    }
    .into();
    if let Some(brush) = compile_effect_brush(compositor, &sat_only, &[("BackdropDet", host)]) {
        return Some(brush);
    }

    // Fallback 2: GaussianBlur only
    let blur_only: IGraphicsEffect = BlurEffect {
        name: RefCell::new(HSTRING::from("Blur")),
        source: src_det,
        sigma: spec.detail_sigma,
    }
    .into();
    if let Some(brush) = compile_effect_brush(compositor, &blur_only, &[("BackdropDet", host)]) {
        return Some(brush);
    }

    // Fallback 3: Raw HostBackdropBrush
    host.cast().ok()
}

pub unsafe fn init() -> bool {
    ENGINE.with_borrow_mut(|slot| {
        if slot.is_some() {
            return true;
        }
        let dq = match CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_NONE,
        }) {
            Ok(d) => d,
            Err(_) => return false,
        };
        let compositor = match Compositor::new() {
            Ok(c) => c,
            Err(_) => return false,
        };
        let mut d3d_opt: Option<ID3D11Device> = None;
        let levels = [D3D_FEATURE_LEVEL_11_0];
        let hr = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut d3d_opt),
            None,
            None,
        );
        if hr.is_err() {
            let _ = D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut d3d_opt),
                None,
                None,
            );
        }
        let Some(d3d) = d3d_opt else {
            return false;
        };
        let Ok(dxgi) = d3d.cast::<IDXGIDevice>() else {
            return false;
        };
        let Ok(d2d_factory) =
            D2D1CreateFactory::<ID2D1Factory1>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
        else {
            return false;
        };
        let Ok(d2d_device) = d2d_factory.CreateDevice(&dxgi) else {
            return false;
        };
        let Ok(interop) = compositor.cast::<ICompositorInterop>() else {
            return false;
        };
        let Ok(gfx_device) = interop.CreateGraphicsDevice(&d2d_device) else {
            return false;
        };
        let Ok(dwrite) = DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) else {
            return false;
        };

        let host_backdrop = compositor.CreateHostBackdropBrush().ok();
        let stroke_props = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            dashCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_ROUND,
            miterLimit: 4.0,
            ..zeroed()
        };
        let round_stroke = ID2D1Factory::CreateStrokeStyle(&d2d_factory, &stroke_props, None).ok();

        // Pick the best installed UI font on this PC (Yu Gothic UI on Japanese Windows,
        // gracefully falling back to Segoe UI Variable / Segoe UI on non-Japanese systems).
        let mut ui_font = HSTRING::from("Segoe UI");
        let mut ui_locale = HSTRING::from("");
        let mut font_col = None;
        if dwrite.GetSystemFontCollection(&mut font_col, false).is_ok() {
            if let Some(col) = font_col {
                for (cand, loc) in [
                    ("Yu Gothic UI", "ja-JP"),
                    ("Segoe UI Variable", ""),
                    ("Segoe UI", ""),
                ] {
                    let mut idx = 0u32;
                    let mut exists = windows::Win32::Foundation::BOOL(0);
                    if col
                        .FindFamilyName(&HSTRING::from(cand), &mut idx, &mut exists)
                        .is_ok()
                        && exists.as_bool()
                    {
                        ui_font = HSTRING::from(cand);
                        ui_locale = HSTRING::from(loc);
                        break;
                    }
                }
            }
        }

        *slot = Some(GlassEngine {
            _dq: dq,
            compositor,
            _d3d: d3d,
            d2d_factory,
            _d2d_device: d2d_device,
            gfx_device,
            dwrite,
            host_backdrop,
            dock_brushes: RefCell::new([None, None, None]),
            panel_brushes: RefCell::new([None, None, None]),
            bar_brushes: RefCell::new([None, None, None]),
            text_formats: RefCell::new(HashMap::with_capacity(24)),
            round_stroke,
            ui_font,
            ui_locale,
        });
        true
    })
}

pub fn with_engine<R>(f: impl FnOnce(&GlassEngine) -> R) -> Option<R> {
    ENGINE.with_borrow(|e| e.as_ref().map(f))
}

pub fn shutdown() {
    ENGINE.with_borrow_mut(|slot| {
        *slot = None;
    });
}

/// Single seamless GPU Liquid Glass backdrop (zero stepped multi-layer rings!)
pub struct PillBackdrop {
    pub visual: SpriteVisual,
    geom: CompositionRoundedRectangleGeometry,
}

impl PillBackdrop {
    pub fn new(compositor: &Compositor, role: SurfaceRole, style: GlassStyle) -> Option<Self> {
        let visual = compositor.CreateSpriteVisual().ok()?;
        let geom = compositor.CreateRoundedRectangleGeometry().ok()?;
        let clip: CompositionGeometricClip =
            compositor.CreateGeometricClipWithGeometry(&geom).ok()?;
        visual.SetClip(&clip).ok()?;

        if let Some(Some(b)) = with_engine(|e| e.brush_for(role, style)) {
            let _ = visual.SetBrush(&b);
        }

        Some(Self { visual, geom })
    }

    pub fn set_style(&self, style: GlassStyle, role: SurfaceRole) {
        if let Some(Some(b)) = with_engine(|e| e.brush_for(role, style)) {
            let _ = self.visual.SetBrush(&b);
        }
    }

    pub fn set_rect(&self, x: f32, y: f32, w: f32, h: f32, radius: f32, opacity: f32) {
        let w = w.max(0.0);
        let h = h.max(0.0);
        let visible = opacity > 0.005 && w > 1.0 && h > 1.0;
        let _ = self.visual.SetIsVisible(visible);
        let _ = self.visual.SetOpacity(opacity);
        if !visible {
            return;
        }
        let r = radius.min(w * 0.5).min(h * 0.5).max(0.0);
        let _ = self.visual.SetOffset(Vector3::new(x, y, 0.0));
        let _ = self.visual.SetSize(Vector2::new(w, h));
        let _ = self.geom.SetSize(Vector2::new(w, h));
        let _ = self.geom.SetCornerRadius(Vector2::new(r, r));
    }
}

pub struct GlassSurface {
    _target: DesktopWindowTarget,
    pub root: ContainerVisual,
    pub main_backdrop: PillBackdrop,
    pub extra_backdrops: Vec<(PillBackdrop, SurfaceRole)>,
    fg_visual: SpriteVisual,
    fg_brush: CompositionSurfaceBrush,
    surface: Option<CompositionDrawingSurface>,
    width: i32,
    height: i32,
    role: SurfaceRole,
}

impl GlassSurface {
    pub unsafe fn new(
        hwnd: HWND,
        role: SurfaceRole,
        extra_roles: &[SurfaceRole],
        style: GlassStyle,
    ) -> Option<Self> {
        // Required on Windows 11 so classic Win32 HWNDs sample live screen pixels behind them!
        let enable: i32 = 1;
        let _ = windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            17, // DWMWA_USE_HOSTBACKDROPBRUSH
            &enable as *const i32 as *const _,
            size_of::<i32>() as u32,
        );

        let (target, root, main_backdrop, extra_backdrops, fg_visual, fg_brush) =
            with_engine(|e| -> Option<_> {
                let desktop: ICompositorDesktopInterop = e.compositor.cast().ok()?;
                let target = desktop.CreateDesktopWindowTarget(WH(hwnd), true).ok()?;
                let root = e.compositor.CreateContainerVisual().ok()?;
                target.SetRoot(&root).ok()?;

                let main_backdrop = PillBackdrop::new(&e.compositor, role, style)?;
                root.Children()
                    .ok()?
                    .InsertAtTop(&main_backdrop.visual)
                    .ok()?;

                let mut extras = Vec::with_capacity(extra_roles.len());
                for &erole in extra_roles {
                    let b = PillBackdrop::new(&e.compositor, erole, style)?;
                    b.set_rect(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
                    root.Children().ok()?.InsertAtTop(&b.visual).ok()?;
                    extras.push((b, erole));
                }

                let fg_visual = e.compositor.CreateSpriteVisual().ok()?;
                let fg_brush = e.compositor.CreateSurfaceBrush().ok()?;
                fg_visual.SetBrush(&fg_brush).ok()?;
                root.Children().ok()?.InsertAtTop(&fg_visual).ok()?;

                Some((target, root, main_backdrop, extras, fg_visual, fg_brush))
            })??;

        Some(Self {
            _target: target,
            root,
            main_backdrop,
            extra_backdrops,
            fg_visual,
            fg_brush,
            surface: None,
            width: 0,
            height: 0,
            role,
        })
    }

    pub fn set_style(&self, style: GlassStyle) {
        self.main_backdrop.set_style(style, self.role);
        for (extra, erole) in &self.extra_backdrops {
            extra.set_style(style, *erole);
        }
    }

    /// GPU-composited transform on the root ContainerVisual (animates both live Liquid Glass
    /// HostBackdropBrush refraction and Direct2D content in exact lockstep at 60-120fps!).
    pub fn set_transform(&self, cx: f32, cy: f32, scale: f32, offset_y: f32, opacity: f32) {
        let _ = self.root.SetCenterPoint(Vector3::new(cx, cy, 0.0));
        let _ = self.root.SetScale(Vector3::new(scale, scale, 1.0));
        let _ = self.root.SetOffset(Vector3::new(0.0, offset_y, 0.0));
        let _ = self.root.SetOpacity(opacity.clamp(0.0, 1.0));
    }

    pub unsafe fn ensure_size(&mut self, w: i32, h: i32) -> bool {
        let w = w.max(1);
        let h = h.max(1);
        if self.width == w && self.height == h && self.surface.is_some() {
            return true;
        }
        let ok = with_engine(|e| -> Option<()> {
            let _ = self.root.SetSize(Vector2::new(w as f32, h as f32));
            let _ = self.fg_visual.SetSize(Vector2::new(w as f32, h as f32));
            if let Some(surf) = &self.surface {
                let interop: ICompositionDrawingSurfaceInterop = surf.cast().ok()?;
                if interop
                    .Resize(windows::Win32::Foundation::SIZE { cx: w, cy: h })
                    .is_ok()
                {
                    return Some(());
                }
            }
            let surf = e
                .gfx_device
                .CreateDrawingSurface(
                    Size {
                        Width: w as f32,
                        Height: h as f32,
                    },
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    DirectXAlphaMode::Premultiplied,
                )
                .ok()?;
            self.fg_brush.SetSurface(&surf).ok()?;
            self.surface = Some(surf);
            Some(())
        })
        .flatten()
        .is_some();
        if ok {
            self.width = w;
            self.height = h;
        }
        ok
    }

    pub unsafe fn draw<F>(&mut self, w: i32, h: i32, draw_fn: F)
    where
        F: FnOnce(&ID2D1DeviceContext, f32, f32),
    {
        if !self.ensure_size(w, h) {
            return;
        }
        for attempt in 0..2 {
            let Some(surf) = &self.surface else {
                return;
            };
            let Ok(interop) = surf.cast::<ICompositionDrawingSurfaceInterop>() else {
                return;
            };
            let mut offset = POINT::default();
            match interop.BeginDraw::<ID2D1DeviceContext>(None, &mut offset) {
                Ok(ctx) => {
                    ctx.SetDpi(96.0, 96.0);
                    ctx.SetTransform(&Matrix3x2::translation(offset.x as f32, offset.y as f32));
                    ctx.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
                    draw_fn(&ctx, w as f32, h as f32);
                    let _ = interop.EndDraw();
                    return;
                }
                Err(_) if attempt == 0 => {
                    // Recover automatically if DWM/Composition invalidated the drawing surface
                    self.surface = None;
                    self.width = 0;
                    self.height = 0;
                    if !self.ensure_size(w, h) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }
}

// ---------- Direct2D Color & Brush Helpers ----------

#[inline]
pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

#[inline]
pub fn d2d_rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

#[inline]
pub fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}

pub unsafe fn solid(ctx: &ID2D1DeviceContext, color: D2D1_COLOR_F) -> Option<ID2D1SolidColorBrush> {
    ctx.CreateSolidColorBrush(&color, None).ok()
}

pub unsafe fn linear_grad(
    ctx: &ID2D1DeviceContext,
    start: D2D_POINT_2F,
    end: D2D_POINT_2F,
    stops: &[D2D1_GRADIENT_STOP],
) -> Option<ID2D1LinearGradientBrush> {
    let col = ID2D1RenderTarget::CreateGradientStopCollection(
        ctx,
        stops,
        D2D1_GAMMA_2_2,
        D2D1_EXTEND_MODE_CLAMP,
    )
    .ok()?;
    ctx.CreateLinearGradientBrush(
        &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: start,
            endPoint: end,
        },
        None,
        &col,
    )
    .ok()
}

pub unsafe fn radial_grad(
    ctx: &ID2D1DeviceContext,
    center: D2D_POINT_2F,
    rx: f32,
    ry: f32,
    stops: &[D2D1_GRADIENT_STOP],
) -> Option<ID2D1RadialGradientBrush> {
    let col = ID2D1RenderTarget::CreateGradientStopCollection(
        ctx,
        stops,
        D2D1_GAMMA_2_2,
        D2D1_EXTEND_MODE_CLAMP,
    )
    .ok()?;
    ctx.CreateRadialGradientBrush(
        &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
            center,
            gradientOriginOffset: pt(0.0, 0.0),
            radiusX: rx.max(1.0),
            radiusY: ry.max(1.0),
        },
        None,
        &col,
    )
    .ok()
}

pub unsafe fn round_stroke_style() -> Option<ID2D1StrokeStyle> {
    with_engine(|e| e.round_stroke.clone()).flatten()
}

/// Mathematically seamless, zero-ring analytical exterior drop shadow.
/// Composed of 4 non-overlapping linear side gradients and 4 non-overlapping corner radial
/// gradients whose values match identically along every edge junction and stay strictly 0.0
/// inside `rect` (zero onion-rings, zero interior darkening, zero PushLayer overhead).
unsafe fn draw_smooth_exterior_shadow(
    ctx: &ID2D1DeviceContext,
    rect: D2D_RECT_F,
    radius: f32,
    spread: f32,
    max_alpha: f32,
) {
    if max_alpha <= 0.002 || spread <= 1.0 {
        return;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    let r = radius.clamp(1.0, (w * 0.5).min(h * 0.5));
    let s = spread;

    // Smooth Gaussian curve exp(-3.5 * u^2)
    let curve: [(f32, f32); 6] = [
        (0.00, 1.00),
        (0.16, 0.76),
        (0.36, 0.46),
        (0.58, 0.21),
        (0.80, 0.06),
        (1.00, 0.00),
    ];
    let lin_stops: [D2D1_GRADIENT_STOP; 6] = curve.map(|(u, f)| D2D1_GRADIENT_STOP {
        position: u,
        color: rgba(0.0, 0.0, 0.0, max_alpha * f),
    });

    // Top & Bottom exterior strips
    if rect.right - rect.left > r * 2.0 + 0.5 {
        let top_r = d2d_rect(rect.left + r, rect.top - s, rect.right - r, rect.top);
        if let Some(g) = linear_grad(ctx, pt(0.0, rect.top), pt(0.0, rect.top - s), &lin_stops) {
            ctx.FillRectangle(&top_r, &g);
        }
        let bot_r = d2d_rect(rect.left + r, rect.bottom, rect.right - r, rect.bottom + s);
        if let Some(g) = linear_grad(
            ctx,
            pt(0.0, rect.bottom),
            pt(0.0, rect.bottom + s),
            &lin_stops,
        ) {
            ctx.FillRectangle(&bot_r, &g);
        }
    }

    // Left & Right exterior strips
    if rect.bottom - rect.top > r * 2.0 + 0.5 {
        let left_r = d2d_rect(rect.left - s, rect.top + r, rect.left, rect.bottom - r);
        if let Some(g) = linear_grad(ctx, pt(rect.left, 0.0), pt(rect.left - s, 0.0), &lin_stops) {
            ctx.FillRectangle(&left_r, &g);
        }
        let right_r = d2d_rect(rect.right, rect.top + r, rect.right + s, rect.bottom - r);
        if let Some(g) = linear_grad(
            ctx,
            pt(rect.right, 0.0),
            pt(rect.right + s, 0.0),
            &lin_stops,
        ) {
            ctx.FillRectangle(&right_r, &g);
        }
    }

    // Four corner quadrants with radial gradient strictly 0.0 inside radius `r`
    let r_tot = r + s;
    let t0 = (r / r_tot).clamp(0.0, 0.99);
    let t_zero = ((r - 0.15) / r_tot).clamp(0.0, t0);
    let rad_stops = [
        D2D1_GRADIENT_STOP {
            position: 0.0,
            color: rgba(0.0, 0.0, 0.0, 0.0),
        },
        D2D1_GRADIENT_STOP {
            position: t_zero,
            color: rgba(0.0, 0.0, 0.0, 0.0),
        },
        D2D1_GRADIENT_STOP {
            position: t0,
            color: rgba(0.0, 0.0, 0.0, max_alpha),
        },
        D2D1_GRADIENT_STOP {
            position: t0 + (1.0 - t0) * 0.16,
            color: rgba(0.0, 0.0, 0.0, max_alpha * 0.76),
        },
        D2D1_GRADIENT_STOP {
            position: t0 + (1.0 - t0) * 0.36,
            color: rgba(0.0, 0.0, 0.0, max_alpha * 0.46),
        },
        D2D1_GRADIENT_STOP {
            position: t0 + (1.0 - t0) * 0.58,
            color: rgba(0.0, 0.0, 0.0, max_alpha * 0.21),
        },
        D2D1_GRADIENT_STOP {
            position: t0 + (1.0 - t0) * 0.80,
            color: rgba(0.0, 0.0, 0.0, max_alpha * 0.06),
        },
        D2D1_GRADIENT_STOP {
            position: 1.0,
            color: rgba(0.0, 0.0, 0.0, 0.0),
        },
    ];
    let corners = [
        (
            pt(rect.left + r, rect.top + r),
            d2d_rect(rect.left - s, rect.top - s, rect.left + r, rect.top + r),
        ),
        (
            pt(rect.right - r, rect.top + r),
            d2d_rect(rect.right - r, rect.top - s, rect.right + s, rect.top + r),
        ),
        (
            pt(rect.left + r, rect.bottom - r),
            d2d_rect(
                rect.left - s,
                rect.bottom - r,
                rect.left + r,
                rect.bottom + s,
            ),
        ),
        (
            pt(rect.right - r, rect.bottom - r),
            d2d_rect(
                rect.right - r,
                rect.bottom - r,
                rect.right + s,
                rect.bottom + s,
            ),
        ),
    ];
    for (center, cbox) in corners {
        if let Some(g) = radial_grad(ctx, center, r_tot, r_tot, &rad_stops) {
            ctx.FillRectangle(&cbox, &g);
        }
    }
}

/// Renders a single, seamless, background-color-reactive Apple Liquid Glass surface:
/// 1. Silky analytical exterior drop shadow (ZERO multi-layer rings!)
/// 2. Sheer background-hue-harmonized crystal glaze adapting to light vs. dark backgrounds
/// 3. Subtle top caustic sheen + optional pointer-tracking dynamic specular reflection
/// 4. ONE crisp unified 1.15px Fresnel specular border (zero stacked inner/outer rings!)
pub unsafe fn draw_liquid_glass_pill(
    ctx: &ID2D1DeviceContext,
    rect: D2D_RECT_F,
    radius: f32,
    style: GlassStyle,
    tone: AmbientTone,
    is_panel: bool,
    shimmer_pt: Option<(f32, f32, f32)>,
    with_shadow: bool,
    opacity: f32,
) {
    if opacity <= 0.005 {
        return;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 2.0 || h <= 2.0 {
        return;
    }
    let r = radius.min(w * 0.5).min(h * 0.5);
    let is_light = tone.is_light(style);
    let (lr, lg, lb) = tone.light_tint();
    let (dr, dg, db) = tone.dark_tint();

    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: r,
        radiusY: r,
    };

    // 1. Single continuous Gaussian exterior drop shadow (no rings, no interior bleed)
    if with_shadow {
        let spread = if is_panel { 14.0 } else { 11.5 };
        let sh_alpha = if is_light { 0.14 } else { 0.20 } * opacity;
        draw_smooth_exterior_shadow(ctx, rect, r, spread, sh_alpha);
    }

    // 2. Sheer adaptive background-tinted crystal glaze (lets live GPU chromatic backdrop dominate!)
    let (top_tint, mid_tint, bot_tint) = match (style, is_panel, is_light) {
        // Dock Clear: ultra-sheer background-infused crystal (nearly pure optical refraction!)
        (GlassStyle::Clear, false, true) => (
            rgba(lr, lg, lb, 0.18 * opacity),
            rgba(lr, lg, lb, 0.10 * opacity),
            rgba(lr, lg, lb, 0.16 * opacity),
        ),
        (GlassStyle::Clear, false, false) => (
            rgba(lr, lg, lb, 0.11 * opacity),
            rgba(dr, dg, db, 0.07 * opacity),
            rgba(lr, lg, lb, 0.09 * opacity),
        ),
        // Panel Clear: adapts cleanly between luminous light crystal & rich dark crystal
        (GlassStyle::Clear, true, true) => (
            rgba(lr, lg, lb, 0.34 * opacity),
            rgba(lr, lg, lb, 0.26 * opacity),
            rgba(lr, lg, lb, 0.30 * opacity),
        ),
        (GlassStyle::Clear, true, false) => (
            rgba(dr, dg, db, 0.22 * opacity),
            rgba(dr, dg, db, 0.26 * opacity),
            rgba(dr, dg, db, 0.24 * opacity),
        ),
        // Standard (`.regular`): balanced Liquid Glass tinted by live background hue
        (GlassStyle::Standard, false, true) => (
            rgba(lr, lg, lb, 0.30 * opacity),
            rgba(lr, lg, lb, 0.22 * opacity),
            rgba(lr, lg, lb, 0.28 * opacity),
        ),
        (GlassStyle::Standard, false, false) => (
            rgba(dr, dg, db, 0.24 * opacity),
            rgba(dr, dg, db, 0.28 * opacity),
            rgba(dr, dg, db, 0.26 * opacity),
        ),
        (GlassStyle::Standard, true, true) => (
            rgba(lr, lg, lb, 0.48 * opacity),
            rgba(lr, lg, lb, 0.42 * opacity),
            rgba(lr, lg, lb, 0.46 * opacity),
        ),
        (GlassStyle::Standard, true, false) => (
            rgba(dr, dg, db, 0.38 * opacity),
            rgba(dr, dg, db, 0.44 * opacity),
            rgba(dr, dg, db, 0.42 * opacity),
        ),
        // Dark (`.tinted`): rich smoked glass subtly warmed/cooled by background color
        (GlassStyle::Dark, _, _) => (
            rgba(dr, dg, db, 0.56 * opacity),
            rgba(dr * 0.72, dg * 0.72, db * 0.72, 0.64 * opacity),
            rgba(dr * 0.82, dg * 0.82, db * 0.82, 0.62 * opacity),
        ),
    };
    if let Some(body) = linear_grad(
        ctx,
        pt(rect.left, rect.top),
        pt(rect.left, rect.bottom),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: top_tint,
            },
            D2D1_GRADIENT_STOP {
                position: 0.48,
                color: mid_tint,
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: bot_tint,
            },
        ],
    ) {
        ctx.FillRoundedRectangle(&rr, &body);
    }

    // 3. Subtle top caustic reflection (delicate, never washes out background color)
    let sheen_a = match style {
        GlassStyle::Clear => {
            if is_light {
                0.20
            } else {
                0.13
            }
        }
        GlassStyle::Standard => 0.11,
        GlassStyle::Dark => 0.06,
    } * opacity;
    if let Some(sheen) = linear_grad(
        ctx,
        pt(rect.left, rect.top),
        pt(rect.left, rect.top + (h * 0.34).min(28.0)),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(1.0, 1.0, 1.0, sheen_a),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(1.0, 1.0, 1.0, 0.0),
            },
        ],
    ) {
        ctx.FillRoundedRectangle(&rr, &sheen);
    }

    // 4. Dynamic cursor-tracking specular reflection (Apple HIG motion response)
    if let Some((sx, sy, intensity)) = shimmer_pt {
        if intensity > 0.01 {
            let rx = (w * 0.24).clamp(84.0, 195.0);
            let ry = (h * 0.82).clamp(52.0, 145.0);
            if let Some(spot) = radial_grad(
                ctx,
                pt(sx, sy),
                rx,
                ry,
                &[
                    D2D1_GRADIENT_STOP {
                        position: 0.0,
                        color: rgba(1.0, 1.0, 1.0, 0.18 * intensity * opacity),
                    },
                    D2D1_GRADIENT_STOP {
                        position: 0.50,
                        color: rgba(lr, lg, lb, 0.05 * intensity * opacity),
                    },
                    D2D1_GRADIENT_STOP {
                        position: 1.0,
                        color: rgba(1.0, 1.0, 1.0, 0.0),
                    },
                ],
            ) {
                ctx.FillRoundedRectangle(&rr, &spot);
            }
        }
    }

    // 5. ONE single unified Fresnel specular rim (no extra inner/outer stacked lines!)
    let border_rr = D2D1_ROUNDED_RECT {
        rect: d2d_rect(
            rect.left + 0.55,
            rect.top + 0.55,
            rect.right - 0.55,
            rect.bottom - 0.55,
        ),
        radiusX: (r - 0.55).max(0.0),
        radiusY: (r - 0.55).max(0.0),
    };
    let rim_boost = match style {
        GlassStyle::Clear => 1.0,
        GlassStyle::Standard => 0.86,
        GlassStyle::Dark => 0.64,
    } * opacity;
    let rim_stops = if is_light {
        [
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(1.0, 1.0, 1.0, 0.88 * rim_boost),
            },
            D2D1_GRADIENT_STOP {
                position: 0.46,
                color: rgba(0.18, 0.22, 0.28, 0.20 * rim_boost),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(1.0, 1.0, 1.0, 0.62 * rim_boost),
            },
        ]
    } else {
        [
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(
                    (0.75 + lr * 0.25).min(1.0),
                    (0.75 + lg * 0.25).min(1.0),
                    (0.75 + lb * 0.25).min(1.0),
                    0.76 * rim_boost,
                ),
            },
            D2D1_GRADIENT_STOP {
                position: 0.48,
                color: rgba(lr, lg, lb, 0.20 * rim_boost),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(
                    (0.72 + lr * 0.28).min(1.0),
                    (0.72 + lg * 0.28).min(1.0),
                    (0.72 + lb * 0.28).min(1.0),
                    0.50 * rim_boost,
                ),
            },
        ]
    };
    if let Some(rim) = linear_grad(
        ctx,
        pt(rect.left + w * 0.10, rect.top),
        pt(rect.left + w * 0.90, rect.bottom),
        &rim_stops,
    ) {
        ctx.DrawRoundedRectangle(&border_rr, &rim, 1.1, None);
    }

    // 6. Dynamic specular edge glint along the SAME single border (zero extra rings)
    if let Some((sx, sy, intensity)) = shimmer_pt {
        if intensity > 0.01 {
            if let Some(glint) = radial_grad(
                ctx,
                pt(sx, sy),
                120.0,
                (h * 0.90).max(66.0),
                &[
                    D2D1_GRADIENT_STOP {
                        position: 0.0,
                        color: rgba(1.0, 1.0, 1.0, 0.78 * intensity * opacity),
                    },
                    D2D1_GRADIENT_STOP {
                        position: 0.52,
                        color: rgba(lr, lg, lb, 0.20 * intensity * opacity),
                    },
                    D2D1_GRADIENT_STOP {
                        position: 1.0,
                        color: rgba(1.0, 1.0, 1.0, 0.0),
                    },
                ],
            ) {
                ctx.DrawRoundedRectangle(&border_rr, &glint, 1.2, None);
            }
        }
    }
}

/// Draws the top Menu Bar Liquid Glass overlay over the live Dual-Scale Chromatic backdrop.
/// Always frosted Liquid Glass across Clear / Standard / Dark (never naked see-through wallpaper),
/// adapting its tint and bottom specular hairline to the live background luminance & color!
pub unsafe fn draw_menu_bar_glass(
    ctx: &ID2D1DeviceContext,
    w: f32,
    h: f32,
    style: GlassStyle,
    tone: AmbientTone,
) {
    let is_light = tone.is_light(style);
    let (lr, lg, lb) = tone.light_tint();
    let (dr, dg, db) = tone.dark_tint();

    let (top, bot) = match (style, is_light) {
        (GlassStyle::Clear, true) => (rgba(lr, lg, lb, 0.34), rgba(lr, lg, lb, 0.26)),
        (GlassStyle::Clear, false) => (rgba(dr, dg, db, 0.20), rgba(dr, dg, db, 0.24)),
        (GlassStyle::Standard, true) => (rgba(lr, lg, lb, 0.48), rgba(lr, lg, lb, 0.42)),
        (GlassStyle::Standard, false) => (rgba(dr, dg, db, 0.34), rgba(dr, dg, db, 0.38)),
        (GlassStyle::Dark, _) => (
            rgba(dr, dg, db, 0.62),
            rgba(dr * 0.75, dg * 0.75, db * 0.75, 0.68),
        ),
    };
    if let Some(bg) = linear_grad(
        ctx,
        pt(0.0, 0.0),
        pt(0.0, h),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: top,
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: bot,
            },
        ],
    ) {
        ctx.FillRectangle(&d2d_rect(0.0, 0.0, w, h), &bg);
    }

    // Gentle top sheen
    let sheen_a = match (style, is_light) {
        (GlassStyle::Clear, true) => 0.22,
        (GlassStyle::Clear, false) => 0.11,
        (GlassStyle::Standard, _) => 0.12,
        (GlassStyle::Dark, _) => 0.06,
    };
    if let Some(sheen) = linear_grad(
        ctx,
        pt(0.0, 0.0),
        pt(0.0, h * 0.46),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(1.0, 1.0, 1.0, sheen_a),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(1.0, 1.0, 1.0, 0.0),
            },
        ],
    ) {
        ctx.FillRectangle(&d2d_rect(0.0, 0.0, w, h * 0.46), &sheen);
    }

    // Subtle adaptive bottom edge definition
    let line_col = if is_light {
        rgba(0.0, 0.0, 0.0, 0.12)
    } else {
        rgba(lr, lg, lb, 0.24)
    };
    if let Some(b) = solid(ctx, line_col) {
        ctx.FillRectangle(&d2d_rect(0.0, h - 1.0, w, h), &b);
    }
}

// ---------- DirectWrite Text Helpers ----------

#[derive(Clone, Copy)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy)]
pub enum TextWeight {
    Regular,
    Medium,
    SemiBold,
    Bold,
}

pub unsafe fn create_text_format(
    size: f32,
    weight: TextWeight,
    align: TextAlign,
) -> Option<IDWriteTextFormat> {
    let clamped = size.max(8.0);
    let key = (
        (clamped * 4.0).round() as u32,
        match weight {
            TextWeight::Regular => 0u8,
            TextWeight::Medium => 1u8,
            TextWeight::SemiBold => 2u8,
            TextWeight::Bold => 3u8,
        },
        match align {
            TextAlign::Left => 0u8,
            TextAlign::Center => 1u8,
            TextAlign::Right => 2u8,
        },
    );
    with_engine(|e| {
        if let Some(cached) = e.text_formats.borrow().get(&key) {
            return Some(cached.clone());
        }
        let w = match weight {
            TextWeight::Regular => DWRITE_FONT_WEIGHT_NORMAL,
            TextWeight::Medium => DWRITE_FONT_WEIGHT_MEDIUM,
            TextWeight::SemiBold => DWRITE_FONT_WEIGHT_SEMI_BOLD,
            TextWeight::Bold => DWRITE_FONT_WEIGHT_BOLD,
        };
        let fmt = e
            .dwrite
            .CreateTextFormat(
                &e.ui_font,
                None,
                DWRITE_FONT_WEIGHT(w.0),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                key.0 as f32 * 0.25,
                &e.ui_locale,
            )
            .ok()?;
        let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        let _ = fmt.SetTextAlignment(match align {
            TextAlign::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
            TextAlign::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            TextAlign::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
        });
        if let Ok(ellipsis) = e.dwrite.CreateEllipsisTrimmingSign(&fmt) {
            let trim = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            };
            let _ = fmt.SetTrimming(&trim, &ellipsis);
        }
        let mut cache = e.text_formats.borrow_mut();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, fmt.clone());
        Some(fmt)
    })
    .flatten()
}

pub unsafe fn measure_text_width(text: &str, size: f32, weight: TextWeight) -> f32 {
    with_engine(|e| {
        let Some(fmt) = create_text_format(size, weight, TextAlign::Left) else {
            return text.chars().count() as f32 * size * 0.6;
        };
        let wide: Vec<u16> = text.encode_utf16().collect();
        let Ok(layout) = e.dwrite.CreateTextLayout(&wide, &fmt, 2000.0, 200.0) else {
            return text.chars().count() as f32 * size * 0.6;
        };
        let mut m = zeroed();
        if layout.GetMetrics(&mut m).is_ok() {
            m.widthIncludingTrailingWhitespace.ceil()
        } else {
            text.chars().count() as f32 * size * 0.6
        }
    })
    .unwrap_or(60.0)
}

pub unsafe fn draw_text(
    ctx: &ID2D1DeviceContext,
    text: &str,
    rect: D2D_RECT_F,
    size: f32,
    weight: TextWeight,
    color: D2D1_COLOR_F,
    align: TextAlign,
    shadow: bool,
) {
    let Some(fmt) = create_text_format(size, weight, align) else {
        return;
    };
    let wide: Vec<u16> = text.encode_utf16().collect();
    if shadow {
        for (dy, a) in [(1.5f32, 0.28f32), (0.75f32, 0.50f32)] {
            if let Some(sb) = solid(ctx, rgba(0.0, 0.0, 0.0, a * color.a)) {
                let sr = d2d_rect(rect.left, rect.top + dy, rect.right, rect.bottom + dy);
                ctx.DrawText(
                    &wide,
                    &fmt,
                    &sr,
                    &sb,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }
    if let Some(b) = solid(ctx, color) {
        ctx.DrawText(
            &wide,
            &fmt,
            &rect,
            &b,
            D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}

// ---------- Bitmap / Icon Conversion to GPU Premultiplied BGRA ----------

/// Automatic macOS-style squircle tile metadata for icons with large transparent backgrounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IconBadge {
    /// `true` -> deep obsidian/graphite slate for bright/white icons;
    /// `false` -> warm porcelain/pearl off-white for dark/mid-tone/colorful icons.
    pub dark: bool,
    /// Subtle harmonic RGB tint (0..1) sampled from the icon's visible pixels.
    pub tint: (f32, f32, f32),
    /// Tight opaque source bounding box in pixels `(left, top, right, bottom)` for optical centering.
    pub src_bbox: (f32, f32, f32, f32),
}

/// Analyzes premultiplied BGRA pixels to decide whether a transparent icon needs a squircle tile,
/// and picks a high-contrast, tasteful off-white or deep graphite backdrop color.
pub fn analyze_icon_badge(w: u32, h: u32, bgra: &[u8]) -> Option<IconBadge> {
    let total = (w as usize).saturating_mul(h as usize);
    if total == 0 || bgra.len() < total * 4 {
        return None;
    }

    let mut transparent_px = 0usize;
    let mut visible_weight = 0.0f64;
    let mut sum_lum = 0.0f64;
    let mut high_lum_weight = 0.0f64;
    let mut sum_r = 0.0f64;
    let mut sum_g = 0.0f64;
    let mut sum_b = 0.0f64;
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0u32;
    let mut max_y = 0u32;

    for y in 0..h {
        let row_off = (y as usize) * (w as usize) * 4;
        for x in 0..w {
            let off = row_off + (x as usize) * 4;
            let b_pm = bgra[off] as f64;
            let g_pm = bgra[off + 1] as f64;
            let r_pm = bgra[off + 2] as f64;
            let a_u8 = bgra[off + 3];

            if a_u8 < 36 {
                transparent_px += 1;
                continue;
            }
            if x < min_x {
                min_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if x > max_x {
                max_x = x;
            }
            if y > max_y {
                max_y = y;
            }

            let a = a_u8 as f64 / 255.0;
            // Un-premultiply safely back to straight RGB in 0..1
            let r = (r_pm / (a_u8 as f64)).clamp(0.0, 1.0);
            let g = (g_pm / (a_u8 as f64)).clamp(0.0, 1.0);
            let b = (b_pm / (a_u8 as f64)).clamp(0.0, 1.0);
            let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;

            visible_weight += a;
            sum_lum += lum * a;
            if lum >= 0.72 {
                high_lum_weight += a;
            }
            sum_r += r * a;
            sum_g += g * a;
            sum_b += b * a;
        }
    }

    if visible_weight <= 1.0 || min_x > max_x || min_y > max_y {
        return None;
    }

    let transparent_ratio = transparent_px as f32 / total as f32;
    // Filled squares / filled macOS squircles (< 38% transparent) keep their own background untouched.
    if transparent_ratio < 0.38 {
        return None;
    }

    let avg_lum = (sum_lum / visible_weight) as f32;
    let high_lum_ratio = (high_lum_weight / visible_weight) as f32;
    let tint = (
        (sum_r / visible_weight) as f32,
        (sum_g / visible_weight) as f32,
        (sum_b / visible_weight) as f32,
    );

    // Bright / white / pastel transparent icons -> tasteful deep graphite slate;
    // Dark / rich / colorful transparent icons -> tasteful warm porcelain off-white.
    let dark = avg_lum >= 0.56 || (avg_lum >= 0.46 && high_lum_ratio >= 0.42);

    // Pad tight bounding box by 1px (clamped) so anti-aliased edges stay razor-clean
    let sl = min_x.saturating_sub(1) as f32;
    let st = min_y.saturating_sub(1) as f32;
    let sr = (max_x + 2).min(w) as f32;
    let sb = (max_y + 2).min(h) as f32;

    Some(IconBadge {
        dark,
        tint,
        src_bbox: (sl, st, sr, sb),
    })
}

pub struct RawIcon {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub badge: Option<IconBadge>,
}

impl RawIcon {
    pub fn from_premul_bgra(width: u32, height: u32, bgra: Vec<u8>) -> Option<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if width == 0 || height == 0 || width > 512 || height > 512 || bgra.len() != expected {
            return None;
        }
        let badge = analyze_icon_badge(width, height, &bgra);
        Some(Self {
            width,
            height,
            bgra,
            badge,
        })
    }

    pub unsafe fn from_hbitmap(hbmp: windows_sys::Win32::Graphics::Gdi::HBITMAP) -> Option<Self> {
        if hbmp.is_null() {
            return None;
        }
        let mut bm: BITMAP = zeroed();
        if GetObjectW(
            WHBITMAP(hbmp),
            size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        ) == 0
        {
            return None;
        }
        let w = bm.bmWidth.max(0) as u32;
        let h = bm.bmHeight.unsigned_abs();
        if w == 0 || h == 0 || w > 512 || h > 512 {
            return None;
        }
        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w as i32;
        bi.bmiHeader.biHeight = -(h as i32);
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let lines = GetDIBits(
            dc,
            WHBITMAP(hbmp),
            0,
            h,
            Some(buf.as_mut_ptr().cast()),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        if lines == 0 {
            return None;
        }

        let mut any_alpha = false;
        let mut needs_premul = false;
        for px in buf.chunks_exact(4) {
            let a = px[3];
            if a != 0 {
                any_alpha = true;
                if px[0] > a || px[1] > a || px[2] > a {
                    needs_premul = true;
                }
            }
        }
        if !any_alpha {
            for px in buf.chunks_exact_mut(4) {
                if px[0] != 0 || px[1] != 0 || px[2] != 0 {
                    px[3] = 255;
                }
            }
        } else if needs_premul {
            for px in buf.chunks_exact_mut(4) {
                let a = px[3] as u16;
                px[0] = ((px[0] as u16 * a + 127) / 255) as u8;
                px[1] = ((px[1] as u16 * a + 127) / 255) as u8;
                px[2] = ((px[2] as u16 * a + 127) / 255) as u8;
            }
        }

        let badge = analyze_icon_badge(w, h, &buf);
        Some(Self {
            width: w,
            height: h,
            bgra: buf,
            badge,
        })
    }

    pub unsafe fn from_hicon(
        hicon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    ) -> Option<Self> {
        if hicon.is_null() {
            return None;
        }
        let mut info = zeroed();
        if GetIconInfo(WHICON(hicon), &mut info).is_err() {
            return None;
        }
        let res = if !info.hbmColor.is_invalid() {
            Self::from_hbitmap(info.hbmColor.0)
        } else {
            None
        };
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor);
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask);
        }
        res
    }

    pub unsafe fn to_d2d(&self, ctx: &ID2D1DeviceContext) -> Option<ID2D1Bitmap> {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        ID2D1RenderTarget::CreateBitmap(
            ctx,
            D2D_SIZE_U {
                width: self.width,
                height: self.height,
            },
            Some(self.bgra.as_ptr().cast()),
            self.width * 4,
            &props,
        )
        .ok()
    }
}

pub unsafe fn draw_d2d_bitmap(
    ctx: &ID2D1DeviceContext,
    bmp: &ID2D1Bitmap,
    dest: D2D_RECT_F,
    opacity: f32,
) {
    ctx.DrawBitmap(
        bmp,
        Some(&dest),
        opacity,
        D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
        None,
        None,
    );
}

/// Draws an app icon bitmap, automatically backing largely transparent icons with a refined
/// macOS-style squircle tile (soft pearl off-white or deep obsidian slate, harmonized with icon hue).
pub unsafe fn draw_icon_with_badge(
    ctx: &ID2D1DeviceContext,
    bmp: &ID2D1Bitmap,
    badge: Option<&IconBadge>,
    dest: D2D_RECT_F,
    opacity: f32,
) {
    let Some(b) = badge else {
        draw_d2d_bitmap(ctx, bmp, dest, opacity);
        return;
    };

    let w = (dest.right - dest.left).max(1.0);
    let h = (dest.bottom - dest.top).max(1.0);
    let r = w.min(h) * 0.225;
    let rr = D2D1_ROUNDED_RECT {
        rect: dest,
        radiusX: r,
        radiusY: r,
    };

    let (tr, tg, tb) = b.tint;
    let (top_c, bot_c, sheen_a, rim_top, rim_bot) = if b.dark {
        // Refined Apple dark-mode obsidian/graphite slate (never harsh pure #000000)
        (
            rgba(
                (0.168 + tr * 0.045).clamp(0.0, 1.0),
                (0.176 + tg * 0.045).clamp(0.0, 1.0),
                (0.204 + tb * 0.052).clamp(0.0, 1.0),
                0.96 * opacity,
            ),
            rgba(
                (0.098 + tr * 0.032).clamp(0.0, 1.0),
                (0.104 + tg * 0.032).clamp(0.0, 1.0),
                (0.126 + tb * 0.038).clamp(0.0, 1.0),
                0.98 * opacity,
            ),
            0.13 * opacity,
            rgba(1.0, 1.0, 1.0, 0.24 * opacity),
            rgba(1.0, 1.0, 1.0, 0.06 * opacity),
        )
    } else {
        // Refined Apple ceramic pearl / porcelain off-white (never blinding pure #FFFFFF)
        (
            rgba(
                (0.946 + tr * 0.028).clamp(0.0, 0.985),
                (0.952 + tg * 0.026).clamp(0.0, 0.988),
                (0.966 + tb * 0.022).clamp(0.0, 0.992),
                0.97 * opacity,
            ),
            rgba(
                (0.862 + tr * 0.036).clamp(0.0, 0.94),
                (0.874 + tg * 0.034).clamp(0.0, 0.945),
                (0.900 + tb * 0.032).clamp(0.0, 0.96),
                0.98 * opacity,
            ),
            0.36 * opacity,
            rgba(1.0, 1.0, 1.0, 0.78 * opacity),
            rgba(0.10, 0.12, 0.18, 0.16 * opacity),
        )
    };

    if let Some(bg) = linear_grad(
        ctx,
        pt(dest.left, dest.top),
        pt(dest.left, dest.bottom),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: top_c,
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: bot_c,
            },
        ],
    ) {
        ctx.FillRoundedRectangle(&rr, &bg);
    }

    // Subtle top ceramic/glass specular highlight
    if let Some(sheen) = linear_grad(
        ctx,
        pt(dest.left, dest.top),
        pt(dest.left, dest.top + h * 0.42),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(1.0, 1.0, 1.0, sheen_a),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(1.0, 1.0, 1.0, 0.0),
            },
        ],
    ) {
        ctx.FillRoundedRectangle(&rr, &sheen);
    }

    // Crisp hairline edge definition
    let rim_rr = D2D1_ROUNDED_RECT {
        rect: d2d_rect(
            dest.left + 0.45,
            dest.top + 0.45,
            dest.right - 0.45,
            dest.bottom - 0.45,
        ),
        radiusX: (r - 0.45).max(1.0),
        radiusY: (r - 0.45).max(1.0),
    };
    if let Some(rim) = linear_grad(
        ctx,
        pt(dest.left, dest.top),
        pt(dest.left, dest.bottom),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rim_top,
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rim_bot,
            },
        ],
    ) {
        ctx.DrawRoundedRectangle(&rim_rr, &rim, 0.9, None);
    }

    // Center the icon's tight opaque bounding box cleanly (aspect-preserving) inside 72% inner frame
    let (sl, st, sr, sb) = b.src_bbox;
    let sw = (sr - sl).max(1.0);
    let sh = (sb - st).max(1.0);
    let max_inner = w.min(h) * 0.72;
    let scale = (max_inner / sw).min(max_inner / sh);
    let dw = sw * scale;
    let dh = sh * scale;
    let cx = (dest.left + dest.right) * 0.5;
    let cy = (dest.top + dest.bottom) * 0.5;
    let icon_dest = d2d_rect(cx - dw * 0.5, cy - dh * 0.5, cx + dw * 0.5, cy + dh * 0.5);
    let src_rect = d2d_rect(sl, st, sr, sb);

    ctx.DrawBitmap(
        bmp,
        Some(&icon_dest),
        opacity,
        D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
        Some(&src_rect),
        None,
    );
}

// ---------- Crisp Vector macOS System Icons (Direct2D Paths) ----------

pub unsafe fn draw_mac_emblem(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    color: D2D1_COLOR_F,
) {
    let Some(brush) = solid(ctx, color) else {
        return;
    };
    let Some(path) = with_engine(|e| e.d2d_factory.CreatePathGeometry().ok()).flatten() else {
        return;
    };
    if let Ok(sink) = path.Open() {
        let x = |v: f32| cx + v * s;
        let y = |v: f32| cy + v * s;
        sink.BeginFigure(pt(x(0.0), y(-0.22)), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(0.18), y(-0.34)),
            point2: pt(x(0.42), y(-0.28)),
            point3: pt(x(0.43), y(-0.08)),
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(0.28), y(-0.04)),
            point2: pt(x(0.28), y(0.18)),
            point3: pt(x(0.44), y(0.22)),
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(0.38), y(0.42)),
            point2: pt(x(0.22), y(0.54)),
            point3: pt(x(0.11), y(0.50)),
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(0.04), y(0.47)),
            point2: pt(x(-0.04), y(0.47)),
            point3: pt(x(-0.11), y(0.50)),
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(-0.24), y(0.54)),
            point2: pt(x(-0.46), y(0.32)),
            point3: pt(x(-0.46), y(0.02)),
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(-0.46), y(-0.26)),
            point2: pt(x(-0.22), y(-0.35)),
            point3: pt(x(0.0), y(-0.22)),
        });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);

        sink.BeginFigure(pt(x(-0.01), y(-0.28)), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(-0.02), y(-0.46)),
            point2: pt(x(0.12), y(-0.56)),
            point3: pt(x(0.22), y(-0.54)),
        });
        sink.AddBezier(&D2D1_BEZIER_SEGMENT {
            point1: pt(x(0.22), y(-0.38)),
            point2: pt(x(0.10), y(-0.28)),
            point3: pt(x(-0.01), y(-0.28)),
        });
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);

        let _ = sink.Close();
        ctx.FillGeometry(&path, &brush, None);
    }
}

pub unsafe fn draw_wifi_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    strength: u8,
    color: D2D1_COLOR_F,
) {
    let style = round_stroke_style();
    if strength == 4 {
        // Crisp macOS-style Wired Ethernet / LAN topology glyph (immediately distinct from Wi-Fi arcs!)
        let Some(b) = solid(ctx, color) else { return };
        let sw = (s * 0.095).max(1.25);
        // Top network node
        let top_w = s * 0.22;
        let top_h = s * 0.16;
        let top_rr = D2D1_ROUNDED_RECT {
            rect: d2d_rect(
                cx - top_w,
                cy - s * 0.40,
                cx + top_w,
                cy - s * 0.40 + top_h * 2.0,
            ),
            radiusX: s * 0.07,
            radiusY: s * 0.07,
        };
        ctx.DrawRoundedRectangle(&top_rr, &b, sw, None);
        // Vertical stem down to horizontal bus
        let bus_y = cy + s * 0.04;
        ctx.DrawLine(
            pt(cx, cy - s * 0.40 + top_h * 2.0),
            pt(cx, bus_y),
            &b,
            sw,
            style.as_ref(),
        );
        // Horizontal LAN bus
        let bus_half = s * 0.30;
        ctx.DrawLine(
            pt(cx - bus_half, bus_y),
            pt(cx + bus_half, bus_y),
            &b,
            sw,
            style.as_ref(),
        );
        // Left & right downward legs + bottom endpoint plugs
        let leg_bot = cy + s * 0.18;
        let node_w = s * 0.13;
        let node_h = s * 0.20;
        for sx in [-bus_half, bus_half] {
            ctx.DrawLine(
                pt(cx + sx, bus_y),
                pt(cx + sx, leg_bot),
                &b,
                sw,
                style.as_ref(),
            );
            let nr = D2D1_ROUNDED_RECT {
                rect: d2d_rect(
                    cx + sx - node_w,
                    leg_bot,
                    cx + sx + node_w,
                    leg_bot + node_h,
                ),
                radiusX: s * 0.05,
                radiusY: s * 0.05,
            };
            ctx.FillRoundedRectangle(&nr, &b);
        }
        return;
    }

    let base_y = cy + s * 0.28;
    let active_b = solid(ctx, color);
    let dim_b = solid(ctx, rgba(color.r, color.g, color.b, color.a * 0.28));

    let dot_b = if strength >= 1 { &active_b } else { &dim_b };
    if let Some(b) = dot_b {
        ctx.FillEllipse(
            &D2D1_ELLIPSE {
                point: pt(cx, base_y),
                radiusX: s * 0.08,
                radiusY: s * 0.08,
            },
            b,
        );
    }

    let radii = [0.24f32, 0.42, 0.60];
    let stroke_w = (s * 0.105).max(1.3);
    for (idx, r_frac) in radii.iter().enumerate() {
        let lit = (idx as u8) < strength;
        let Some(b) = (if lit { &active_b } else { &dim_b }) else {
            continue;
        };
        let r = s * r_frac;
        let angle = 0.72f32;
        let sx = cx - r * angle.sin();
        let sy = base_y - r * angle.cos();
        let ex = cx + r * angle.sin();
        let ey = sy;
        if let Some(path) = with_engine(|e| e.d2d_factory.CreatePathGeometry().ok()).flatten() {
            if let Ok(sink) = path.Open() {
                sink.BeginFigure(pt(sx, sy), D2D1_FIGURE_BEGIN_HOLLOW);
                sink.AddArc(&D2D1_ARC_SEGMENT {
                    point: pt(ex, ey),
                    size: D2D_SIZE_F {
                        width: r,
                        height: r,
                    },
                    rotationAngle: 0.0,
                    sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                    arcSize: D2D1_ARC_SIZE_SMALL,
                });
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                let _ = sink.Close();
                ctx.DrawGeometry(&path, b, stroke_w, style.as_ref());
            }
        }
    }

    if strength == 0 {
        if let Some(b) = &active_b {
            ctx.DrawLine(
                pt(cx - s * 0.36, cy - s * 0.34),
                pt(cx + s * 0.36, cy + s * 0.36),
                b,
                (s * 0.10).max(1.3),
                style.as_ref(),
            );
        }
    }
}

pub unsafe fn draw_volume_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    level: u8,
    muted: bool,
    color: D2D1_COLOR_F,
) {
    let Some(b) = solid(ctx, color) else { return };
    let dim_b = solid(ctx, rgba(color.r, color.g, color.b, color.a * 0.28));
    let style = round_stroke_style();

    if let Some(path) = with_engine(|e| e.d2d_factory.CreatePathGeometry().ok()).flatten() {
        if let Ok(sink) = path.Open() {
            let ox = cx - s * 0.16;
            sink.BeginFigure(pt(ox - s * 0.28, cy - s * 0.14), D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(pt(ox - s * 0.12, cy - s * 0.14));
            sink.AddLine(pt(ox + s * 0.08, cy - s * 0.32));
            sink.AddLine(pt(ox + s * 0.08, cy + s * 0.32));
            sink.AddLine(pt(ox - s * 0.12, cy + s * 0.14));
            sink.AddLine(pt(ox - s * 0.28, cy + s * 0.14));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
            ctx.FillGeometry(&path, &b, None);
        }
    }

    if muted || level == 0 {
        let ox = cx + s * 0.16;
        let d = s * 0.15;
        let sw = (s * 0.10).max(1.3);
        ctx.DrawLine(
            pt(ox - d, cy - d),
            pt(ox + d, cy + d),
            &b,
            sw,
            style.as_ref(),
        );
        ctx.DrawLine(
            pt(ox + d, cy - d),
            pt(ox - d, cy + d),
            &b,
            sw,
            style.as_ref(),
        );
    } else {
        let waves = if level > 66 {
            3
        } else if level > 30 {
            2
        } else {
            1
        };
        let origin_x = cx - s * 0.04;
        let radii = [0.18f32, 0.31, 0.44];
        let sw = (s * 0.095).max(1.2);
        for (i, rf) in radii.iter().enumerate() {
            let brush = if i < waves {
                &b
            } else if let Some(db) = &dim_b {
                db
            } else {
                continue;
            };
            let r = s * rf;
            let ang = 0.65f32;
            let sx = origin_x + r * ang.cos();
            let sy = cy - r * ang.sin();
            let ex = sx;
            let ey = cy + r * ang.sin();
            if let Some(path) = with_engine(|e| e.d2d_factory.CreatePathGeometry().ok()).flatten() {
                if let Ok(sink) = path.Open() {
                    sink.BeginFigure(pt(sx, sy), D2D1_FIGURE_BEGIN_HOLLOW);
                    sink.AddArc(&D2D1_ARC_SEGMENT {
                        point: pt(ex, ey),
                        size: D2D_SIZE_F {
                            width: r,
                            height: r,
                        },
                        rotationAngle: 0.0,
                        sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                        arcSize: D2D1_ARC_SIZE_SMALL,
                    });
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    let _ = sink.Close();
                    ctx.DrawGeometry(&path, brush, sw, style.as_ref());
                }
            }
        }
    }
}

pub unsafe fn draw_battery_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    percent: u8,
    charging: bool,
    color: D2D1_COLOR_F,
) {
    let w = s * 1.18;
    let h = s * 0.62;
    let left = cx - w * 0.52;
    let top = cy - h * 0.5;
    let body = D2D1_ROUNDED_RECT {
        rect: d2d_rect(left, top, left + w, top + h),
        radiusX: h * 0.28,
        radiusY: h * 0.28,
    };
    if let Some(b) = solid(ctx, rgba(color.r, color.g, color.b, color.a * 0.80)) {
        ctx.DrawRoundedRectangle(&body, &b, (s * 0.08).max(1.1), None);
        let nub = D2D1_ROUNDED_RECT {
            rect: d2d_rect(left + w + 1.5, cy - h * 0.22, left + w + 3.4, cy + h * 0.22),
            radiusX: 1.0,
            radiusY: 1.0,
        };
        ctx.FillRoundedRectangle(&nub, &b);
    }
    let fill_color = if charging {
        rgba(0.32, 0.88, 0.45, 0.96)
    } else if percent <= 20 {
        rgba(1.0, 0.34, 0.30, 0.96)
    } else {
        color
    };
    let pad = 2.1;
    let max_fw = (w - pad * 2.0).max(1.0);
    let fw = (max_fw * (percent.min(100) as f32 / 100.0)).max(2.0);
    let fill_rr = D2D1_ROUNDED_RECT {
        rect: d2d_rect(left + pad, top + pad, left + pad + fw, top + h - pad),
        radiusX: (h * 0.16).max(1.0),
        radiusY: (h * 0.16).max(1.0),
    };
    if let Some(fb) = solid(ctx, fill_color) {
        ctx.FillRoundedRectangle(&fill_rr, &fb);
    }
}

pub unsafe fn draw_search_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    color: D2D1_COLOR_F,
) {
    let Some(b) = solid(ctx, color) else { return };
    let style = round_stroke_style();
    let sw = (s * 0.11).max(1.4);
    let r = s * 0.28;
    let gx = cx - s * 0.07;
    let gy = cy - s * 0.07;
    ctx.DrawEllipse(
        &D2D1_ELLIPSE {
            point: pt(gx, gy),
            radiusX: r,
            radiusY: r,
        },
        &b,
        sw,
        None,
    );
    let d = r * 0.72;
    ctx.DrawLine(
        pt(gx + d, gy + d),
        pt(cx + s * 0.35, cy + s * 0.35),
        &b,
        sw * 1.15,
        style.as_ref(),
    );
}

pub unsafe fn draw_control_center_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    color: D2D1_COLOR_F,
) {
    let Some(b) = solid(ctx, color) else { return };
    let tw = s * 0.86;
    let th = s * 0.36;
    let sw = (s * 0.075).max(1.05);

    let ty = cy - th * 0.68;
    let top_rr = D2D1_ROUNDED_RECT {
        rect: d2d_rect(cx - tw * 0.5, ty - th * 0.5, cx + tw * 0.5, ty + th * 0.5),
        radiusX: th * 0.5,
        radiusY: th * 0.5,
    };
    ctx.DrawRoundedRectangle(&top_rr, &b, sw, None);
    ctx.FillEllipse(
        &D2D1_ELLIPSE {
            point: pt(cx - tw * 0.5 + th * 0.5, ty),
            radiusX: th * 0.34,
            radiusY: th * 0.34,
        },
        &b,
    );

    let by = cy + th * 0.68;
    let bot_rr = D2D1_ROUNDED_RECT {
        rect: d2d_rect(cx - tw * 0.5, by - th * 0.5, cx + tw * 0.5, by + th * 0.5),
        radiusX: th * 0.5,
        radiusY: th * 0.5,
    };
    if let Some(dim) = solid(ctx, rgba(color.r, color.g, color.b, color.a * 0.35)) {
        ctx.FillRoundedRectangle(&bot_rr, &dim);
    }
    ctx.DrawRoundedRectangle(&bot_rr, &b, sw, None);
    ctx.FillEllipse(
        &D2D1_ELLIPSE {
            point: pt(cx + tw * 0.5 - th * 0.5, by),
            radiusX: th * 0.34,
            radiusY: th * 0.34,
        },
        &b,
    );
}

pub unsafe fn draw_chevron_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    color: D2D1_COLOR_F,
) {
    let Some(b) = solid(ctx, color) else { return };
    let style = round_stroke_style();
    let sw = (s * 0.11).max(1.3);
    let dx = s * 0.26;
    let dy = s * 0.14;
    ctx.DrawLine(
        pt(cx - dx, cy + dy),
        pt(cx, cy - dy),
        &b,
        sw,
        style.as_ref(),
    );
    ctx.DrawLine(
        pt(cx, cy - dy),
        pt(cx + dx, cy + dy),
        &b,
        sw,
        style.as_ref(),
    );
}

pub unsafe fn draw_ime_badge(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    label: &str,
    color: D2D1_COLOR_F,
) {
    let w = s * 1.15;
    let h = s * 0.96;
    let rr = D2D1_ROUNDED_RECT {
        rect: d2d_rect(cx - w * 0.5, cy - h * 0.5, cx + w * 0.5, cy + h * 0.5),
        radiusX: s * 0.24,
        radiusY: s * 0.24,
    };
    if let Some(bg) = solid(ctx, rgba(color.r, color.g, color.b, color.a * 0.18)) {
        ctx.FillRoundedRectangle(&rr, &bg);
    }
    if let Some(bd) = solid(ctx, rgba(color.r, color.g, color.b, color.a * 0.68)) {
        ctx.DrawRoundedRectangle(&rr, &bd, 1.0, None);
    }
    let dark_ink = (color.r * 0.299 + color.g * 0.587 + color.b * 0.114) < 0.45;
    draw_text(
        ctx,
        label,
        d2d_rect(cx - w * 0.5, cy - h * 0.5 - 0.5, cx + w * 0.5, cy + h * 0.5),
        (s * 0.66).max(9.0),
        TextWeight::SemiBold,
        color,
        TextAlign::Center,
        !dark_ink,
    );
}

/// Crisp back chevron (`‹`) for Liquid Glass sub-panels (e.g. Network -> Control Center).
pub unsafe fn draw_back_chevron(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    color: D2D1_COLOR_F,
) {
    let Some(b) = solid(ctx, color) else { return };
    let style = round_stroke_style();
    let sw = (s * 0.14).max(1.4);
    let dx = s * 0.18;
    let dy = s * 0.30;
    ctx.DrawLine(
        pt(cx + dx, cy - dy),
        pt(cx - dx, cy),
        &b,
        sw,
        style.as_ref(),
    );
    ctx.DrawLine(
        pt(cx - dx, cy),
        pt(cx + dx, cy + dy),
        &b,
        sw,
        style.as_ref(),
    );
}

/// Crisp miniature padlock glyph for secured Wi-Fi networks.
pub unsafe fn draw_lock_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    s: f32,
    color: D2D1_COLOR_F,
) {
    let Some(b) = solid(ctx, color) else { return };
    let sw = (s * 0.11).max(1.1);
    let bw = s * 0.58;
    let bh = s * 0.46;
    let body = D2D1_ROUNDED_RECT {
        rect: d2d_rect(
            cx - bw * 0.5,
            cy - s * 0.04,
            cx + bw * 0.5,
            cy - s * 0.04 + bh,
        ),
        radiusX: s * 0.10,
        radiusY: s * 0.10,
    };
    ctx.FillRoundedRectangle(&body, &b);
    let sh_r = s * 0.18;
    let sh_rr = D2D1_ROUNDED_RECT {
        rect: d2d_rect(cx - sh_r, cy - s * 0.40, cx + sh_r, cy + s * 0.08),
        radiusX: sh_r,
        radiusY: sh_r,
    };
    ctx.DrawRoundedRectangle(&sh_rr, &b, sw, None);
}

/// Authentic macOS Launchpad / App Launcher Squircle Icon (3x3 vibrant rounded tiles on brushed glass!)
pub unsafe fn draw_dock_launcher_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    size: f32,
    opacity: f32,
) {
    let half = size * 0.45;
    let rect = d2d_rect(cx - half, cy - half, cx + half, cy + half);
    let r = size * 0.225;
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: r,
        radiusY: r,
    };

    if let Some(bg) = linear_grad(
        ctx,
        pt(cx, rect.top),
        pt(cx, rect.bottom),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(0.95, 0.97, 1.0, 0.96 * opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 0.55,
                color: rgba(0.82, 0.86, 0.93, 0.95 * opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(0.68, 0.74, 0.84, 0.96 * opacity),
            },
        ],
    ) {
        ctx.FillRoundedRectangle(&rr, &bg);
    }
    if let Some(rim) = linear_grad(
        ctx,
        pt(rect.left, rect.top),
        pt(rect.right, rect.bottom),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(1.0, 1.0, 1.0, 0.88 * opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 0.5,
                color: rgba(1.0, 1.0, 1.0, 0.26 * opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(1.0, 1.0, 1.0, 0.58 * opacity),
            },
        ],
    ) {
        ctx.DrawRoundedRectangle(&rr, &rim, 1.15, None);
    }

    // 3x3 colorful macOS Launchpad rounded tiles
    let tiles: [(f32, f32, f32); 9] = [
        (0.98, 0.34, 0.36), // Coral Red
        (1.00, 0.62, 0.18), // Tangerine
        (0.99, 0.82, 0.16), // Warm Gold
        (0.24, 0.80, 0.44), // Emerald
        (0.16, 0.76, 0.86), // Cyan
        (0.16, 0.54, 1.00), // Azure
        (0.36, 0.40, 0.98), // Indigo
        (0.68, 0.34, 0.96), // Violet
        (0.96, 0.32, 0.68), // Magenta
    ];
    let tile_sz = size * 0.175;
    let gap = size * 0.068;
    let total = tile_sz * 3.0 + gap * 2.0;
    let start_x = cx - total * 0.5;
    let start_y = cy - total * 0.5;
    let tile_r = tile_sz * 0.28;

    for (idx, (tr, tg, tb)) in tiles.into_iter().enumerate() {
        let col = (idx % 3) as f32;
        let row = (idx / 3) as f32;
        let x = start_x + col * (tile_sz + gap);
        let y = start_y + row * (tile_sz + gap);
        let trr = D2D1_ROUNDED_RECT {
            rect: d2d_rect(x, y, x + tile_sz, y + tile_sz),
            radiusX: tile_r,
            radiusY: tile_r,
        };
        if let Some(g) = linear_grad(
            ctx,
            pt(x, y),
            pt(x, y + tile_sz),
            &[
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: rgba(
                        (tr + 0.12).min(1.0),
                        (tg + 0.12).min(1.0),
                        (tb + 0.12).min(1.0),
                        opacity,
                    ),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: rgba(tr * 0.86, tg * 0.86, tb * 0.86, opacity),
                },
            ],
        ) {
            ctx.FillRoundedRectangle(&trr, &g);
        }
    }
}

/// Layered Apple HIG Liquid Glass System Settings Squircle Icon (Icon Composer style!)
pub unsafe fn draw_dock_settings_icon(
    ctx: &ID2D1DeviceContext,
    cx: f32,
    cy: f32,
    size: f32,
    opacity: f32,
) {
    let half = size * 0.45;
    let rect = d2d_rect(cx - half, cy - half, cx + half, cy + half);
    let r = size * 0.225; // Apple continuous squircle corner ratio
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: r,
        radiusY: r,
    };

    if let Some(bg) = linear_grad(
        ctx,
        pt(cx, rect.top),
        pt(cx, rect.bottom),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(0.86, 0.89, 0.94, opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 0.5,
                color: rgba(0.56, 0.61, 0.68, opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(0.36, 0.40, 0.47, opacity),
            },
        ],
    ) {
        ctx.FillRoundedRectangle(&rr, &bg);
    }
    // Layered Icon Composer specular rim
    if let Some(rim) = linear_grad(
        ctx,
        pt(rect.left, rect.top),
        pt(rect.right, rect.bottom),
        &[
            D2D1_GRADIENT_STOP {
                position: 0.0,
                color: rgba(1.0, 1.0, 1.0, 0.72 * opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 0.5,
                color: rgba(1.0, 1.0, 1.0, 0.20 * opacity),
            },
            D2D1_GRADIENT_STOP {
                position: 1.0,
                color: rgba(1.0, 1.0, 1.0, 0.48 * opacity),
            },
        ],
    ) {
        ctx.DrawRoundedRectangle(&rr, &rim, 1.15, None);
    }

    let outer_r = size * 0.27;
    if let Some(dark) = solid(ctx, rgba(0.15, 0.17, 0.22, 0.86 * opacity)) {
        ctx.FillEllipse(
            &D2D1_ELLIPSE {
                point: pt(cx, cy),
                radiusX: outer_r,
                radiusY: outer_r,
            },
            &dark,
        );
    }

    let style = round_stroke_style();
    if let Some(metal) = solid(ctx, rgba(0.94, 0.96, 0.99, 0.96 * opacity)) {
        let sw = (size * 0.055).max(1.5);
        for i in 0..12 {
            let ang = (i as f32) * std::f32::consts::TAU / 12.0;
            let (s, c) = ang.sin_cos();
            let r1 = size * 0.20;
            let r2 = size * 0.32;
            ctx.DrawLine(
                pt(cx + c * r1, cy + s * r1),
                pt(cx + c * r2, cy + s * r2),
                &metal,
                sw,
                style.as_ref(),
            );
        }
        ctx.DrawEllipse(
            &D2D1_ELLIPSE {
                point: pt(cx, cy),
                radiusX: size * 0.21,
                radiusY: size * 0.21,
            },
            &metal,
            (size * 0.065).max(1.8),
            None,
        );
        ctx.FillEllipse(
            &D2D1_ELLIPSE {
                point: pt(cx, cy),
                radiusX: size * 0.085,
                radiusY: size * 0.085,
            },
            &metal,
        );
    }
}

pub unsafe fn draw_cc_quick_icon(
    ctx: &ID2D1DeviceContext,
    kind: usize,
    cx: f32,
    cy: f32,
    s: f32,
    color: D2D1_COLOR_F,
) {
    let Some(b) = solid(ctx, color) else { return };
    let style = round_stroke_style();
    let sw = (s * 0.095).max(1.3);
    match kind {
        0 => {
            let r1 = D2D1_ROUNDED_RECT {
                rect: d2d_rect(cx - s * 0.42, cy - s * 0.34, cx - s * 0.04, cy + s * 0.02),
                radiusX: s * 0.08,
                radiusY: s * 0.08,
            };
            let r2 = D2D1_ROUNDED_RECT {
                rect: d2d_rect(cx + s * 0.06, cy - s * 0.34, cx + s * 0.42, cy - s * 0.04),
                radiusX: s * 0.08,
                radiusY: s * 0.08,
            };
            let r3 = D2D1_ROUNDED_RECT {
                rect: d2d_rect(cx - s * 0.20, cy + s * 0.08, cx + s * 0.38, cy + s * 0.38),
                radiusX: s * 0.08,
                radiusY: s * 0.08,
            };
            ctx.DrawRoundedRectangle(&r1, &b, sw, None);
            ctx.DrawRoundedRectangle(&r2, &b, sw, None);
            ctx.DrawRoundedRectangle(&r3, &b, sw, None);
        }
        1 => {
            let d = s * 0.36;
            let k = s * 0.16;
            for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let x0 = cx + sx * d;
                let y0 = cy + sy * d;
                ctx.DrawLine(pt(x0, y0), pt(x0 - sx * k, y0), &b, sw, style.as_ref());
                ctx.DrawLine(pt(x0, y0), pt(x0, y0 - sy * k), &b, sw, style.as_ref());
            }
            ctx.FillEllipse(
                &D2D1_ELLIPSE {
                    point: pt(cx, cy),
                    radiusX: s * 0.09,
                    radiusY: s * 0.09,
                },
                &b,
            );
        }
        2 => {
            // Crisp 3x3 App Launcher grid icon for Control Center Quick Actions!
            let cell = s * 0.18;
            let gap = s * 0.10;
            let total = cell * 3.0 + gap * 2.0;
            let sx = cx - total * 0.5;
            let sy = cy - total * 0.5;
            for row in 0..3 {
                for col in 0..3 {
                    let x = sx + col as f32 * (cell + gap);
                    let y = sy + row as f32 * (cell + gap);
                    let rr = D2D1_ROUNDED_RECT {
                        rect: d2d_rect(x, y, x + cell, y + cell),
                        radiusX: cell * 0.28,
                        radiusY: cell * 0.28,
                    };
                    ctx.FillRoundedRectangle(&rr, &b);
                }
            }
        }
        _ => {
            for i in 0..8 {
                let ang = i as f32 * std::f32::consts::TAU / 8.0;
                let (sn, cs) = ang.sin_cos();
                ctx.DrawLine(
                    pt(cx + cs * s * 0.24, cy + sn * s * 0.24),
                    pt(cx + cs * s * 0.39, cy + sn * s * 0.39),
                    &b,
                    sw * 1.1,
                    style.as_ref(),
                );
            }
            ctx.DrawEllipse(
                &D2D1_ELLIPSE {
                    point: pt(cx, cy),
                    radiusX: s * 0.22,
                    radiusY: s * 0.22,
                },
                &b,
                sw,
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

    #[test]
    fn icon_badge_detects_large_transparency_and_chooses_contrast_backdrop() {
        let w = 64u32;
        let h = 64u32;

        // 1. Opaque full-square icon -> NO extra badge
        let opaque = vec![200u8; (w * h * 4) as usize];
        assert!(analyze_icon_badge(w, h, &opaque).is_none());

        // 2. Transparent dark icon (only center 28x28 = ~19% opaque -> 81% transparent) -> light off-white badge
        let mut dark_logo = vec![0u8; (w * h * 4) as usize];
        for y in 18..46 {
            for x in 18..46 {
                let off = ((y * w + x) * 4) as usize;
                dark_logo[off] = 140; // B
                dark_logo[off + 1] = 60; // G
                dark_logo[off + 2] = 30; // R
                dark_logo[off + 3] = 255; // A
            }
        }
        let b_dark_logo = analyze_icon_badge(w, h, &dark_logo).expect("Expected badge");
        assert!(
            !b_dark_logo.dark,
            "Dark transparent icon should receive warm off-white backdrop"
        );
        assert_eq!(b_dark_logo.src_bbox, (17.0, 17.0, 47.0, 47.0));

        // 3. Transparent bright/white icon (~81% transparent) -> deep graphite slate badge
        let mut white_glyph = vec![0u8; (w * h * 4) as usize];
        for y in 18..46 {
            for x in 18..46 {
                let off = ((y * w + x) * 4) as usize;
                white_glyph[off] = 245;
                white_glyph[off + 1] = 248;
                white_glyph[off + 2] = 250;
                white_glyph[off + 3] = 255;
            }
        }
        let b_white = analyze_icon_badge(w, h, &white_glyph).expect("Expected badge");
        assert!(
            b_white.dark,
            "Bright/white transparent icon should receive deep obsidian slate backdrop"
        );
    }

    #[test]
    fn liquid_glass_composition_pipeline_initializes() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            assert!(init(), "GlassEngine failed to initialize");
            let ready = with_engine(|e| {
                // Verify tree-shaped Dual-Scale Chromatic Liquid Glass GPU pipeline compiles with S_OK
                let host1 = e.compositor.CreateHostBackdropBrush().unwrap();
                let host2 = e.compositor.CreateHostBackdropBrush().unwrap();
                let p1 = windows::UI::Composition::CompositionEffectSourceParameter::Create(
                    &HSTRING::from("BackdropDet"),
                )
                .unwrap();
                let p2 = windows::UI::Composition::CompositionEffectSourceParameter::Create(
                    &HSTRING::from("BackdropBlm"),
                )
                .unwrap();
                let s_p1: IGraphicsEffectSource = p1.cast().unwrap();
                let s_p2: IGraphicsEffectSource = p2.cast().unwrap();
                let b1: IGraphicsEffectSource = BlurEffect {
                    name: RefCell::new(HSTRING::from("BlurDet")),
                    source: s_p1,
                    sigma: 10.5,
                }
                .into();
                let s1: IGraphicsEffectSource = SaturationEffect {
                    name: RefCell::new(HSTRING::from("SatDet")),
                    source: b1,
                    saturation: 1.55,
                }
                .into();
                let b2: IGraphicsEffectSource = BlurEffect {
                    name: RefCell::new(HSTRING::from("BlurBlm")),
                    source: s_p2,
                    sigma: 46.0,
                }
                .into();
                let s2: IGraphicsEffectSource = SaturationEffect {
                    name: RefCell::new(HSTRING::from("SatBlm")),
                    source: b2,
                    saturation: 1.95,
                }
                .into();
                let fused: IGraphicsEffect = ArithmeticCompositeEffect {
                    name: RefCell::new(HSTRING::from("LiquidFuse")),
                    source1: s1,
                    source2: s2,
                    coeffs: [0.22, 0.56, 0.36, 0.01],
                }
                .into();
                let full_ok = compile_effect_brush(
                    &e.compositor,
                    &fused,
                    &[("BackdropDet", &host1), ("BackdropBlm", &host2)],
                )
                .is_some();
                assert!(
                    full_ok,
                    "Dual-Scale Chromatic Liquid Glass ArithmeticComposite pipeline failed"
                );

                // Verify Direct2D surface render (both dark & light adaptive tones + seamless exterior shadow)
                let surf = e
                    .gfx_device
                    .CreateDrawingSurface(
                        Size {
                            Width: 320.0,
                            Height: 120.0,
                        },
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        DirectXAlphaMode::Premultiplied,
                    )
                    .unwrap();
                let interop: ICompositionDrawingSurfaceInterop = surf.cast().unwrap();
                let mut offset = POINT::default();
                let ctx = interop
                    .BeginDraw::<ID2D1DeviceContext>(None, &mut offset)
                    .unwrap();
                ctx.SetDpi(96.0, 96.0);
                ctx.SetTransform(&Matrix3x2::translation(offset.x as f32, offset.y as f32));
                ctx.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
                draw_liquid_glass_pill(
                    &ctx,
                    d2d_rect(20.0, 20.0, 300.0, 90.0),
                    24.0,
                    GlassStyle::Clear,
                    AmbientTone::from_rgb(0.25, 0.65, 0.95),
                    false,
                    Some((160.0, 40.0, 1.0)),
                    true,
                    1.0,
                );
                interop.EndDraw().unwrap();

                // Verify DirectWrite TextFormat & round StrokeStyle caching reuse identical COM pointers
                let f1 = create_text_format(13.0, TextWeight::Medium, TextAlign::Center).unwrap();
                let f2 = create_text_format(13.0, TextWeight::Medium, TextAlign::Center).unwrap();
                assert_eq!(
                    f1.as_raw(),
                    f2.as_raw(),
                    "TextFormat cache must reuse COM object"
                );
                let s1 = round_stroke_style().unwrap();
                let s2 = round_stroke_style().unwrap();
                assert_eq!(
                    s1.as_raw(),
                    s2.as_raw(),
                    "StrokeStyle cache must reuse COM object"
                );

                [GlassStyle::Clear, GlassStyle::Standard, GlassStyle::Dark]
                    .iter()
                    .all(|&s| {
                        e.brush_for(SurfaceRole::Dock, s).is_some()
                            && e.brush_for(SurfaceRole::Panel, s).is_some()
                            && e.brush_for(SurfaceRole::Bar, s).is_some()
                    })
            })
            .unwrap_or(false);
            assert!(ready, "Liquid Glass CompositionEffectBrush failed to build");
            shutdown();
            CoUninitialize();
        }
    }
}
