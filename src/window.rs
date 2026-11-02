//! The viewer window: a plain Win32 window that paints the newest decoded frame and turns the
//! keyboard and pointer into the messages the host understands.
//!
//! GDI rather than D3D11 on purpose. `StretchDIBits` scales and presents in one call, so changing
//! the window size never restarts the decoder — with a decoder that outputs exactly the window
//! size, every resize would cost a key frame and a frozen picture. If a bigger picture turns out to
//! be too slow here, this is the piece to replace.
//!
//! The Win32 entry points are reached through [`crate::sys::export`], the same way the capture side
//! reaches D3D, so nothing new is linked and no crate is added.

use std::sync::{Arc, Mutex, OnceLock};

use crate::cursor::Shape;
use crate::decode::Frame;
use crate::sys::{self, BitmapInfo, BitmapInfoHeader, Pointer};

pub type Hwnd = isize;
type Hdc = Pointer;

const WM_DESTROY: u32 = 0x0002;
const WM_SIZE: u32 = 0x0005;
const WM_KILLFOCUS: u32 = 0x0008;
const WM_PAINT: u32 = 0x000F;
const WM_CLOSE: u32 = 0x0010;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_SETCURSOR: u32 = 0x0020;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;

const CS_HREDRAW: u32 = 0x0002;
const CS_VREDRAW: u32 = 0x0001;
const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_POPUP: u32 = 0x8000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;
const CW_USEDEFAULT: i32 = 0x8000_0000u32 as i32;
const SW_SHOW: i32 = 5;
const IDC_ARROW: usize = 32512;
const GWL_STYLE: i32 = -16;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_FRAMECHANGED: u32 = 0x0020;
const SWP_SHOWWINDOW: u32 = 0x0040;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00CC_0020;
const BLACKNESS: u32 = 0x00FF_0062;
const HALFTONE: i32 = 4;
const VK_ESCAPE: usize = 0x1B;
const VK_F11: usize = 0x7A;

/// What the window needs from the application behind it.
pub trait Target: Send + Sync {
    /// One protocol message for the host.
    fn send(&self, message: &str);
    /// Where the host's pointer is, in frame pixels, and what it looks like right now.
    fn cursor(&self) -> Option<(Shape, (i32, i32))>;
    /// The user closed the window.
    fn closed(&self);
}

static TARGET: OnceLock<Arc<dyn Target>> = OnceLock::new();
static FRAME: OnceLock<Arc<Mutex<Option<Frame>>>> = OnceLock::new();
static WINDOW: Mutex<Option<Hwnd>> = Mutex::new(None);
/// Where the picture sits inside the client area, updated on every paint and resize.
static FIT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);
/// The last pointer position seen inside the window, in client coordinates.
static POINTER: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static FULLSCREEN: Mutex<bool> = Mutex::new(false);
static CURSOR_BITMAP: Mutex<Option<CursorBitmap>> = Mutex::new(None);
static WHEEL: Mutex<i32> = Mutex::new(0);

struct CursorBitmap {
    handle: isize,
    /// GDI handles, kept as integers so the cache can live in a `static`: raw pointers are not
    /// `Send`, and what these are used for always happens on the window thread anyway.
    bitmap: isize,
    dc: isize,
}

type WndProcFn = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WndClassEx {
    size: u32,
    style: u32,
    window_proc: Option<WndProcFn>,
    class_extra: i32,
    window_extra: i32,
    instance: Pointer,
    icon: Pointer,
    cursor: Pointer,
    background: Pointer,
    menu_name: *const u16,
    class_name: *const u16,
    icon_small: Pointer,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Msg {
    window: Hwnd,
    message: u32,
    padding: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: sys::Point,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlendFunction {
    operation: u8,
    flags: u8,
    constant_alpha: u8,
    alpha_format: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MonitorInfo {
    size: u32,
    monitor: sys::Rect,
    work: sys::Rect,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PaintStruct {
    dc: Hdc,
    erase: i32,
    paint: sys::Rect,
    restore: i32,
    inc_update: i32,
    reserved: [u8; 32],
}

/// Registers a class, creates the window and runs its message loop. Blocks until the window closes.
pub fn run(
    target: Arc<dyn Target>,
    frame: Arc<Mutex<Option<Frame>>>,
    fullscreen: bool,
) -> Result<(), String> {
    let _ = TARGET.set(target);
    let _ = FRAME.set(frame);

    let register = export::<unsafe extern "system" fn(*const WndClassEx) -> u16>(
        "user32.dll",
        b"RegisterClassExW\0",
    )
    .ok_or("cannot reach user32")?;
    let create =
        export::<CreateWindowExFn>("user32.dll", b"CreateWindowExW\0").ok_or("cannot reach user32")?;

    let class = sys::wide("OpenScreenViewer");
    let title = sys::wide("OpenScreen");

    let definition = WndClassEx {
        size: std::mem::size_of::<WndClassEx>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        window_proc: Some(window_proc),
        cursor: load_cursor(IDC_ARROW).unwrap_or(std::ptr::null_mut()),
        class_name: class.as_ptr(),
        ..Default::default()
    };

    if unsafe { register(&definition) } == 0 {
        return Err("cannot register the window class".to_string());
    }

    let window = unsafe {
        create(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    if window == 0 {
        return Err("cannot create the window".to_string());
    }

    *WINDOW.lock().unwrap() = Some(window);

    show_window(window, SW_SHOW);

    if fullscreen {
        set_fullscreen(window, true);
    }

    message_loop();

    Ok(())
}

/// Asks for a repaint. Safe to call from any thread; the frame and cursor threads use it.
pub fn invalidate() {
    let window = *WINDOW.lock().unwrap();

    let Some(window) = window else {
        return;
    };

    if let Some(api) = invalidate_rect() {
        unsafe { api(window, std::ptr::null(), 0) };
    }
}

/// Puts a line in the title bar. Called from the status thread, once a second.
pub fn set_title(text: &str) {
    let window = *WINDOW.lock().unwrap();

    if let Some(window) = window {
        let _ = set_window_text(window, &sys::wide(text));
    }
}

fn message_loop() {
    let mut message = Msg::default();

    loop {
        let result = get_message(&mut message);

        if result == 0 || result == -1 {
            return;
        }

        translate_message(&message);
        dispatch_message(&message);
    }
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

unsafe extern "system" fn window_proc(
    window: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match message {
        WM_PAINT => {
            // BeginPaint is what clears the update region. Painting from GetDC without it leaves
            // the window permanently invalid, and Windows then sends WM_PAINT in a tight loop —
            // nine hundred repaints a second was the first symptom of that here.
            let Some((begin, end)) = begin_and_end_paint() else {
                return def_window_proc(window, message, wparam, lparam);
            };

            let mut state = PaintStruct::default();
            let hdc = unsafe { begin(window, &mut state) };

            if !hdc.is_null() {
                paint(hdc, window);
                unsafe { end(window, &mut state) };
            }

            0
        }
        // The picture covers the whole client area; the default erase would only flicker.
        WM_ERASEBKGND => 1,
        WM_SIZE => {
            *FIT.lock().unwrap() = None;
            invalidate();
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            // F11 toggles fullscreen, and Escape leaves it: the only two keys this window keeps.
            if wparam == VK_F11 || (wparam == VK_ESCAPE && *FULLSCREEN.lock().unwrap()) {
                if (lparam & (1 << 30)) == 0 {
                    toggle_fullscreen(window);
                }

                return 0;
            }

            key(lparam, true);
            0
        }
        WM_KEYUP | WM_SYSKEYUP => {
            if wparam != VK_F11 {
                key(lparam, false);
            }

            0
        }
        WM_KILLFOCUS => {
            // Nothing the host believes is held may stay held once this window stops receiving keys.
            send("r");
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = position(lparam);
            *POINTER.lock().unwrap() = Some((x, y));
            pointer(window, x, y);
            0
        }
        WM_LBUTTONDOWN => button(window, lparam, 0, true),
        WM_LBUTTONUP => button(window, lparam, 0, false),
        WM_MBUTTONDOWN => button(window, lparam, 1, true),
        WM_MBUTTONUP => button(window, lparam, 1, false),
        WM_RBUTTONDOWN => button(window, lparam, 2, true),
        WM_RBUTTONUP => button(window, lparam, 2, false),
        WM_MOUSEWHEEL => {
            // A wheel notch is 120 whatever the device sends; touchpads send fractions, so they
            // are accumulated instead of being rounded away.
            let mut wheel = WHEEL.lock().unwrap();
            *wheel += ((wparam >> 16) as u16 as i16) as i32;

            let notches = *wheel / 120;

            if notches != 0 {
                *wheel -= notches * 120;
                send(&format!("w {}", -notches));
            }

            0
        }
        WM_SETCURSOR => {
            // Over the picture the host's own pointer is drawn, so a second local arrow would be
            // one more thing moving a few milliseconds off.
            if *FULLSCREEN.lock().unwrap() || over_picture() {
                if let Some(set_cursor) = set_cursor_api() {
                    unsafe { set_cursor(std::ptr::null_mut()) };
                }
            }

            1
        }
        WM_CLOSE => {
            destroy_window(window);
            0
        }
        WM_DESTROY => {
            if let Some(target) = TARGET.get() {
                target.closed();
            }

            if let Some(bitmap) = CURSOR_BITMAP.lock().unwrap().take() {
                destroy_bitmap(bitmap);
            }

            *WINDOW.lock().unwrap() = None;
            post_quit();
            0
        }
        _ => def_window_proc(window, message, wparam, lparam),
    }
}

/// One key, straight from the message's own scan code — which is what the host wants.
fn key(lparam: isize, down: bool) {
    let scan = ((lparam >> 16) & 0xFF) as u16;
    let extended = ((lparam >> 24) & 1) != 0;
    // A held key arrives as a stream of downs here; the host's keyboard does its own repeating and
    // would type the character twice as fast if both did.
    let repeat = (lparam & (1 << 30)) != 0;

    if scan == 0 || (down && repeat) {
        return;
    }

    send(&format!("s {scan} {} {}", extended as u8, down as u8));
}

fn position(lparam: isize) -> (i32, i32) {
    (
        (lparam & 0xFFFF) as u16 as i16 as i32,
        ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

fn pointer(window: Hwnd, x: i32, y: i32) {
    if let Some(fraction) = fraction_of(window, x, y) {
        send(&format!("m {} {}", fraction.0, fraction.1));
    }
}

fn button(window: Hwnd, lparam: isize, which: u32, down: bool) -> isize {
    let (x, y) = position(lparam);

    // A click lands where the pointer is, so move first: without this the click would land
    // wherever the host's pointer happened to be left.
    if down {
        pointer(window, x, y);
        let _ = set_capture(window);
    } else {
        release_capture();
    }

    send(&format!("b {which} {}", down as u8));

    0
}

fn send(message: &str) {
    if let Some(target) = TARGET.get() {
        target.send(message);
    }
}

// ---------------------------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------------------------

fn paint(hdc: Hdc, window: Hwnd) {
    let mut client = sys::Rect::default();
    get_client_rect(window, &mut client);

    let area = (client.right - client.left, client.bottom - client.top);

    if area.0 <= 0 || area.1 <= 0 {
        return;
    }

    // Letterbox background: black, like every other remote desktop window.
    pat_blt(hdc, 0, 0, area.0, area.1);

    let Some(slot) = FRAME.get() else {
        return;
    };

    let guard = slot.lock().unwrap();

    let Some(frame) = guard.as_ref() else {
        return;
    };

    let fit = fit((frame.width, frame.height), area);

    *FIT.lock().unwrap() = Some(fit);

    blit(hdc, frame, fit);
    draw_cursor(hdc, fit, frame.width);
}

/// Where a frame goes inside a client area, keeping its aspect ratio and centring it.
fn fit(source: (usize, usize), area: (i32, i32)) -> (i32, i32, i32, i32) {
    let width = source.0.max(1) as f64;
    let height = source.1.max(1) as f64;
    let scale = (area.0.max(1) as f64 / width).min(area.1.max(1) as f64 / height);

    let scaled_width = (width * scale).round().max(1.0) as i32;
    let scaled_height = (height * scale).round().max(1.0) as i32;

    (
        (area.0 - scaled_width) / 2,
        (area.1 - scaled_height) / 2,
        scaled_width,
        scaled_height,
    )
}

/// The 0..10000 fraction of the picture under a client position, or None outside it.
fn fraction(rect: (i32, i32, i32, i32), x: i32, y: i32) -> Option<(i32, i32)> {
    let (left, top, width, height) = rect;
    let local_x = x - left;
    let local_y = y - top;

    if width <= 0
        || height <= 0
        || local_x < 0
        || local_y < 0
        || local_x >= width
        || local_y >= height
    {
        return None;
    }

    Some((
        (local_x * 10_000 / width).min(9_999),
        (local_y * 10_000 / height).min(9_999),
    ))
}

fn fraction_of(window: Hwnd, x: i32, y: i32) -> Option<(i32, i32)> {
    let rect = fit_for(window)?;

    fraction(rect, x, y)
}

/// The picture rectangle, computing it if the window has not painted yet.
fn fit_for(window: Hwnd) -> Option<(i32, i32, i32, i32)> {
    if let Some(rect) = *FIT.lock().unwrap() {
        return Some(rect);
    }

    let mut client = sys::Rect::default();
    get_client_rect(window, &mut client);

    let slot = FRAME.get()?.lock().unwrap();
    let frame = slot.as_ref()?;

    Some(fit(
        (frame.width, frame.height),
        (client.right - client.left, client.bottom - client.top),
    ))
}

fn over_picture() -> bool {
    let Some((x, y)) = *POINTER.lock().unwrap() else {
        return false;
    };

    let window = *WINDOW.lock().unwrap();

    let Some(window) = window else {
        return false;
    };

    match fit_for(window) {
        Some(rect) => fraction(rect, x, y).is_some(),
        None => false,
    }
}

fn blit(hdc: Hdc, frame: &Frame, rect: (i32, i32, i32, i32)) {
    let Some(api) = stretch_dib_bits() else {
        return;
    };

    let info = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: frame.width as i32,
            // Negative height: top down rows, which is the order the decoder produces. Without
            // the sign GDI reads the buffer bottom-up and the picture arrives upside down.
            height: -(frame.height as i32),
            planes: 1,
            bit_count: 32,
            compression: 0,
            ..Default::default()
        },
        ..Default::default()
    };

    if let Some(set_mode) = set_stretch_blt_mode() {
        unsafe { set_mode(hdc, HALFTONE) };
    }

    if let Some(set_brush) = set_brush_org() {
        let mut previous = sys::Point::default();
        unsafe { set_brush(hdc, 0, 0, &mut previous) };
    }

    let _painted = unsafe {
        api(
            hdc,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            0,
            0,
            frame.width as i32,
            frame.height as i32,
            frame.pixels.as_ptr(),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    };
}

fn draw_cursor(hdc: Hdc, rect: (i32, i32, i32, i32), frame_width: usize) {
    let Some(target) = TARGET.get() else {
        return;
    };

    let Some((shape, position)) = target.cursor() else {
        return;
    };

    if frame_width == 0 || rect.2 <= 0 {
        return;
    }

    // The pointer position is in frame pixels, and the picture may be scaled and offset.
    let scale = rect.2 as f64 / frame_width as f64;
    let x = rect.0 + (position.0 as f64 * scale) as i32 - (shape.hotspot_x as f64 * scale) as i32;
    let y = rect.1 + (position.1 as f64 * scale) as i32 - (shape.hotspot_y as f64 * scale) as i32;

    let width = (shape.width as f64 * scale).round().max(1.0) as i32;
    let height = (shape.height as f64 * scale).round().max(1.0) as i32;

    let mut cache = CURSOR_BITMAP.lock().unwrap();

    if cache.as_ref().map(|bitmap| bitmap.handle) != Some(shape.handle) {
        if let Some(previous) = cache.take() {
            destroy_bitmap(previous);
        }

        *cache = create_bitmap(hdc, &shape);
    }

    let Some(bitmap) = cache.as_ref() else {
        return;
    };

    let Some(blend) = alpha_blend() else {
        return;
    };

    let function = BlendFunction {
        operation: 0,
        flags: 0,
        constant_alpha: 255,
        alpha_format: 1,
    };

    unsafe {
        blend(
            hdc,
            x,
            y,
            width,
            height,
            bitmap.dc as Pointer,
            0,
            0,
            shape.width as i32,
            shape.height as i32,
            function,
        );
    }
}

/// Builds a bitmap GDI can alpha blend, which means premultiplied BGRA — what every compositor
/// expects and the one thing the browser overlay gets for free from `ImageData`.
fn create_bitmap(hdc: Hdc, shape: &Shape) -> Option<CursorBitmap> {
    let create = export::<CreateDibSectionFn>("gdi32.dll", b"CreateDIBSection\0")?;
    let create_dc = export::<CreateCompatibleDcFn>("gdi32.dll", b"CreateCompatibleDC\0")?;
    let select = export::<SelectObjectFn>("gdi32.dll", b"SelectObject\0")?;

    let info = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: shape.width as i32,
            height: -(shape.height as i32),
            planes: 1,
            bit_count: 32,
            compression: 0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut u8 = std::ptr::null_mut();
    let bitmap = unsafe {
        create(
            hdc,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        )
    };

    if bitmap.is_null() || bits.is_null() {
        return None;
    }

    let scale = unsafe { std::slice::from_raw_parts_mut(bits, shape.width * shape.height * 4) };

    for (index, pixel) in shape.rgba.chunks_exact(4).enumerate() {
        let alpha = pixel[3] as u32;
        let offset = index * 4;

        if offset + 3 >= scale.len() {
            break;
        }

        scale[offset] = (pixel[2] as u32 * alpha / 255) as u8;
        scale[offset + 1] = (pixel[1] as u32 * alpha / 255) as u8;
        scale[offset + 2] = (pixel[0] as u32 * alpha / 255) as u8;
        scale[offset + 3] = alpha as u8;
    }

    let dc = unsafe { create_dc(hdc) };

    if dc.is_null() {
        delete_object(bitmap);
        return None;
    }

    unsafe { select(dc, bitmap) };

    Some(CursorBitmap {
        handle: shape.handle,
        bitmap: bitmap as isize,
        dc: dc as isize,
    })
}

fn destroy_bitmap(bitmap: CursorBitmap) {
    delete_object(bitmap.bitmap as Pointer);
    delete_dc(bitmap.dc as Hdc);
}

// ---------------------------------------------------------------------------------------------
// Fullscreen
// ---------------------------------------------------------------------------------------------

fn toggle_fullscreen(window: Hwnd) {
    let fullscreen = *FULLSCREEN.lock().unwrap();

    set_fullscreen(window, !fullscreen);
}

fn set_fullscreen(window: Hwnd, on: bool) {
    let Some(set_long) = set_window_long() else {
        return;
    };
    let Some(set_pos) = set_window_pos() else {
        return;
    };

    *FULLSCREEN.lock().unwrap() = on;

    if on {
        let monitor = monitor_from_window(window, MONITOR_DEFAULTTONEAREST);

        if monitor.is_null() {
            return;
        }

        let mut info = MonitorInfo {
            size: std::mem::size_of::<MonitorInfo>() as u32,
            ..Default::default()
        };

        if get_monitor_info(monitor, &mut info) == 0 {
            return;
        }

        unsafe {
            set_long(window, GWL_STYLE, (WS_POPUP | WS_VISIBLE) as i32);
            set_pos(
                window,
                0,
                info.monitor.left,
                info.monitor.top,
                info.monitor.right - info.monitor.left,
                info.monitor.bottom - info.monitor.top,
                SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
            );
        }

        return;
    }

    unsafe {
        set_long(
            window,
            GWL_STYLE,
            (WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN) as i32,
        );
        set_pos(
            window,
            0,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Win32, reached by name
// ---------------------------------------------------------------------------------------------

type CreateWindowExFn = unsafe extern "system" fn(
    u32,
    *const u16,
    *const u16,
    u32,
    i32,
    i32,
    i32,
    i32,
    Hwnd,
    Pointer,
    Pointer,
    Pointer,
) -> Hwnd;
type CreateDibSectionFn = unsafe extern "system" fn(
    Hdc,
    *const BitmapInfo,
    u32,
    *mut *mut u8,
    Pointer,
    u32,
) -> Pointer;
type CreateCompatibleDcFn = unsafe extern "system" fn(Hdc) -> Hdc;
type SelectObjectFn = unsafe extern "system" fn(Hdc, Pointer) -> Pointer;
type StretchDibBitsFn = unsafe extern "system" fn(
    Hdc,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    *const u8,
    *const BitmapInfo,
    u32,
    u32,
) -> i32;
type AlphaBlendFn = unsafe extern "system" fn(
    Hdc,
    i32,
    i32,
    i32,
    i32,
    Hdc,
    i32,
    i32,
    i32,
    i32,
    BlendFunction,
) -> i32;

fn export<T: Copy>(module: &str, name: &[u8]) -> Option<T> {
    sys::export(module, name)
}

fn begin_and_end_paint() -> Option<(
    unsafe extern "system" fn(Hwnd, *mut PaintStruct) -> Hdc,
    unsafe extern "system" fn(Hwnd, *mut PaintStruct) -> i32,
)> {
    let begin = export::<unsafe extern "system" fn(Hwnd, *mut PaintStruct) -> Hdc>(
        "user32.dll",
        b"BeginPaint\0",
    )?;
    let end = export::<unsafe extern "system" fn(Hwnd, *mut PaintStruct) -> i32>(
        "user32.dll",
        b"EndPaint\0",
    )?;

    Some((begin, end))
}

fn get_client_rect(window: Hwnd, rect: &mut sys::Rect) {
    if let Some(api) = export::<unsafe extern "system" fn(Hwnd, *mut sys::Rect) -> i32>(
        "user32.dll",
        b"GetClientRect\0",
    ) {
        unsafe { api(window, rect) };
    }
}

fn def_window_proc(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize {
    match export::<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>(
        "user32.dll",
        b"DefWindowProcW\0",
    ) {
        Some(api) => unsafe { api(window, message, wparam, lparam) },
        None => 0,
    }
}

fn show_window(window: Hwnd, command: i32) {
    if let Some(api) =
        export::<unsafe extern "system" fn(Hwnd, i32) -> i32>("user32.dll", b"ShowWindow\0")
    {
        unsafe { api(window, command) };
    }
}

fn destroy_window(window: Hwnd) {
    if let Some(api) =
        export::<unsafe extern "system" fn(Hwnd) -> i32>("user32.dll", b"DestroyWindow\0")
    {
        unsafe { api(window) };
    }
}

fn invalidate_rect() -> Option<unsafe extern "system" fn(Hwnd, *const sys::Rect, i32) -> i32> {
    export("user32.dll", b"InvalidateRect\0")
}

fn set_window_text(window: Hwnd, text: &[u16]) -> Option<i32> {
    let api =
        export::<unsafe extern "system" fn(Hwnd, *const u16) -> i32>("user32.dll", b"SetWindowTextW\0")?;

    Some(unsafe { api(window, text.as_ptr()) })
}

fn get_message(message: &mut Msg) -> i32 {
    match export::<unsafe extern "system" fn(*mut Msg, Hwnd, u32, u32) -> i32>(
        "user32.dll",
        b"GetMessageW\0",
    ) {
        Some(api) => unsafe { api(message, 0, 0, 0) },
        None => 0,
    }
}

fn translate_message(message: &Msg) {
    if let Some(api) =
        export::<unsafe extern "system" fn(*const Msg) -> i32>("user32.dll", b"TranslateMessage\0")
    {
        unsafe { api(message) };
    }
}

fn dispatch_message(message: &Msg) {
    if let Some(api) =
        export::<unsafe extern "system" fn(*const Msg) -> isize>("user32.dll", b"DispatchMessageW\0")
    {
        unsafe { api(message) };
    }
}

fn post_quit() {
    if let Some(api) = export::<unsafe extern "system" fn(i32)>("user32.dll", b"PostQuitMessage\0") {
        unsafe { api(0) };
    }
}

fn load_cursor(name: usize) -> Option<Pointer> {
    let api =
        export::<unsafe extern "system" fn(Pointer, usize) -> Pointer>("user32.dll", b"LoadCursorW\0")?;

    Some(unsafe { api(std::ptr::null_mut(), name) })
}

fn set_cursor_api() -> Option<unsafe extern "system" fn(Pointer) -> Pointer> {
    export("user32.dll", b"SetCursor\0")
}

fn set_capture(window: Hwnd) -> Option<Hwnd> {
    let api = export::<unsafe extern "system" fn(Hwnd) -> Hwnd>("user32.dll", b"SetCapture\0")?;

    Some(unsafe { api(window) })
}

fn release_capture() {
    if let Some(api) = export::<unsafe extern "system" fn() -> i32>("user32.dll", b"ReleaseCapture\0")
    {
        unsafe { api() };
    }
}

fn pat_blt(hdc: Hdc, x: i32, y: i32, width: i32, height: i32) {
    if let Some(api) =
        export::<unsafe extern "system" fn(Hdc, i32, i32, i32, i32, u32) -> i32>("gdi32.dll", b"PatBlt\0")
    {
        unsafe { api(hdc, x, y, width, height, BLACKNESS) };
    }
}

fn stretch_dib_bits() -> Option<StretchDibBitsFn> {
    export("gdi32.dll", b"StretchDIBits\0")
}

fn set_stretch_blt_mode() -> Option<unsafe extern "system" fn(Hdc, i32) -> i32> {
    export("gdi32.dll", b"SetStretchBltMode\0")
}

fn set_brush_org() -> Option<unsafe extern "system" fn(Hdc, i32, i32, *mut sys::Point) -> i32> {
    export("gdi32.dll", b"SetBrushOrgEx\0")
}

fn alpha_blend() -> Option<AlphaBlendFn> {
    export("msimg32.dll", b"AlphaBlend\0")
}

fn delete_object(object: Pointer) {
    if let Some(api) =
        export::<unsafe extern "system" fn(Pointer) -> i32>("gdi32.dll", b"DeleteObject\0")
    {
        unsafe { api(object) };
    }
}

fn delete_dc(dc: Hdc) {
    if let Some(api) = export::<unsafe extern "system" fn(Hdc) -> i32>("gdi32.dll", b"DeleteDC\0") {
        unsafe { api(dc) };
    }
}

fn set_window_long() -> Option<unsafe extern "system" fn(Hwnd, i32, i32) -> i32> {
    export("user32.dll", b"SetWindowLongW\0")
}

fn set_window_pos() -> Option<unsafe extern "system" fn(Hwnd, Hwnd, i32, i32, i32, i32, u32) -> i32>
{
    export("user32.dll", b"SetWindowPos\0")
}

fn monitor_from_window(window: Hwnd, flags: u32) -> Pointer {
    match export::<unsafe extern "system" fn(Hwnd, u32) -> Pointer>("user32.dll", b"MonitorFromWindow\0")
    {
        Some(api) => unsafe { api(window, flags) },
        None => std::ptr::null_mut(),
    }
}

fn get_monitor_info(monitor: Pointer, info: &mut MonitorInfo) -> i32 {
    match export::<unsafe extern "system" fn(Pointer, *mut MonitorInfo) -> i32>(
        "user32.dll",
        b"GetMonitorInfoW\0",
    ) {
        Some(api) => unsafe { api(monitor, info) },
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{fit, fraction};

    #[test]
    fn a_tall_picture_gets_bars_on_the_sides() {
        // 2560x1600 into a square client area: the width is the limit.
        assert_eq!(fit((2560, 1600), (1000, 1000)), (0, 187, 1000, 625));
    }

    #[test]
    fn a_wide_client_area_gets_bars_above_and_below() {
        // The same picture into a thin strip: the height is the limit.
        assert_eq!(fit((2560, 1600), (2000, 500)), (600, 0, 800, 500));
    }

    #[test]
    fn a_fraction_is_only_defined_over_the_picture() {
        let rect = (0, 187, 1000, 625);

        assert_eq!(fraction(rect, 0, 187), Some((0, 0)));
        assert_eq!(fraction(rect, 999, 811), Some((9990, 9984)));
        // The letterbox bars belong to the window, not to the desktop.
        assert_eq!(fraction(rect, 500, 100), None);
        assert_eq!(fraction(rect, 1000, 500), None);
    }
}
