// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Bounded screen capture output shared by the action spine and its authoring surface.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_CAPTURE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureTarget {
    #[default]
    Screen,
    Window,
    Region,
}

impl CaptureTarget {
    pub fn label(self) -> &'static str {
        match self { Self::Screen => "screen", Self::Window => "window", Self::Region => "region" }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }

impl Rect {
    pub fn intersection(self, other: Self) -> Result<Self, &'static str> {
        let rect = Self {
            left: self.left.max(other.left), top: self.top.max(other.top),
            right: self.right.min(other.right), bottom: self.bottom.min(other.bottom),
        };
        rect.dimensions()?;
        Ok(rect)
    }

    pub fn dimensions(self) -> Result<(u32, u32), &'static str> {
        let width = i64::from(self.right) - i64::from(self.left);
        let height = i64::from(self.bottom) - i64::from(self.top);
        if width <= 0 || height <= 0 || width > u32::MAX as i64 || height > u32::MAX as i64 {
            return Err("capture rectangle is empty or invalid");
        }
        let (width, height) = (width as u32, height as u32);
        let bytes = usize::try_from(width).ok().and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h))).and_then(|p| p.checked_mul(4));
        if bytes.is_none_or(|bytes| bytes > MAX_CAPTURE_BYTES) { return Err("capture exceeds the 128 MiB pixel limit"); }
        Ok((width, height))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels { pub width: u32, pub height: u32, pub bgra: Vec<u8> }

/// Run one screenshot action. `path` is created exclusively so an existing file is never replaced.
pub fn capture(target: CaptureTarget, path: Option<&str>, clipboard: bool) -> String {
    #[cfg(windows)]
    let window_rect = if target == CaptureTarget::Window { windows::foreground_target().map(|(_, rect)| rect) } else { None };
    #[cfg(not(windows))]
    let window_rect = None;
    capture_resolved(target, window_rect, path, clipboard)
}

fn capture_resolved(target: CaptureTarget, window_rect: Option<Rect>, path: Option<&str>, clipboard: bool) -> String {
    if !crate::action::input_armed() { return "screenshot [disarmed]".into(); }
    if !clipboard && path.is_none() { return "screenshot: choose the clipboard or a PNG path".into(); }
    #[cfg(not(windows))]
    { let _ = (target, path, clipboard); return "screenshot: capture is unsupported on this platform".into(); }
    #[cfg(windows)]
    {
        let rect = match target {
            CaptureTarget::Screen => windows::virtual_screen_rect(),
            CaptureTarget::Window => match window_rect { Some(rect) => windows::visible_rect(rect), None => Err("no trigger-time foreground window") },
            CaptureTarget::Region => match select_region() { Some(rect) => windows::visible_rect(rect), None => return "screenshot: region selection cancelled or unavailable".into() },
        };
        let rect = match rect { Ok(rect) => rect, Err(error) => return format!("screenshot: {error}") };
        if !crate::action::input_armed() { return "screenshot [disarmed]".into(); }
        let (width, height) = match rect.dimensions() { Ok(dims) => dims, Err(error) => return format!("screenshot: {error}") };
        let pixels = match windows::capture_rect(rect, width, height) { Some(pixels) => pixels, None => return "screenshot: screen capture failed".into() };
        let mut file_error = None;
        let saved_path = if let Some(requested_path) = path {
            if !crate::action::input_armed() { return "screenshot [disarmed]".into(); }
            match encode_png(&pixels) {
                Ok(png) => {
                    let result = if requested_path.is_empty() {
                        save_default(&png)
                    } else {
                        let path = PathBuf::from(requested_path);
                        save_exclusive(&path, &png).map(|()| path)
                    };
                    match result { Ok(path) => Some(path), Err(error) => { file_error = Some(error); None } }
                }
                Err(error) => { file_error = Some(format!("PNG encoding failed: {error}")); None }
            }
        } else { None };
        if !clipboard && file_error.is_some() {
            return format!("screenshot: {}", file_error.unwrap_or_default());
        }
        if clipboard {
            let (original, sequence) = match crate::pocket::snapshot_for_action() {
                Ok(snapshot) => snapshot,
                Err("busy") => return capture_failure_with_file("clipboard is busy", saved_path.as_deref(), file_error.as_deref()),
                Err("unsupported") => return capture_failure_with_file("clipboard format cannot be preserved", saved_path.as_deref(), file_error.as_deref()),
                Err(_) => return capture_failure_with_file("clipboard is unavailable", saved_path.as_deref(), file_error.as_deref()),
            };
            if !crate::action::input_armed() { return capture_failure_with_file("disarmed before clipboard commit", saved_path.as_deref(), file_error.as_deref()); }
            let dib = match to_dib(&pixels) { Some(dib) => dib, None => return capture_failure_with_file("DIB conversion failed", saved_path.as_deref(), file_error.as_deref()) };
            let replacement = crate::pocket::Pocket { formats: vec![crate::pocket::ClipFormat { id: 8, bytes: dib }] };
            if let Err(error) = crate::pocket::replace_at_sequence(&replacement, &original, sequence) {
                let failure = match error {
                    "changed" => "clipboard changed; capture not copied",
                    "disarmed" => "clipboard write disarmed",
                    "write-failed" => "clipboard write failed; original restored",
                    "rollback-failed" => "clipboard write and rollback failed; contents may be incomplete",
                    _ => "clipboard is busy; capture not copied",
                };
                return capture_failure_with_file(failure, saved_path.as_deref(), file_error.as_deref());
            }
        }
        let where_to = if let Some(path) = saved_path { format!(" saved to {}", path.display()) } else { String::new() };
        format!("screenshot {}×{}{}{}{}", width, height, if clipboard { " copied" } else { "" }, where_to,
            file_error.map_or(String::new(), |error| format!("; PNG output failed: {error}")))
    }
}

fn capture_failure(reason: &str, saved_path: Option<&std::path::Path>) -> String {
    format!("screenshot: {reason}{}", saved_path.map_or(String::new(), |path| format!("; saved to {}", path.display())))
}

fn capture_failure_with_file(reason: &str, saved_path: Option<&std::path::Path>, file_error: Option<&str>) -> String {
    format!("{}{}", capture_failure(reason, saved_path), file_error.map_or(String::new(), |error| format!("; PNG output failed: {error}")))
}

/// Queue a capture on the bounded worker pool. Only one selection/capture may be outstanding.
pub fn request(target: CaptureTarget, path: Option<String>, clipboard: bool) -> String {
    request_inner(target, path, clipboard, None)
}

/// Queue a capture against the trigger-time foreground window retained in [`crate::macros::Context`].
pub fn request_for_window(target: CaptureTarget, path: Option<String>, clipboard: bool, hwnd: isize) -> String {
    request_inner(target, path, clipboard, Some(hwnd))
}

fn request_inner(target: CaptureTarget, path: Option<String>, clipboard: bool, retained_window: Option<isize>) -> String {
    if !crate::action::input_armed() { return "screenshot [disarmed]".into(); }
    if !clipboard && path.is_none() { return "screenshot: choose the clipboard or a PNG path".into(); }
    if let Err(reason) = validate_region_request(target, region_selector_installed()) {
        return format!("screenshot: {reason}");
    }
    let window_rect = if target == CaptureTarget::Window {
        resolve_window_target(retained_window,
            #[cfg(windows)]
            || windows::foreground_target().map(|(_, rect)| rect),
            #[cfg(not(windows))]
            || None,
            #[cfg(windows)]
            |hwnd| windows::window_rect(hwnd),
            #[cfg(not(windows))]
            |_| None,
        )
    } else { None };
    #[cfg(windows)]
    if target == CaptureTarget::Window && retained_window.is_some() && window_rect.is_none() {
        return "screenshot: trigger-time window is no longer available".into();
    }
    static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::sync::atomic::Ordering;
    if ACTIVE.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return "screenshot: capture already in progress".into();
    }
    let queued = crate::worker::spawn_detached("neuron-screenshot", move || {
        struct Reset(&'static std::sync::atomic::AtomicBool);
        impl Drop for Reset { fn drop(&mut self) { self.0.store(false, Ordering::SeqCst); } }
        let _reset = Reset(&ACTIVE);
        let result = capture_resolved(target, window_rect, path.as_deref(), clipboard);
        publish_completion(result);
    });
    if queued { "screenshot: capture queued".into() }
    else { ACTIVE.store(false, Ordering::SeqCst); "screenshot: worker is unavailable".into() }
}

fn resolve_window_target(
    retained: Option<isize>,
    current: impl FnOnce() -> Option<Rect>,
    resolve: impl FnOnce(isize) -> Option<Rect>,
) -> Option<Rect> {
    match retained {
        Some(hwnd) => resolve(hwnd),
        None => current(),
    }
}

fn encode_png(pixels: &Pixels) -> Result<Vec<u8>, String> {
    let expected = usize::try_from(pixels.width).ok().and_then(|w| usize::try_from(pixels.height).ok().and_then(|h| w.checked_mul(h))).and_then(|n| n.checked_mul(4))
        .filter(|bytes| *bytes <= MAX_CAPTURE_BYTES).ok_or_else(|| "invalid pixel dimensions".to_string())?;
    if pixels.bgra.len() != expected { return Err("pixel buffer does not match dimensions".into()); }
    let mut rgb = Vec::with_capacity((pixels.width as usize).saturating_mul(pixels.height as usize).saturating_mul(3));
    for bgra in pixels.bgra.chunks_exact(4) { rgb.extend_from_slice(&[bgra[2], bgra[1], bgra[0]]); }
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, pixels.width, pixels.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(&rgb).map_err(|e| e.to_string())?;
    }
    Ok(output)
}

fn to_dib(pixels: &Pixels) -> Option<Vec<u8>> {
    let (width, height) = (i32::try_from(pixels.width).ok()?, i32::try_from(pixels.height).ok()?);
    if pixels.bgra.len() != (pixels.width as usize).checked_mul(pixels.height as usize)?.checked_mul(4)? { return None; }
    let mut out = Vec::with_capacity(40 + pixels.bgra.len());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&(-height).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(pixels.bgra.len() as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for pixel in pixels.bgra.chunks_exact(4) { out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]); }
    Some(out)
}

fn default_path() -> PathBuf {
    crate::runroot::run_root().join("screenshots")
        .join(format!("neuron-{}.png", chrono_free_timestamp()))
}

fn chrono_free_timestamp() -> String {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().to_string()
}

fn save_exclusive(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists { format!("file already exists: {}", path.display()) } else { e.to_string() }
    })?;
    use std::io::Write;
    if let Err(error) = file.write_all(bytes) { let _ = std::fs::remove_file(path); return Err(error.to_string()); }
    Ok(())
}

fn save_default(bytes: &[u8]) -> Result<PathBuf, String> {
    let base = default_path();
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("neuron-shot");
    for index in 0..1000u32 {
        let name = if index == 0 { format!("{stem}.png") } else { format!("{stem}-{index}.png") };
        let path = base.with_file_name(name);
        match save_exclusive(&path, bytes) {
            Ok(()) => return Ok(path),
            Err(error) if error.starts_with("file already exists:") => continue,
            Err(error) => return Err(error),
        }
    }
    Err("could not choose a unique screenshot filename".into())
}

pub type RegionSelector = std::sync::Arc<dyn Fn() -> Option<Rect> + Send + Sync>;
type CompletionSink = std::sync::Arc<dyn Fn(String) + Send + Sync>;
static REGION_SELECTOR: std::sync::OnceLock<std::sync::Mutex<Option<RegionSelector>>> = std::sync::OnceLock::new();
static COMPLETION_SINK: std::sync::OnceLock<std::sync::Mutex<Option<CompletionSink>>> = std::sync::OnceLock::new();

/// Install the app's native region overlay. The callback runs on the screenshot worker and returns
/// only after the selection is accepted or cancelled.
pub fn install_region_selector<F>(selector: F)
where F: Fn() -> Option<Rect> + Send + Sync + 'static {
    *REGION_SELECTOR.get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::sync::Arc::new(selector));
}

fn select_region() -> Option<Rect> {
    let selector = REGION_SELECTOR.get()?.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()?;
    selector()
}

fn region_selector_installed() -> bool {
    REGION_SELECTOR.get().is_some_and(|cell| cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some())
}

fn validate_region_request(target: CaptureTarget, selector_installed: bool) -> Result<(), &'static str> {
    if target == CaptureTarget::Region && !selector_installed {
        Err("region selection requires the resident app")
    } else {
        Ok(())
    }
}

/// Install the host's nonblocking completion surface for screenshot results.
pub fn install_completion_sink<F>(sink: F)
where F: Fn(String) + Send + Sync + 'static {
    *COMPLETION_SINK.get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::sync::Arc::new(sink));
}

fn publish_completion(message: String) {
    if let Some(sink) = COMPLETION_SINK.get().and_then(|cell| cell.lock().ok().and_then(|g| g.clone())) { sink(message); }
}

#[cfg(windows)]
mod windows {
    use super::{Pixels, Rect};
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetSystemMetrics, GetWindowRect, IsWindow, SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    struct ScreenDc(windows_sys::Win32::Graphics::Gdi::HDC);
    impl Drop for ScreenDc { fn drop(&mut self) {
        // SAFETY: acquired by GetDC(NULL) and released exactly once.
        unsafe { ReleaseDC(std::ptr::null_mut(), self.0); }
    } }
    struct MemoryDc(windows_sys::Win32::Graphics::Gdi::HDC);
    impl Drop for MemoryDc { fn drop(&mut self) {
        // SAFETY: created by CreateCompatibleDC and released exactly once.
        unsafe { DeleteDC(self.0); }
    } }
    struct Bitmap(windows_sys::Win32::Graphics::Gdi::HBITMAP);
    impl Drop for Bitmap { fn drop(&mut self) {
        // SAFETY: created by CreateDIBSection and deselected from its DC before this guard drops.
        unsafe { DeleteObject(self.0); }
    } }

    pub fn virtual_screen_rect() -> Result<Rect, &'static str> {
        // SAFETY: GetSystemMetrics takes scalar indexes and returns values; no pointers are passed.
        unsafe {
            let (left, top, width, height) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
            if width <= 0 || height <= 0 { return Err("virtual desktop bounds are unavailable"); }
            Ok(Rect { left, top, right: left.saturating_add(width), bottom: top.saturating_add(height) })
        }
    }

    pub fn visible_rect(rect: Rect) -> Result<Rect, &'static str> {
        let desktop = virtual_screen_rect()?;
        rect.intersection(desktop)
    }

    pub fn foreground_target() -> Option<(isize, Rect)> {
        // SAFETY: the RECT is initialized here and passed writable to GetWindowRect for the
        // foreground HWND returned immediately before the call.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() { return None; }
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &raw mut rect) == 0 { return None; }
            Some((hwnd as isize, Rect { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom }))
        }
    }

    pub fn window_rect(handle: isize) -> Option<Rect> {
        // SAFETY: `handle` is the captured HWND; IsWindow validates it before GetWindowRect writes to initialized RECT storage.
        unsafe {
            let hwnd = handle as windows_sys::Win32::Foundation::HWND;
            if hwnd.is_null() || IsWindow(hwnd) == 0 { return None; }
            let mut rect: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &raw mut rect) == 0 { return None; }
            Some(Rect { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom })
        }
    }

    pub fn capture_rect(rect: Rect, width: u32, height: u32) -> Option<Pixels> {
        let (w, h) = (i32::try_from(width).ok()?, i32::try_from(height).ok()?);
        let byte_len = usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?.checked_mul(4)?;
        if byte_len > super::MAX_CAPTURE_BYTES { return None; }
        // SAFETY: GDI handles use RAII guards; the DIB is checked to match the capped allocation and
        // is deselected before cleanup. The copied pixels are owned before the bitmap is released.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() { return None; }
            let screen = ScreenDc(screen);
            let memory = CreateCompatibleDC(screen.0);
            if memory.is_null() { return None; }
            let memory = MemoryDc(memory);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h,
                biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, biSizeImage: 0,
                biXPelsPerMeter: 0, biYPelsPerMeter: 0, biClrUsed: 0, biClrImportant: 0,
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(screen.0, &raw const info, DIB_RGB_COLORS, &raw mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() { if !bitmap.is_null() { DeleteObject(bitmap); } return None; }
            let bitmap = Bitmap(bitmap);
            let previous = SelectObject(memory.0, bitmap.0);
            if previous.is_null() || previous as isize == -1 { return None; }
            let copied = BitBlt(memory.0, 0, 0, w, h, screen.0, rect.left, rect.top, SRCCOPY) != 0;
            let restored = SelectObject(memory.0, previous);
            if restored.is_null() || restored as isize == -1 {
                drop(memory);
                drop(bitmap);
                return None;
            }
            if !copied { return None; }
            let mut bgra = Vec::with_capacity(byte_len);
            // SAFETY: CreateDIBSection returned `bits`, the selected bitmap owns `byte_len` bytes,
            // and it remains alive until the owned Vec copy completes.
            bgra.extend_from_slice(std::slice::from_raw_parts(bits.cast::<u8>(), byte_len));
            for pixel in bgra.chunks_exact_mut(4) { pixel[3] = 255; }
            Some(Pixels { width, height, bgra })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_accept_negative_desktop_coordinates_and_reject_growth() {
        assert_eq!(Rect { left: -1920, top: -200, right: 0, bottom: 880 }.dimensions(), Ok((1920, 1080)));
        assert!(Rect { left: 0, top: 0, right: i32::MAX, bottom: i32::MAX }.dimensions().is_err());
    }

    #[test]
    fn headless_region_requests_are_rejected_before_queueing() {
        assert_eq!(validate_region_request(CaptureTarget::Region, false), Err("region selection requires the resident app"));
        assert_eq!(validate_region_request(CaptureTarget::Screen, false), Ok(()));
        assert_eq!(validate_region_request(CaptureTarget::Region, true), Ok(()));
    }

    #[test]
    fn retained_window_never_falls_back_to_a_new_foreground_window() {
        let mut current_called = false;
        let rect = resolve_window_target(Some(42), || { current_called = true; Some(Rect { left: 0, top: 0, right: 10, bottom: 10 }) }, |_| None);
        assert_eq!(rect, None);
        assert!(!current_called);
        assert_eq!(resolve_window_target(None, || Some(Rect { left: -10, top: 0, right: 0, bottom: 10 }), |_| None), Some(Rect { left: -10, top: 0, right: 0, bottom: 10 }));
    }

    #[test]
    fn dib_is_top_down_rgb_and_opaque() {
        let dib = to_dib(&Pixels { width: 1, height: 1, bgra: vec![1, 2, 3, 4] }).unwrap();
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), -1);
        assert_eq!(&dib[40..44], &[1, 2, 3, 255]);
    }

    #[test]
    fn crop_intersection_respects_negative_virtual_desktop_coordinates() {
        let window = Rect { left: -2200, top: 100, right: -100, bottom: 900 };
        let desktop = Rect { left: -1920, top: 0, right: 1920, bottom: 1080 };
        assert_eq!(window.intersection(desktop), Ok(Rect { left: -1920, top: 100, right: -100, bottom: 900 }));
        assert!(Rect { left: -2200, top: 0, right: -2000, bottom: 10 }.intersection(desktop).is_err());
    }

    #[test]
    fn png_output_round_trips_synthetic_bgra_pixels() {
        let pixels = Pixels { width: 2, height: 1, bgra: vec![3, 2, 1, 255, 30, 20, 10, 255] };
        let png = encode_png(&pixels).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut data = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut data).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&data[..6], &[1, 2, 3, 10, 20, 30]);
    }

    #[test]
    fn explicit_png_path_never_overwrites_existing_file() {
        let path = std::env::temp_dir().join(format!("neuron-shot-collision-{}-{}.png", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
        std::fs::write(&path, b"existing").unwrap();
        assert!(save_exclusive(&path, b"new").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        std::fs::remove_file(path).unwrap();
    }
}
