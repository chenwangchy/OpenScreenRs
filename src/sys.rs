//! The little bit of Win32 and COM plumbing this program needs.
//!
//! Calling COM from Rust without the `windows` crate means two things: `LoadLibraryW` /
//! `GetProcAddress` to reach functions by name, and one helper that calls a method by its
//! **vtable slot**. Every interface below is annotated with the slot layout it was checked
//! against, because an off-by-one here is exactly the kind of bug that costs an afternoon.
//!
//! Slot layout recap (IUnknown is always 0..2, IDXGIObject adds 3..6):
//!
//! | interface                  | slot | method            |
//! |----------------------------|------|-------------------|
//! | IUnknown                   | 0/1/2| QueryInterface/AddRef/Release |
//! | IDXGIDevice                |    7 | GetAdapter        |
//! | IDXGIAdapter               |    7 | EnumOutputs       |
//! | IDXGIOutput                |    7 | GetDesc           |
//! | IDXGIOutput1               |   22 | DuplicateOutput   |
//! | IDXGIOutputDuplication     |    8 | AcquireNextFrame  |
//! | IDXGIOutputDuplication     |   14 | ReleaseFrame      |
//! | ID3D11Device               |    5 | CreateTexture2D   |
//! | ID3D11DeviceContext        |   14 | Map               |
//! | ID3D11DeviceContext        |   15 | Unmap             |
//! | ID3D11DeviceContext        |   47 | CopyResource      |
//! | ID3D11Texture2D            |   10 | GetDesc           |

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::OnceLock;

pub type Pointer = *mut c_void;
pub type HResult = i32;

pub const S_OK: HResult = 0;
/// Nothing changed on the desktop: not an error.
pub const DXGI_ERROR_WAIT_TIMEOUT: HResult = 0x887A_0027u32 as i32;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> Pointer;
    fn GetProcAddress(module: Pointer, name: *const u8) -> Pointer;
}

/// Encodes a name as the null terminated UTF-16 that the Win32 API expects.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Loads a DLL and returns its module handle, or None.
pub fn load_library(path: &str) -> Option<Pointer> {
    let name = wide(path);
    let module = unsafe { LoadLibraryW(name.as_ptr()) };

    if module.is_null() {
        None
    } else {
        Some(module)
    }
}

/// Looks up an exported function. The caller transmutes it to the right signature.
pub fn symbol(module: Pointer, name: &[u8]) -> Option<Pointer> {
    let address = unsafe { GetProcAddress(module, name.as_ptr()) };

    if address.is_null() {
        None
    } else {
        Some(address)
    }
}

/// A COM GUID.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl Guid {
    const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

pub const IID_IDXGI_DEVICE: Guid =
    Guid::new(0x54ec_77fa, 0x1377, 0x44e6, [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c]);
pub const IID_IDXGI_OUTPUT1: Guid =
    Guid::new(0x00cd_dea8, 0x939b, 0x4b83, [0xa3, 0x40, 0xa6, 0x85, 0x22, 0x66, 0x66, 0xcc]);
pub const IID_ID3D11_TEXTURE2D: Guid =
    Guid::new(0x6f15_aaf2, 0xd208, 0x4e89, [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c]);

pub type QueryInterfaceFn =
    unsafe extern "system" fn(Pointer, *const Guid, *mut Pointer) -> HResult;
pub type ReleaseFn = unsafe extern "system" fn(Pointer) -> u32;

/// Calls method `slot` of a COM object and returns it as the requested function type.
///
/// # Safety
/// `object` must be a valid COM pointer and the slot must be that method's index.
pub unsafe fn method<T: Copy>(object: Pointer, slot: usize) -> T {
    let vtable = *(object as *mut *const usize);
    let address = *vtable.add(slot);
    std::mem::transmute_copy::<usize, T>(&address)
}

/// Drops one reference to a COM object.
///
/// # Safety
/// `object` must be a valid COM pointer, and it must not be used afterwards.
pub unsafe fn release(object: Pointer) {
    if object.is_null() {
        return;
    }

    let release: ReleaseFn = method(object, 2);
    release(object);
}

/// Asks an object for another of its interfaces.
///
/// # Safety
/// `object` must be a valid COM pointer.
pub unsafe fn query_interface(object: Pointer, id: &Guid) -> Option<Pointer> {
    let query: QueryInterfaceFn = method(object, 0);
    let mut result: Pointer = std::ptr::null_mut();

    if query(object, id, &mut result) == S_OK && !result.is_null() {
        Some(result)
    } else {
        None
    }
}

/// A rectangle in screen coordinates, as COM passes it.
#[repr(C)]
#[derive(Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}


/// DXGI_OUTPUT_DESC, 96 bytes on x64.
#[repr(C)]
#[derive(Default)]
pub struct OutputDescription {
    pub device_name: [u16; 32],
    pub desktop_coordinates: Rect,
    pub attached_to_desktop: i32,
    pub rotation: u32,
    pub monitor: Pointer,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SampleDescription {
    pub count: u32,
    pub quality: u32,
}

/// D3D11_TEXTURE2D_DESC, 44 bytes.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Texture2dDescription {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub array_size: u32,
    pub format: u32,
    pub sample: SampleDescription,
    pub usage: u32,
    pub bind_flags: u32,
    pub cpu_access_flags: u32,
    pub misc_flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct MappedSubresource {
    pub data: Pointer,
    pub row_pitch: u32,
    pub depth_pitch: u32,
}

// D3D11 constants used by the capture path.
pub const D3D_DRIVER_TYPE_HARDWARE: u32 = 1;
pub const D3D11_CREATE_DEVICE_BGRA_SUPPORT: u32 = 0x20;
pub const D3D11_SDK_VERSION: u32 = 7;
pub const D3D11_USAGE_STAGING: u32 = 3;
pub const D3D11_CPU_ACCESS_READ: u32 = 0x20000;
pub const D3D11_MAP_READ: u32 = 1;

/// Asks the system for a 1 ms timer. Frame pacing sleeps a few milliseconds between frames,
/// and the default 15.6 ms tick would turn a 60 fps target into 50.
pub fn begin_period() {
    if let Some(module) = load_library("winmm.dll") {
        if let Some(address) = symbol(module, b"timeBeginPeriod\0") {
            let function: unsafe extern "system" fn(u32) -> u32 =
                unsafe { std::mem::transmute(address) };
            unsafe { function(1) };
        }
    }
}

pub fn end_period() {
    if let Some(module) = load_library("winmm.dll") {
        if let Some(address) = symbol(module, b"timeEndPeriod\0") {
            let function: unsafe extern "system" fn(u32) -> u32 =
                unsafe { std::mem::transmute(address) };
            unsafe { function(1) };
        }
    }
}
/// A point in screen coordinates.
#[repr(C)]
#[derive(Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

type GetCursorPosFn = unsafe extern "system" fn(*mut Point) -> i32;
type GetSystemMetricsFn = unsafe extern "system" fn(i32) -> i32;

/// Looks up a function by name and returns it as the requested signature.
pub(crate) fn export<T: Copy>(module: &str, name: &[u8]) -> Option<T> {
    let library = load_library(module)?;
    let address = symbol(library, name)?;

    Some(unsafe { std::mem::transmute_copy::<Pointer, T>(&address) })
}

/// The pointer position in physical pixels.
///
/// `GetPhysicalCursorPos` matters here: DXGI reports monitor rectangles in physical pixels, and
/// this program is DPI unaware, so `GetCursorPos` would come back in a scaled space and every
/// cursor would be off by the scaling factor on a high DPI screen.
pub fn cursor_position() -> Option<(i32, i32)> {
    static FUNCTION: OnceLock<Option<GetCursorPosFn>> = OnceLock::new();

    let function = match *FUNCTION.get_or_init(|| {
        export("user32.dll", b"GetPhysicalCursorPos\0")
            .or_else(|| export("user32.dll", b"GetCursorPos\0"))
    }) {
        Some(function) => function,
        None => return None,
    };

    let mut point = Point::default();

    if unsafe { function(&mut point) } == 0 {
        None
    } else {
        Some((point.x, point.y))
    }
}

// GetAdaptersAddresses structures, laid out exactly as iphlpapi declares them on 64 bit
// Windows. Only the leading fields are named: the system allocates whole structs, and nothing
// here writes to them.
#[repr(C)]
struct SocketAddress {
    sockaddr: *mut u8,
    length: i32,
}

#[repr(C)]
struct UnicastAddress {
    /// union of ULONGLONG Alignment and {Length, Flags}.
    alignment: u64,
    next: *mut UnicastAddress,
    address: SocketAddress,
}

#[repr(C)]
struct AdapterAddresses {
    /// union of ULONGLONG Alignment and {Length, IfIndex}.
    alignment: u64,
    next: *mut AdapterAddresses,
    adapter_name: *mut u8,
    first_unicast: *mut UnicastAddress,
    first_anycast: *mut u8,
    first_multicast: *mut u8,
    first_dns_server: *mut u8,
    dns_suffix: *mut u16,
    description: *mut u16,
    friendly_name: *mut u16,
    physical_address: [u8; 8],
    physical_address_length: u32,
    flags: u32,
    mtu: u32,
    if_type: u32,
    oper_status: u32,
}

type GetAdaptersAddressesFn =
    unsafe extern "system" fn(u32, u32, *mut u8, *mut AdapterAddresses, *mut u32) -> u32;

/// The addresses another device on the LAN can reach, best first.
///
/// Asking the routing table which local address is used gets this wrong on any machine running a
/// proxy in TUN mode: those install a virtual adapter on 198.18.0.0/15 that owns the default
/// route, so the answer is an address no phone can reach. Enumerating the adapters and filtering
/// out the virtual ones is the only way to find the real one.
pub fn lan_addresses() -> Vec<String> {
    const AF_INET: u32 = 2;
    const GAA_FLAG_SKIP_ANYCAST: u32 = 0x2;
    const GAA_FLAG_SKIP_MULTICAST: u32 = 0x4;
    const GAA_FLAG_SKIP_DNS_SERVER: u32 = 0x8;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    const IF_TYPE_ETHERNET: u32 = 6;
    const IF_TYPE_WIFI: u32 = 71;

    static FUNCTION: OnceLock<Option<GetAdaptersAddressesFn>> = OnceLock::new();

    let Some(function) = *FUNCTION.get_or_init(|| export("iphlpapi.dll", b"GetAdaptersAddresses\0"))
    else {
        return Vec::new();
    };

    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    let mut buffer: Vec<u8> = vec![0; size as usize];

    loop {
        let result = unsafe {
            function(
                AF_INET,
                flags,
                std::ptr::null_mut(),
                buffer.as_mut_ptr() as *mut AdapterAddresses,
                &mut size,
            )
        };

        if result == ERROR_BUFFER_OVERFLOW {
            buffer = vec![0; size as usize];
            continue;
        }

        if result != 0 {
            return Vec::new();
        }

        break;
    }

    // The routing table is still useful evidence, as long as the answer is a real adapter: a
    // proxy's TUN address is filtered out above, and what remains is the interface the machine
    // actually uses to reach the network.
    let default_route = default_route_address();

    let mut scored: Vec<(u32, String)> = Vec::new();
    let mut adapter = buffer.as_ptr() as *const AdapterAddresses;

    while !adapter.is_null() {
        let entry = unsafe { &*adapter };
        let virtual_adapter = looks_virtual(&unsafe { wide_text(entry.description) })
            || looks_virtual(&unsafe { wide_text(entry.friendly_name) });
        let wired = entry.if_type == IF_TYPE_ETHERNET || entry.if_type == IF_TYPE_WIFI;

        let mut unicast = entry.first_unicast;

        while !unicast.is_null() {
            let node = unsafe { &*unicast };
            unicast = node.next;

            let socket = &node.address;

            if socket.length < 8 || socket.sockaddr.is_null() {
                continue;
            }

            let raw = unsafe { std::slice::from_raw_parts(socket.sockaddr, 8) };

            // Only IPv4: sockaddr_in, family little endian, then four address bytes.
            if u16::from_ne_bytes([raw[0], raw[1]]) != AF_INET as u16 {
                continue;
            }

            let address = format!("{}.{}.{}.{}", raw[4], raw[5], raw[6], raw[7]);

            if let Some(mut score) = score_address(&address, wired, virtual_adapter) {
                if default_route.as_deref() == Some(address.as_str()) {
                    score += 50;
                }

                scored.push((score, address));
            }
        }

        adapter = entry.next;
    }

    scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    scored.into_iter().map(|(_, address)| address).collect()
}

/// Ranks an address by how likely it is to be the one a phone on the same network can reach.
fn score_address(address: &str, wired: bool, virtual_adapter: bool) -> Option<u32> {
    let parts: Vec<u32> = address.split('.').filter_map(|part| part.parse().ok()).collect();

    if parts.len() != 4 {
        return None;
    }

    let (first, second) = (parts[0], parts[1]);

    // Loopback and the address Windows invents when DHCP fails are never the answer.
    if first == 0 || first == 127 || (first == 169 && second == 254) {
        return None;
    }

    // RFC 2544 space, which is what a proxy's TUN adapter puts in the routing table.
    if first == 198 && (second == 18 || second == 19) {
        return None;
    }

    let mut score: u32 = match (first, second) {
        (192, 168) => 30,
        (10, _) => 25,
        (172, 16..=31) => 20,
        // CGNAT is Tailscale or a carrier gateway: reachable for some users, so keep it last
        // instead of dropping it.
        (100, 64..=127) => 5,
        _ => 10,
    };

    if wired {
        score += 5;
    }

    // 192.168.137.0/24 is what Windows itself hands to an Internet Connection Sharing or mobile
    // hotspot adapter: reachable from the phones on the hotspot, not from the wired network.
    if (first, second) == (192, 168) && parts[2] == 137 {
        score = score.saturating_sub(35);
    }

    if virtual_adapter {
        // Names of adapters that exist only on this machine: their addresses go nowhere.
        score = score.saturating_sub(40);
    }

    if score == 0 {
        return None;
    }

    Some(score)
}

/// The address the routing table would use to reach the internet. No packets are sent: a
/// connected UDP socket only asks which local address the route implies.
fn default_route_address() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn looks_virtual(text: &str) -> bool {
    let lowered = text.to_lowercase();

    [
        "loopback",
        "wintun",
        "wireguard",
        "tailscale",
        "zerotier",
        "clash",
        "mihomo",
        "sing-box",
        "virtualbox",
        "vmware",
        "hyper-v",
        "vethernet",
        "tap-",
        "tun",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

unsafe fn wide_text(pointer: *const u16) -> String {
    if pointer.is_null() {
        return String::new();
    }

    let mut length = 0;
    while *pointer.add(length) != 0 {
        length += 1;
        if length > 256 {
            break;
        }
    }

    String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length))
}

/// One `GetSystemMetrics` value.
fn metric(index: i32) -> i32 {
    static FUNCTION: OnceLock<Option<GetSystemMetricsFn>> = OnceLock::new();

    let Some(function) = *FUNCTION.get_or_init(|| export("user32.dll", b"GetSystemMetrics\0")) else {
        return 0;
    };

    unsafe { function(index) }
}

/// The primary screen size, in physical pixels once [`become_dpi_aware`] has run.
///
/// Without DPI awareness Windows reports a 2560x1600 desktop at 125% as 2048x1280, and that is
/// where the frame height was silently capped. The pointer position and the DXGI monitor rectangles
/// always came back in physical pixels, so this was the one that disagreed with them.
pub fn screen_size() -> (i32, i32) {
    // SM_CXSCREEN = 0, SM_CYSCREEN = 1
    (metric(0), metric(1))
}

/// The whole virtual desktop as `SendInput` measures absolute positions: left, top, width, height.
///
/// Asked for from the same place `SendInput` asks, rather than assembled from the monitor
/// rectangles. That is not the same thing: enumeration happens per adapter, so a machine with the
/// integrated GPU driving one screen and the discrete one driving the other reports a bounding box
/// Windows does not use, and every injected position lands at the wrong fraction of the desktop.
pub fn virtual_screen() -> (i32, i32, i32, i32) {
    // SM_XVIRTUALSCREEN = 76, SM_YVIRTUALSCREEN = 77,
    // SM_CXVIRTUALSCREEN = 78, SM_CYVIRTUALSCREEN = 79
    (metric(76), metric(77), metric(78), metric(79))
}

/// Makes this process DPI aware, so that the desktop is described in real pixels.
///
/// Without it every coordinate this program reads is scaled: Windows reports the 2560x1600 primary
/// monitor as 2048x1280, and `SendInput` interprets absolute positions in that same virtualised
/// space while DXGI reports monitor rectangles in physical pixels. Mixing the two puts the injected
/// pointer at 80% of where it should be, which is exactly the kind of bug that only shows up on
/// somebody else's high DPI laptop.
///
/// Called once, before anything reads a size.
pub fn become_dpi_aware() {
    type SetContextFn = unsafe extern "system" fn(Pointer) -> i32;
    type SetAwarenessFn = unsafe extern "system" fn(i32) -> i32;
    type SetAwareFn = unsafe extern "system" fn() -> i32;

    // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, Windows 10 1703 and later. Per monitor rather
    // than per system, because a second screen of a different scale would otherwise be distorted.
    if let Some(function) = export::<SetContextFn>("user32.dll", b"SetProcessDpiAwarenessContext\0") {
        if unsafe { function((-4isize) as usize as Pointer) } != 0 {
            return;
        }
    }

    if let Some(function) = export::<SetAwarenessFn>("shcore.dll", b"SetProcessDpiAwareness\0") {
        // PROCESS_PER_MONITOR_DPI_AWARE = 2
        if unsafe { function(2) } == 0 {
            return;
        }
    }

    // What is left on Windows 7 and on processes that already had awareness set from a manifest.
    if let Some(function) = export::<SetAwareFn>("user32.dll", b"SetProcessDPIAware\0") {
        unsafe { function() };
    }
}