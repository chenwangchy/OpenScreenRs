//! JPEG encoding through libjpeg-turbo.
//!
//! The library is loaded at runtime, so building this crate needs no C compiler and no build
//! script: put `turbojpeg.dll` next to the executable and it is used. For a 1728x1080 frame it
//! encodes in about 3 ms, against roughly 8 ms for the GDI+ encoder that the C# version used
//! and 25 ms for the JDK one.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr;

use crate::sys::{self, Pointer};

/// TJPF_BGRA from turbojpeg.h.
const PIXEL_FORMAT_BGRA: i32 = 8;
/// TJSAMP_444: colour at full resolution.
const SUBSAMPLING_444: i32 = 0;
/// TJSAMP_420: half resolution colour, the tradeoff a browser JPEG makes.
const SUBSAMPLING_420: i32 = 2;
const NO_FLAGS: i32 = 0;

/// How much colour detail a frame keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Subsampling {
    /// Full colour resolution. Screen content is mostly coloured text on coloured backgrounds,
    /// and halving the colour resolution is what makes it look smeared.
    Full,
    /// Half colour resolution: smaller frames, softer coloured edges.
    Half,
}

impl Subsampling {
    fn tj_value(self) -> i32 {
        match self {
            Subsampling::Full => SUBSAMPLING_444,
            Subsampling::Half => SUBSAMPLING_420,
        }
    }
}

type TjInitCompress = unsafe extern "C" fn() -> Pointer;
type TjCompress2 = unsafe extern "C" fn(
    handle: Pointer,
    source: *const u8,
    width: i32,
    pitch: i32,
    height: i32,
    pixel_format: i32,
    jpeg_buffer: *mut *mut u8,
    jpeg_size: *mut u32,
    subsampling: i32,
    quality: i32,
    flags: i32,
) -> i32;
type TjFree = unsafe extern "C" fn(*mut u8);
type TjDestroy = unsafe extern "C" fn(Pointer) -> i32;

pub struct Encoder {
    handle: Pointer,
    compress: TjCompress2,
    free: TjFree,
    destroy: TjDestroy,
}

impl Encoder {
    /// Loads turbojpeg.dll from next to the executable, from `x64/`, or from the path in the
    /// `OPENSCREEN_TURBOJPEG` environment variable.
    pub fn new() -> Result<Self, String> {
        let path = load_path()?;
        let module = sys::load_library(&path.to_string_lossy())
            .ok_or_else(|| format!("cannot load {}", path.display()))?;

        unsafe {
            let init = function::<TjInitCompress>(module, b"tjInitCompress\0")?;
            let compress = function::<TjCompress2>(module, b"tjCompress2\0")?;
            let free = function::<TjFree>(module, b"tjFree\0")?;
            let destroy = function::<TjDestroy>(module, b"tjDestroy\0")?;

            let handle = init();

            if handle.is_null() {
                return Err("tjInitCompress returned nothing".to_string());
            }

            Ok(Self {
                handle,
                compress,
                free,
                destroy,
            })
        }
    }

    /// Encodes one BGRA frame. `stride` is the number of bytes per source row.
    pub fn encode(
        &self,
        pixels: &[u8],
        width: usize,
        height: usize,
        stride: usize,
        quality: i32,
        subsampling: Subsampling,
    ) -> Result<Vec<u8>, String> {
        let mut jpeg_buffer: *mut u8 = ptr::null_mut();
        let mut jpeg_size: u32 = 0;

        let result = unsafe {
            (self.compress)(
                self.handle,
                pixels.as_ptr(),
                width as i32,
                stride as i32,
                height as i32,
                PIXEL_FORMAT_BGRA,
                &mut jpeg_buffer,
                &mut jpeg_size,
                subsampling.tj_value(),
                quality,
                NO_FLAGS,
            )
        };

        if result != 0 || jpeg_buffer.is_null() || jpeg_size == 0 {
            return Err(format!("tjCompress2 failed with {result}"));
        }

        let jpeg = unsafe { std::slice::from_raw_parts(jpeg_buffer, jpeg_size as usize).to_vec() };
        unsafe { (self.free)(jpeg_buffer) };

        Ok(jpeg)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }

        unsafe { (self.destroy)(self.handle) };
    }
}

/// Reads a function pointer out of a loaded module.
unsafe fn function<T: Copy>(module: Pointer, name: &[u8]) -> Result<T, String> {
    let address = sys::symbol(module, name)
        .ok_or_else(|| format!("turbojpeg.dll has no {}", String::from_utf8_lossy(name)))?;

    Ok(std::mem::transmute_copy::<Pointer, T>(&address))
}

fn load_path() -> Result<PathBuf, String> {
    if let Ok(configured) = std::env::var("OPENSCREEN_TURBOJPEG") {
        let path = PathBuf::from(configured);

        if path.is_file() {
            return Ok(path);
        }
    }

    let mut candidates = Vec::new();

    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join("turbojpeg.dll"));
            candidates.push(directory.join("x64").join("turbojpeg.dll"));
        }
    }

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    Err(format!(
        "turbojpeg.dll not found, looked in: {}",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Silences the unused import warning for `c_void` in this module on some targets.
#[allow(dead_code)]
type VoidPointer = *mut c_void;
