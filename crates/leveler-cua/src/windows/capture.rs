//! Window and desktop pixels through GDI.
//!
//! `PrintWindow` with `PW_RENDERFULLCONTENT` asks a window to render itself,
//! including DirectComposition content. When it refuses, an on-screen window
//! falls back to a screen blit of its DWM frame bounds. The desktop path
//! blits the virtual screen. Both return physical pixels; the worker thread is
//! per-monitor DPI aware, so the reported scale is always 1.0.

use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::error::{CuaError, CuaResult};
use crate::types::{Screenshot, WindowInfo};

use super::NAME;

/// `PW_RENDERFULLCONTENT` is declared as a plain `u32` in
/// `Win32_UI_WindowsAndMessaging` but `PrintWindow` takes the typed flag.
const RENDER_FULL_CONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);

/// One memory bitmap with the device contexts it needs. Dropping the canvas
/// restores the previous selection and releases every handle, so a failed
/// capture cannot leak GDI objects.
struct Canvas {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: i32,
    height: i32,
}

impl Canvas {
    fn new(width: i32, height: i32) -> CuaResult<Self> {
        if width <= 0 || height <= 0 {
            return Err(backend(format!(
                "the capture surface is {width}x{height} pixels"
            )));
        }
        let screen = unsafe { GetDC(None) };
        if screen.0.is_null() {
            return Err(backend("could not get the screen device context"));
        }
        let memory = unsafe { CreateCompatibleDC(Some(screen)) };
        if memory.0.is_null() {
            unsafe { ReleaseDC(None, screen) };
            return Err(backend("could not create a memory device context"));
        }
        let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
        if bitmap.0.is_null() {
            unsafe {
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
            }
            return Err(backend("could not create a compatible bitmap"));
        }
        let previous = unsafe { SelectObject(memory, HGDIOBJ(bitmap.0)) };
        Ok(Self {
            screen,
            memory,
            bitmap,
            previous,
            width,
            height,
        })
    }

    fn png(&self) -> CuaResult<Vec<u8>> {
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width,
                // A negative height requests top-down rows.
                biHeight: -self.height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; self.width as usize * self.height as usize * 4];
        let rows = unsafe {
            GetDIBits(
                self.memory,
                self.bitmap,
                0,
                self.height as u32,
                Some(pixels.as_mut_ptr().cast::<c_void>()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if rows == 0 {
            return Err(backend("GetDIBits returned no scan lines"));
        }
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        encode_png(&pixels, self.width as u32, self.height as u32)
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.memory);
            ReleaseDC(None, self.screen);
        }
    }
}

pub(super) fn capture(window: Option<&WindowInfo>) -> CuaResult<Screenshot> {
    match window {
        Some(window) => capture_window(window),
        None => capture_desktop(),
    }
}

fn capture_window(window: &WindowInfo) -> CuaResult<Screenshot> {
    let hwnd = super::hwnd(window.id);
    let width = window.bounds.width.round() as i32;
    let height = window.bounds.height.round() as i32;
    let canvas = Canvas::new(width, height)?;
    let rendered = unsafe { PrintWindow(hwnd, canvas.memory, RENDER_FULL_CONTENT) }.as_bool();
    if !rendered {
        if !window.on_screen {
            return Err(backend(format!(
                "the window did not render itself and is not on screen (id={})",
                window.id
            )));
        }
        unsafe {
            BitBlt(
                canvas.memory,
                0,
                0,
                width,
                height,
                Some(canvas.screen),
                window.bounds.x.round() as i32,
                window.bounds.y.round() as i32,
                SRCCOPY,
            )
        }
        .map_err(|error| {
            backend(format!(
                "blit the window from the screen: {}",
                error.message()
            ))
        })?;
    }
    let png = canvas.png()?;
    Ok(Screenshot {
        png,
        width: width as u32,
        height: height as u32,
        scale: 1.0,
        target: format!("window {} {:?}", window.id, window.title),
        window: Some(window.clone()),
    })
}

fn capture_desktop() -> CuaResult<Screenshot> {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    let canvas = Canvas::new(width, height)?;
    unsafe {
        BitBlt(
            canvas.memory,
            0,
            0,
            width,
            height,
            Some(canvas.screen),
            x,
            y,
            SRCCOPY,
        )
    }
    .map_err(|error| backend(format!("blit the virtual desktop: {}", error.message())))?;
    let png = canvas.png()?;
    Ok(Screenshot {
        png,
        width: width as u32,
        height: height as u32,
        scale: 1.0,
        target: format!("virtual desktop {x},{y} {width}x{height}"),
        window: None,
    })
}

fn encode_png(pixels: &[u8], width: u32, height: u32) -> CuaResult<Vec<u8>> {
    use image::ImageEncoder as _;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(pixels, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| backend(format!("encode the capture as PNG: {error}")))?;
    Ok(png)
}

fn backend(detail: impl Into<String>) -> CuaError {
    CuaError::Backend {
        backend: NAME,
        detail: detail.into(),
    }
}
