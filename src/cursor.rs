//! Reads the real pointer bitmap, so the viewer can draw the pointer the system uses instead of
//! a drawing of our own.
//!
//! The pointer is never part of the captured frame: baking it in would make it lag behind the
//! video by a whole frame buffer. Drawing it separately means it must look native, otherwise the
//! cursor is obviously wrong, so the shape comes straight from Windows.

use std::sync::OnceLock;

use crate::sys::{self, Pointer};

const CURSOR_SHOWING: u32 = 1;
const DIB_RGB_COLORS: u32 = 0;
const BI_RGB: u32 = 0;

#[repr(C)]
#[derive(Default)]
struct CursorInfo {
    size: u32,
    flags: u32,
    cursor: Pointer,
    screen_x: i32,
    screen_y: i32,
}

#[repr(C)]
#[derive(Default)]
struct IconInfo {
    is_icon: i32,
    hotspot_x: i32,
    hotspot_y: i32,
    mask: Pointer,
    colour: Pointer,
}

#[repr(C)]
#[derive(Default)]
struct Bitmap {
    kind: u32,
    width: i32,
    height: i32,
    width_bytes: u32,
    planes: u16,
    bits_per_pixel: u16,
    bits: Pointer,
}

#[repr(C)]
#[derive(Default)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pixels_per_meter: i32,
    y_pixels_per_meter: i32,
    colours_used: u32,
    colours_important: u32,
}

#[repr(C)]
#[derive(Default)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colours: [u32; 3],
}

type GetCursorInfoFn = unsafe extern "system" fn(*mut CursorInfo) -> i32;
type GetIconInfoFn = unsafe extern "system" fn(Pointer, *mut IconInfo) -> i32;
type GetObjectWFn = unsafe extern "system" fn(Pointer, i32, *mut Bitmap) -> i32;
type GetDIBitsFn = unsafe extern "system" fn(
    Pointer,
    Pointer,
    u32,
    u32,
    *mut u8,
    *mut BitmapInfo,
    u32,
) -> i32;
type GetDCFn = unsafe extern "system" fn(Pointer) -> Pointer;
type ReleaseDCFn = unsafe extern "system" fn(Pointer, Pointer) -> i32;
type DeleteObjectFn = unsafe extern "system" fn(Pointer) -> i32;

/// Everything needed to draw the pointer: the pixels, and where the tip is.
pub struct Shape {
    pub width: usize,
    pub height: usize,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    /// RGBA, ready for `ImageData` in the browser.
    pub rgba: Vec<u8>,
    /// Identifies the shape, so it is only sent when it changes.
    pub handle: isize,
}

/// The pointer the system is showing right now, or None when it is hidden.
pub fn current() -> Option<Shape> {
    let get_cursor_info = export::<GetCursorInfoFn>("user32.dll", b"GetCursorInfo\0")?;

    let mut info = CursorInfo {
        size: std::mem::size_of::<CursorInfo>() as u32,
        ..Default::default()
    };

    if unsafe { get_cursor_info(&mut info) } == 0 || info.flags & CURSOR_SHOWING == 0 {
        return None;
    }

    let handle = info.cursor as isize;
    let get_icon_info = export::<GetIconInfoFn>("user32.dll", b"GetIconInfo\0")?;
    let mut icon = IconInfo::default();

    if unsafe { get_icon_info(info.cursor, &mut icon) } == 0 {
        return None;
    }

    // Bitmaps returned here are ours to delete, whatever happens next.
    let shape = read_bitmap(&icon, handle);

    if !icon.mask.is_null() {
        delete(icon.mask);
    }

    if !icon.colour.is_null() {
        delete(icon.colour);
    }

    shape
}

fn read_bitmap(icon: &IconInfo, handle: isize) -> Option<Shape> {
    let get_object = export::<GetObjectWFn>("gdi32.dll", b"GetObjectW\0")?;
    let get_dibits = export::<GetDIBitsFn>("gdi32.dll", b"GetDIBits\0")?;
    let get_dc = export::<GetDCFn>("user32.dll", b"GetDC\0")?;
    let release_dc = export::<ReleaseDCFn>("user32.dll", b"ReleaseDC\0")?;

    let mut bitmap = Bitmap::default();
    let size = std::mem::size_of::<Bitmap>() as i32;

    if unsafe { get_object(icon.colour, size, &mut bitmap) } == 0 {
        return None;
    }

    if bitmap.width <= 0 || bitmap.height <= 0 || bitmap.width > 256 || bitmap.height > 256 {
        return None;
    }

    let width = bitmap.width as usize;
    let height = bitmap.height as usize;
    let mut bytes = vec![0u8; width * height * 4];

    let mut info = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: width as i32,
            // Negative height asks for top down rows, which is the order everything else here
            // expects.
            height: -(height as i32),
            planes: 1,
            bit_count: 32,
            compression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };

    let screen = unsafe { get_dc(std::ptr::null_mut()) };

    if screen.is_null() {
        return None;
    }

    let copied = unsafe {
        get_dibits(
            screen,
            icon.colour,
            0,
            height as u32,
            bytes.as_mut_ptr(),
            &mut info,
            DIB_RGB_COLORS,
        )
    };

    unsafe { release_dc(std::ptr::null_mut(), screen) };

    if copied == 0 {
        return None;
    }

    // Windows hands back BGRA; ImageData wants RGBA.
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    Some(Shape {
        width,
        height,
        hotspot_x: icon.hotspot_x,
        hotspot_y: icon.hotspot_y,
        rgba: bytes,
        handle,
    })
}

fn delete(bitmap: Pointer) {
    if let Some(delete_object) = export::<DeleteObjectFn>("gdi32.dll", b"DeleteObject\0") {
        unsafe { delete_object(bitmap) };
    }
}

/// Looks up a function once and caches it, the same way the rest of the project reaches Win32.
fn export<T: Copy>(module: &str, name: &[u8]) -> Option<T> {
    static CACHE: OnceLock<()> = OnceLock::new();
    let _ = CACHE.get_or_init(|| ());

    let library = sys::load_library(module)?;
    let address = sys::symbol(library, name)?;

    Some(unsafe { std::mem::transmute_copy::<Pointer, T>(&address) })
}
