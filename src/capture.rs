//! Screen capture through the DXGI Desktop Duplication API.
//!
//! This is what makes the difference to the CPU: a full 2560x1600 desktop comes back in a few
//! milliseconds through a GPU copy, and an unchanged desktop costs nothing at all because the
//! operating system says so instead of us comparing pixels.

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::ptr;

use crate::sys::{self, Guid, HResult, Pointer};

/// One captured desktop frame, borrowed from the mapped staging texture.
///
/// Call [`Duplicator::release`] once you are done with it, or the next frame will fail.
pub struct Frame<'a> {
    /// BGRA pixels, `stride` bytes per row.
    pub pixels: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

#[derive(Debug)]
pub enum CaptureError {
    /// The desktop did not change: not a failure.
    NoChange,
    Failed(HResult),
    Message(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::NoChange => write!(f, "the desktop did not change"),
            CaptureError::Failed(hr) => write!(f, "COM call failed with 0x{hr:08X}"),
            CaptureError::Message(message) => write!(f, "{message}"),
        }
    }
}

impl Error for CaptureError {}

impl From<String> for CaptureError {
    fn from(message: String) -> Self {
        CaptureError::Message(message)
    }
}

impl From<&str> for CaptureError {
    fn from(message: &str) -> Self {
        CaptureError::Message(message.to_string())
    }
}

// Signatures of the COM methods this module calls, by vtable slot (see sys::method).
type CreateDeviceFn = unsafe extern "system" fn(
    adapter: Pointer,
    driver_type: u32,
    software: Pointer,
    flags: u32,
    feature_levels: *const u32,
    feature_levels_count: u32,
    sdk_version: u32,
    device: *mut Pointer,
    feature_level: *mut u32,
    context: *mut Pointer,
) -> HResult;
type GetAdapterFn = unsafe extern "system" fn(Pointer, *mut Pointer) -> HResult;
type EnumOutputsFn = unsafe extern "system" fn(Pointer, u32, *mut Pointer) -> HResult;
type OutputGetDescFn = unsafe extern "system" fn(Pointer, *mut sys::OutputDescription) -> HResult;
type DuplicateOutputFn = unsafe extern "system" fn(Pointer, Pointer, *mut Pointer) -> HResult;
type AcquireNextFrameFn =
    unsafe extern "system" fn(Pointer, u32, *mut u8, *mut Pointer) -> HResult;
type ReleaseFrameFn = unsafe extern "system" fn(Pointer) -> HResult;
type CreateTexture2dFn =
    unsafe extern "system" fn(Pointer, *const sys::Texture2dDescription, Pointer, *mut Pointer) -> HResult;
type CopyResourceFn = unsafe extern "system" fn(Pointer, Pointer, Pointer);
type MapFn =
    unsafe extern "system" fn(Pointer, Pointer, u32, u32, u32, *mut sys::MappedSubresource) -> HResult;
type UnmapFn = unsafe extern "system" fn(Pointer, Pointer, u32);
type TextureGetDescFn = unsafe extern "system" fn(Pointer, *mut sys::Texture2dDescription);

/// One monitor, as the control panel lists it.
#[derive(Clone, Debug)]
pub struct Output {
    pub index: u32,
    /// Position in the virtual desktop, which GDI capture needs to aim at this monitor.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

pub struct Duplicator {
    device: Pointer,
    context: Pointer,
    duplication: Pointer,
    /// Which output is being duplicated, for tools that take an index (ffmpeg's ddagrab).
    output_index: u32,
    staging: Pointer,
    staging_size: (u32, u32),
    width: u32,
    height: u32,
    is_frame_held: bool,
}

impl Duplicator {
    /// Starts duplicating the monitor that owns the origin of the virtual desktop, which is
    /// always the primary one.
    /// A monitor the user can pick, described the way the panel shows it.
    pub fn outputs() -> Vec<Output> {
        let Ok((device, context)) = (unsafe { Self::create_device() }) else {
            return Vec::new();
        };

        let mut list = Vec::new();

        unsafe {
            let Some(adapter) = Self::adapter_of(device) else {
                sys::release(context);
                sys::release(device);
                return Vec::new();
            };

            let enumerate: EnumOutputsFn = sys::method(adapter, 7);

            for index in 0.. {
                let mut output: Pointer = ptr::null_mut();

                if enumerate(adapter, index, &mut output) != sys::S_OK || output.is_null() {
                    break;
                }

                let get_desc: OutputGetDescFn = sys::method(output, 7);
                let mut description = sys::OutputDescription::default();

                if get_desc(output, &mut description) == sys::S_OK {
                    let rect = &description.desktop_coordinates;

                    list.push(Output {
                        index,
                        left: rect.left,
                        top: rect.top,
                        width: (rect.right - rect.left).max(0) as u32,
                        height: (rect.bottom - rect.top).max(0) as u32,
                        primary: rect.left <= 0 && rect.top <= 0 && rect.right > 0 && rect.bottom > 0,
                    });
                }

                sys::release(output);
            }

            sys::release(adapter);
            sys::release(context);
            sys::release(device);
        }

        list
    }

    /// The device every capture path creates: one hardware device with BGRA support.
    unsafe fn create_device() -> Result<(Pointer, Pointer), CaptureError> {
        let module = sys::load_library("d3d11.dll")
            .ok_or_else(|| "cannot load d3d11.dll".to_string())?;
        let address = sys::symbol(module, b"D3D11CreateDevice\0")
            .ok_or_else(|| "d3d11.dll has no D3D11CreateDevice".to_string())?;

        let create_device: CreateDeviceFn = std::mem::transmute(address);

        let mut device: Pointer = ptr::null_mut();
        let mut context: Pointer = ptr::null_mut();
        let mut feature_level: u32 = 0;

        let result = create_device(
            ptr::null_mut(),
            sys::D3D_DRIVER_TYPE_HARDWARE,
            ptr::null_mut(),
            sys::D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            ptr::null(),
            0,
            sys::D3D11_SDK_VERSION,
            &mut device,
            &mut feature_level,
            &mut context,
        );

        if result != sys::S_OK || device.is_null() || context.is_null() {
            return Err(CaptureError::Failed(result));
        }

        Ok((device, context))
    }

    unsafe fn adapter_of(device: Pointer) -> Option<Pointer> {
        let dxgi_device = sys::query_interface(device, &sys::IID_IDXGI_DEVICE)?;
        let get_adapter: GetAdapterFn = sys::method(dxgi_device, 7);
        let mut adapter: Pointer = ptr::null_mut();
        let result = get_adapter(dxgi_device, &mut adapter);
        sys::release(dxgi_device);

        if result == sys::S_OK && !adapter.is_null() {
            Some(adapter)
        } else {
            None
        }
    }

    pub fn new() -> Result<Self, CaptureError> {
        Self::new_at(None)
    }

    /// Captures a specific monitor. `None` means "whichever can be duplicated".
    pub fn new_at(wanted: Option<u32>) -> Result<Self, CaptureError> {
        let (device, context) = unsafe { Self::create_device() }?;
        let duplication = unsafe { Self::create_duplication(device, wanted) };

        match duplication {
            Ok((duplication, output_index)) => Ok(Self {
                device,
                context,
                duplication,
                output_index,
                staging: ptr::null_mut(),
                staging_size: (0, 0),
                width: 0,
                height: 0,
                is_frame_held: false,
            }),
            Err(error) => {
                unsafe {
                    sys::release(context);
                    sys::release(device);
                }

                Err(error)
            }
        }
    }

    unsafe fn create_duplication(
        device: Pointer,
        wanted: Option<u32>,
    ) -> Result<(Pointer, u32), CaptureError> {
        let adapter = Self::adapter_of(device)
            .ok_or_else(|| "the device does not report an adapter".to_string())?;

        // The monitor the user picked, if it can actually be duplicated.
        if let Some(wanted) = wanted {
            if let Some(duplication) = Self::duplicate_output(device, adapter, wanted) {
                sys::release(adapter);
                return Ok((duplication, wanted));
            }
        }

        // Otherwise the same scan as before: any output that works, the primary preferred.
        let enumerate: EnumOutputsFn = sys::method(adapter, 7);
        let mut fallback: Option<(Pointer, u32)> = None;

        for index in 0.. {
            let mut output: Pointer = ptr::null_mut();

            if enumerate(adapter, index, &mut output) != sys::S_OK || output.is_null() {
                break;
            }

            let primary = {
                let get_desc: OutputGetDescFn = sys::method(output, 7);
                let mut description = sys::OutputDescription::default();

                get_desc(output, &mut description) == sys::S_OK
                    && description.desktop_coordinates.left <= 0
                    && description.desktop_coordinates.top <= 0
                    && description.desktop_coordinates.right > 0
                    && description.desktop_coordinates.bottom > 0
            };

            sys::release(output);

            let Some(duplication) = Self::duplicate_output(device, adapter, index) else {
                continue;
            };

            if primary {
                if let Some((previous, _)) = fallback {
                    sys::release(previous);
                }

                sys::release(adapter);
                return Ok((duplication, index));
            }

            if fallback.is_none() {
                fallback = Some((duplication, index));
            } else {
                sys::release(duplication);
            }
        }

        sys::release(adapter);

        match fallback {
            Some((duplication, index)) => Ok((duplication, index)),
            None => Err(CaptureError::Message(
                "no output on this adapter can be duplicated".to_string(),
            )),
        }
    }

    /// One attempt: duplication on a single output, or nothing.
    unsafe fn duplicate_output(device: Pointer, adapter: Pointer, index: u32) -> Option<Pointer> {
        let enumerate: EnumOutputsFn = sys::method(adapter, 7);
        let mut output: Pointer = ptr::null_mut();

        if enumerate(adapter, index, &mut output) != sys::S_OK || output.is_null() {
            return None;
        }

        let output1 = sys::query_interface(output, &sys::IID_IDXGI_OUTPUT1);
        sys::release(output);

        let output1 = output1?;
        let duplicate: DuplicateOutputFn = sys::method(output1, 22);
        let mut duplication: Pointer = ptr::null_mut();
        let result = duplicate(output1, device, &mut duplication);
        sys::release(output1);

        if result == sys::S_OK && !duplication.is_null() {
            Some(duplication)
        } else {
            None
        }
    }

    pub fn output_index(&self) -> u32 {
        self.output_index
    }

    /// Waits up to `timeout_ms` for a new desktop frame.
    ///
    /// Returns [`CaptureError::NoChange`] when the desktop is unchanged, which lets the server
    /// skip encoding entirely while nothing moves.
    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Frame<'_>, CaptureError> {
        // DXGI_OUTDUPL_FRAME_INFO, only written by the system; 64 bytes is more than its 48.
        let mut info = [0u8; 64];
        let mut resource: Pointer = ptr::null_mut();

        let acquire: AcquireNextFrameFn = unsafe { sys::method(self.duplication, 8) };
        let result = unsafe { acquire(self.duplication, timeout_ms, info.as_mut_ptr(), &mut resource) };

        if result != sys::S_OK {
            if result == sys::DXGI_ERROR_WAIT_TIMEOUT {
                return Err(CaptureError::NoChange);
            }

            return Err(CaptureError::Failed(result));
        }

        self.is_frame_held = true;

        if let Err(error) = unsafe { self.copy_to_staging(resource) } {
            unsafe { sys::release(resource) };
            self.release();
            return Err(error);
        }

        unsafe { sys::release(resource) };

        let mut mapped = sys::MappedSubresource::default();
        let map: MapFn = unsafe { sys::method(self.context, 14) };
        let result = unsafe { map(self.context, self.staging, 0, sys::D3D11_MAP_READ, 0, &mut mapped) };

        if result != sys::S_OK {
            self.release();
            return Err(CaptureError::Failed(result));
        }

        let stride = mapped.row_pitch as usize;
        let length = stride * self.height as usize;
        let pixels = unsafe { std::slice::from_raw_parts(mapped.data as *const u8, length) };

        Ok(Frame {
            pixels,
            width: self.width as usize,
            height: self.height as usize,
            stride,
        })
    }

    /// Releases the mapped texture and the frame. Must be called after a successful `acquire`.
    pub fn release(&mut self) {
        if self.is_frame_held {
            if !self.staging.is_null() {
                let unmap: UnmapFn = unsafe { sys::method(self.context, 15) };
                unsafe { unmap(self.context, self.staging, 0) };
            }

            let release: ReleaseFrameFn = unsafe { sys::method(self.duplication, 14) };
            unsafe { release(self.duplication) };
            self.is_frame_held = false;
        }
    }

    unsafe fn copy_to_staging(&mut self, resource: Pointer) -> Result<(), CaptureError> {
        let texture = sys::query_interface(resource, &sys::IID_ID3D11_TEXTURE2D)
            .ok_or_else(|| "frame is not an ID3D11Texture2D".to_string())?;

        let get_desc: TextureGetDescFn = sys::method(texture, 10);
        let mut description = sys::Texture2dDescription::default();
        get_desc(texture, &mut description);

        self.width = description.width;
        self.height = description.height;

        let needs_texture = self.staging.is_null()
            || self.staging_size != (description.width, description.height);

        if needs_texture {
            if !self.staging.is_null() {
                sys::release(self.staging);
            }

            let staging_description = sys::Texture2dDescription {
                width: description.width,
                height: description.height,
                mip_levels: 1,
                array_size: 1,
                format: description.format,
                sample: sys::SampleDescription {
                    count: 1,
                    quality: 0,
                },
                usage: sys::D3D11_USAGE_STAGING,
                bind_flags: 0,
                cpu_access_flags: sys::D3D11_CPU_ACCESS_READ,
                misc_flags: 0,
            };

            let create: CreateTexture2dFn = sys::method(self.device, 5);
            let mut staging: Pointer = ptr::null_mut();
            let result =
                create(self.device, &staging_description, ptr::null_mut(), &mut staging);

            if result != sys::S_OK || staging.is_null() {
                sys::release(texture);
                return Err(CaptureError::Failed(result));
            }

            self.staging = staging;
            self.staging_size = (description.width, description.height);
        }

        let copy: CopyResourceFn = sys::method(self.context, 47);
        copy(self.context, self.staging, texture);
        sys::release(texture);

        Ok(())
    }
}

impl Duplicator {
    /// Asks DXGI for an interface, used by tests and by the cursor path later.
    #[allow(dead_code)]
    unsafe fn guid_of(pointer: Pointer, id: &Guid) -> Option<Pointer> {
        sys::query_interface(pointer, id)
    }
}

/// The index of the output that can actually be duplicated. The primary monitor is not always
/// the one that works, so this asks the same way the capture itself does.
pub fn working_output_index() -> Option<u32> {
    Duplicator::new().ok().map(|duplicator| duplicator.output_index())
}

impl Drop for Duplicator {
    fn drop(&mut self) {
        self.release();

        unsafe {
            if !self.staging.is_null() {
                sys::release(self.staging);
            }

            if !self.duplication.is_null() {
                sys::release(self.duplication);
            }

            if !self.context.is_null() {
                sys::release(self.context);
            }

            if !self.device.is_null() {
                sys::release(self.device);
            }
        }
    }
}

// The duplicator is only ever used from the thread that created it.
unsafe impl Send for Duplicator {}

/// Keeps the compiler honest about the pointer type used by the COM helpers.
#[allow(dead_code)]
fn assert_pointer_type(pointer: *mut c_void) -> Pointer {
    pointer
}
