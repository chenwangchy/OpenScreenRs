//! The streaming server.
//!
//! Two modes, picked on the command line:
//!
//! * **MJPEG** (default): our own capture and encode, one JPEG per frame, no dependencies.
//! * **H.264** (`--h264`): ffmpeg captures and encodes, the browser decodes in hardware through
//!   WebCodecs. About twenty times less bandwidth for the same picture.
//!
//! Paths, in both modes:
//!
//! * `/`         the player page for the active mode
//! * `/stream`   MJPEG; any other path is treated the same way
//! * `/h264/ws`  H.264 access units as binary WebSocket messages
//! * `/ws`       cursor positions as text WebSocket messages
//!
//! One producer thread feeds every viewer, and a viewer only ever receives the newest frame, so
//! a slow client skips instead of pushing the others behind.

use std::collections::VecDeque;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::capture::{CaptureError, Duplicator};
use crate::fmp4::{self, Chunk};
use crate::h264::AccessUnit;
use crate::jpeg::Encoder;
use crate::{
    cursor, ffmpeg, h264, h264_page, input, mjpeg, mse_page, page, panel_page, player_js, scaler,
    sys, webrtc_page, webrtc_stream, ws,
};

const MIN_QUALITY: i32 = 50;
/// How often the pointer position is sampled.
const CURSOR_POLL: Duration = Duration::from_millis(10);
/// Longest write we tolerate to one viewer before dropping it, in milliseconds. A fragment can be
/// a few hundred kilobytes and a browser can be briefly busy appending one, so this is generous:
/// only a viewer that has actually gone away trips it.
const WRITE_TIMEOUT_MS: u64 = 5000;
/// How many access units are kept for viewers that join mid stream. One second at 60 fps.
const RECENT_ACCESS_UNITS: usize = 120;
/// How many fragments are replayed to a viewer that joins: about a second.
const REPLAY_FRAGMENTS: usize = 30;
/// Sent right before a fragment a viewer may start decoding at.
const KEYFRAME_MARKER: &str = "{\"keyframe\":true}";

#[derive(Clone)]
pub enum Mode {
    Mjpeg { subsampling: crate::jpeg::Subsampling },
    /// H.264 delivered as access units to WebCodecs. Lowest latency, but the browser only
    /// exposes WebCodecs on a secure context, so this works on localhost or https, not on a
    /// LAN address over plain http.
    H264 { bitrate_kbps: u32, ffmpeg: PathBuf },
    /// H.264 delivered as fragmented MP4 to MediaSource. A few hundred milliseconds slower,
    /// but it plays on any origin, which is what makes it usable over the LAN.
    Mse { bitrate_kbps: u32, gop: u32, fragment_ms: u32, ffmpeg: PathBuf },
    /// H.264 over WebRTC: RTP on UDP, no fragment quantisation, no buffering layer. The lowest
    /// latency this program can reach.
    WebRtc { bitrate_kbps: u32, profile: ffmpeg::Profile, ffmpeg: PathBuf },
}

/// Which page `/` serves, and therefore which player the viewer ends up with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mjpeg,
    WebCodecs,
    MediaSource,
    WebRtc,
}

/// Everything the panel can change. A change means restarting the encoder, which is what the
/// generation counter is for: the running one notices and returns, the supervisor starts the next.
#[derive(Clone)]
pub struct Settings {
    pub height: usize,
    pub fps: u32,
    pub quality: i32,
    /// Where ffmpeg lives. Kept here rather than inside the mode, because the panel can switch
    /// between a mode that needs it and one that does not.
    pub ffmpeg: PathBuf,
    /// Which monitor to capture. None lets the capture path pick one it can duplicate.
    pub screen: Option<u32>,
    pub mode: Mode,
}

/// The ffmpeg shipped next to the executable, or the one on PATH.
fn default_ffmpeg_path() -> PathBuf {
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            let beside = directory.join("ffmpeg.exe");

            if beside.exists() {
                return beside;
            }
        }
    }

    PathBuf::from("ffmpeg")
}

impl Mode {

    fn profile(&self) -> ffmpeg::Profile {
        match self {
            Mode::WebRtc { profile, .. } => *profile,
            _ => ffmpeg::Profile::Baseline,
        }
    }

    fn bitrate_kbps(&self) -> u32 {
        match self {
            Mode::Mjpeg { .. } => 0,
            Mode::H264 { bitrate_kbps, .. }
            | Mode::Mse { bitrate_kbps, .. }
            | Mode::WebRtc { bitrate_kbps, .. } => *bitrate_kbps,
        }
    }

    fn subsampling(&self) -> crate::jpeg::Subsampling {
        match self {
            Mode::Mjpeg { subsampling } => *subsampling,
            _ => crate::jpeg::Subsampling::Full,
        }
    }
}

impl Mode {
    fn kind(&self) -> Kind {
        match self {
            Mode::Mjpeg { .. } => Kind::Mjpeg,
            Mode::H264 { .. } => Kind::WebCodecs,
            Mode::Mse { .. } => Kind::MediaSource,
            Mode::WebRtc { .. } => Kind::WebRtc,
        }
    }
}

struct CursorClient {
    id: u64,
    stream: TcpStream,
}

struct H264Client {
    id: u64,
    stream: TcpStream,
}

struct H264State {
    /// JSON that tells the page how to configure its decoder.
    config: String,
    clients: Vec<H264Client>,
    next_client: u64,
    /// Access units since the last key frame, so a viewer that joins sees a picture at once.
    recent: VecDeque<Arc<AccessUnit>>,
}

impl H264State {
    fn new(config: String) -> Self {
        Self {
            config,
            clients: Vec::new(),
            next_client: 1,
            recent: VecDeque::with_capacity(RECENT_ACCESS_UNITS),
        }
    }
}

struct Shared {
    frame: Option<Arc<Vec<u8>>>,
    version: u64,
    viewers: usize,
    /// Number of viewers that could not hand a frame to the network in time since last check.
    slow: usize,
    closed: bool,
    /// Size of the frames being produced, which is how cursor positions are scaled.
    frame_size: (usize, usize),
    cursor: (i32, i32),
    /// True while the pointer is off the shared monitor, so nothing should be drawn.
    cursor_hidden: bool,
    cursor_clients: Vec<CursorClient>,
    next_cursor_client: u64,
    /// Description of the pointer shape and its pixels, kept so a late viewer still gets it.
    cursor_meta: Option<String>,
    cursor_pixels: Option<Arc<Vec<u8>>>,
    h264: Option<H264State>,
    mse: Option<MseState>,
    /// The WebRTC transport, when that mode is active.
    hub: Option<Arc<webrtc_stream::Hub>>,
}

struct MseState {
    /// JSON that tells the page which MIME type to configure.
    config: String,
    clients: Vec<H264Client>,
    next_client: u64,
    /// The track description, needed before the first fragment.
    init: Option<Arc<Vec<u8>>>,
    /// Self contained fragments, so a viewer that joins mid stream sees a picture at once.
    recent: VecDeque<Arc<Vec<u8>>>,
}

impl MseState {
    fn new(config: String) -> Self {
        Self {
            config,
            clients: Vec::new(),
            next_client: 1,
            init: None,
            recent: VecDeque::with_capacity(8),
        }
    }
}

struct State {
    shared: Mutex<Shared>,
    frame_ready: Condvar,
    /// What the control panel last asked for.
    settings: Mutex<Settings>,
    /// Bumped on every change, so a running encoder knows it has been superseded.
    generation: AtomicU64,
    /// Which monitor is actually being captured, which is not always the requested one.
    capturing: Mutex<Option<u32>>,
    /// Whether a viewer may move this machine's pointer and type on it. Off until the panel says
    /// otherwise: sharing a picture is passive, driving somebody's keyboard is not.
    control: AtomicBool,
    /// Who is driving, for the panel and the console.
    controller: Mutex<Option<String>>,
    /// The keys and buttons a viewer holds down, so they can be released when it leaves.
    input: Mutex<input::Session>,
}

impl State {
    fn new(settings: Settings) -> Self {
        Self {
            settings: Mutex::new(settings),
            generation: AtomicU64::new(1),
            capturing: Mutex::new(None),
            control: AtomicBool::new(false),
            controller: Mutex::new(None),
            input: Mutex::new(input::Session::new()),
            shared: Mutex::new(Shared {
                frame: None,
                version: 0,
                viewers: 0,
                slow: 0,
                closed: false,
                frame_size: (0, 0),
                cursor: (-1, -1),
                cursor_hidden: false,
                cursor_clients: Vec::new(),
                next_cursor_client: 1,
                cursor_meta: None,
                cursor_pixels: None,
                h264: None,
                mse: None,
                hub: None,
            }),
            frame_ready: Condvar::new(),
        }
    }

    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    fn set_settings(&self, settings: Settings) {
        *self.settings.lock().unwrap() = settings;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn set_capturing(&self, index: Option<u32>) {
        *self.capturing.lock().unwrap() = index;
    }

    fn capturing(&self) -> Option<u32> {
        *self.capturing.lock().unwrap()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn kind(&self) -> Kind {
        self.settings().mode.kind()
    }

    fn control(&self) -> bool {
        self.control.load(Ordering::SeqCst)
    }

    fn controller(&self) -> Option<String> {
        self.controller.lock().unwrap().clone()
    }

    /// The JSON every page uses to decide whether its clicks count.
    fn control_message(&self) -> String {
        let controller = match self.controller() {
            Some(who) => format!("\"{}\"", who.replace('"', "")),
            None => "null".to_string(),
        };

        format!(
            "{{\"control\":{},\"controller\":{controller}}}",
            self.control()
        )
    }

    /// Turns remote control on or off. Turning it off also lets go of anything the viewer is
    /// holding down, which is the difference between a finished session and a stuck Ctrl key.
    fn set_control(&self, on: bool) {
        if !on {
            self.input.lock().unwrap().release();
            *self.controller.lock().unwrap() = None;
        }

        self.control.store(on, Ordering::SeqCst);

        println!(
            "remote control: {}",
            if on {
                "on — a viewer can use the pointer and the keyboard"
            } else {
                "off"
            }
        );

        self.announce_control();
    }

    /// Records who is driving and tells the pages. Refuses once control has been turned off.
    fn claim_control(&self, who: &str) -> bool {
        if !self.control() {
            return false;
        }

        let mut controller = self.controller.lock().unwrap();

        if controller.as_deref() == Some(who) {
            return true;
        }

        println!("remote control: {who} is using this machine");
        *controller = Some(who.to_string());
        drop(controller);

        self.announce_control();
        true
    }

    /// Forgets a viewer that went away, releasing whatever it left held down.
    fn release_control(&self, who: &str) {
        let mut controller = self.controller.lock().unwrap();

        if controller.as_deref() != Some(who) {
            return;
        }

        *controller = None;
        drop(controller);

        self.input.lock().unwrap().release();
        println!("remote control: {who} stopped");

        self.announce_control();
    }

    /// Applies one input message from `who`, if that viewer is the one allowed to drive.
    fn inject(&self, who: &str, message: &str) {
        if !self.claim_control(who) {
            return;
        }

        // Not every message the page sends is understood, so an unknown one is simply dropped.
        let shared = self.capturing();
        self.input.lock().unwrap().apply(message, shared);
    }

    /// Tells every page whether it may send input.
    fn announce_control(&self) {
        let message = self.control_message();

        let mut shared = self.shared.lock().unwrap();
        shared
            .cursor_clients
            .retain_mut(|client| ws::send_text(&mut client.stream, message.as_bytes()).is_ok());
    }

    fn publish(&self, jpeg: Vec<u8>) {
        let mut shared = self.shared.lock().unwrap();
        shared.frame = Some(Arc::new(jpeg));
        shared.version += 1;
        drop(shared);
        self.frame_ready.notify_all();
    }

    fn set_frame_size(&self, width: usize, height: usize) {
        self.shared.lock().unwrap().frame_size = (width, height);
    }

    fn frame_size(&self) -> (usize, usize) {
        self.shared.lock().unwrap().frame_size
    }

    /// Blocks until a frame newer than `sent` exists.
    fn wait_for_frame(&self, sent: u64) -> Option<(Arc<Vec<u8>>, u64)> {
        let mut shared = self.shared.lock().unwrap();

        loop {
            if shared.closed {
                return None;
            }

            if let Some(frame) = &shared.frame {
                if shared.version != sent {
                    return Some((frame.clone(), shared.version));
                }
            }

            shared = self
                .frame_ready
                .wait_timeout(shared, Duration::from_millis(500))
                .unwrap()
                .0;
        }
    }

    fn count_viewer(&self, delta: isize) {
        let mut shared = self.shared.lock().unwrap();
        shared.viewers = shared.viewers.saturating_add_signed(delta);
    }

    fn viewers(&self) -> usize {
        self.shared.lock().unwrap().viewers
    }

    fn mark_slow(&self) {
        self.shared.lock().unwrap().slow += 1;
    }

    /// Lowers the quality when a viewer fell behind, wins it back slowly when it recovers.
    fn adapt(&self, quality: i32, ceiling: i32) -> i32 {
        let mut shared = self.shared.lock().unwrap();

        if shared.viewers == 0 {
            return quality;
        }

        let slow = std::mem::take(&mut shared.slow);

        if slow > 0 {
            (quality - 4).max(MIN_QUALITY)
        } else {
            // Recover quickly: a viewer that keeps up should not stay degraded for a minute.
            (quality + 2).min(ceiling)
        }
    }

    /// Stores the pointer position and pushes it to every page.
    /// Tells the viewers there is no pointer to draw, which is true while the pointer sits on a
    /// monitor that is not the one being shared.
    fn hide_cursor(&self) {
        let mut shared = self.shared.lock().unwrap();

        if shared.cursor_hidden {
            return;
        }

        shared.cursor_hidden = true;

        shared
            .cursor_clients
            .retain_mut(|client| ws::send_text(&mut client.stream, b"{\"cursor\":null}").is_ok());
    }

    fn set_cursor(&self, x: i32, y: i32) {
        let mut shared = self.shared.lock().unwrap();
        shared.cursor_hidden = false;
        shared.cursor = (x, y);

        if shared.cursor_clients.is_empty() {
            return;
        }

        let message = format!("{{\"cursor\":[{x},{y}]}}");
        shared
            .cursor_clients
            .retain_mut(|client| ws::send_text(&mut client.stream, message.as_bytes()).is_ok());
    }
    fn set_cursor_shape(&self, shape: &cursor::Shape) {
        let meta = format!(
            "{{\"cursorImage\":{{\"width\":{},\"height\":{},\"hotspotX\":{},\"hotspotY\":{}}}}}",
            shape.width, shape.height, shape.hotspot_x, shape.hotspot_y
        );
        let pixels = Arc::new(shape.rgba.clone());

        let mut shared = self.shared.lock().unwrap();
        shared.cursor_meta = Some(meta.clone());
        shared.cursor_pixels = Some(Arc::clone(&pixels));

        shared.cursor_clients.retain_mut(|client| {
            ws::send_text(&mut client.stream, meta.as_bytes())
                .and_then(|_| ws::send_binary(&mut client.stream, &pixels))
                .is_ok()
        });
    }

    /// Registers a page, telling it straight away whether it may send input and where the pointer is.
    fn register_cursor_client(&self, mut stream: TcpStream) -> std::io::Result<u64> {
        // Built before the lock: it reads another lock, and taking two locks in a different order
        // in two places is how a deadlock gets in.
        let control = self.control_message();
        ws::send_text(&mut stream, control.as_bytes())?;

        let mut shared = self.shared.lock().unwrap();
        let id = shared.next_cursor_client;
        shared.next_cursor_client += 1;

        let (x, y) = shared.cursor;

        if x >= 0 {
            let message = format!("{{\"cursor\":[{x},{y}]}}");
            ws::send_text(&mut stream, message.as_bytes())?;
        }

        if let (Some(meta), Some(pixels)) = (&shared.cursor_meta, &shared.cursor_pixels) {
            ws::send_text(&mut stream, meta.as_bytes())?;
            ws::send_binary(&mut stream, pixels)?;
        }

        shared.cursor_clients.push(CursorClient { id, stream });
        Ok(id)
    }

    fn unregister_cursor_client(&self, id: u64) {
        let mut shared = self.shared.lock().unwrap();
        shared.cursor_clients.retain(|client| client.id != id);
    }

    fn set_h264_config(&self, config: String) {
        self.shared.lock().unwrap().h264 = Some(H264State::new(config));
    }

    fn h264_clients(&self) -> usize {
        self.shared
            .lock()
            .unwrap()
            .h264
            .as_ref()
            .map_or(0, |h264| h264.clients.len())
    }

    /// Drops the replay buffer, which belongs to the encoder that is going away.
    fn reset_h264_recent(&self) {
        let mut shared = self.shared.lock().unwrap();

        if let Some(h264) = shared.h264.as_mut() {
            h264.recent.clear();
        }
    }

    /// Pushes one access unit to every H.264 viewer and remembers it for later joiners.
    fn publish_access_unit(&self, unit: Arc<AccessUnit>) {
        let mut shared = self.shared.lock().unwrap();
        let Some(h264) = shared.h264.as_mut() else {
            return;
        };

        if unit.is_keyframe {
            h264.recent.clear();
        }

        h264.recent.push_back(Arc::clone(&unit));

        while h264.recent.len() > RECENT_ACCESS_UNITS {
            h264.recent.pop_front();
        }

        if h264.clients.is_empty() {
            return;
        }

        h264.clients
            .retain_mut(|client| ws::send_binary(&mut client.stream, &unit.data).is_ok());
    }

    /// Registers an H.264 viewer, replaying from the last key frame so it sees a picture
    /// immediately instead of waiting for the next one.
    fn register_h264_client(&self, mut stream: TcpStream) -> std::io::Result<u64> {
        let mut shared = self.shared.lock().unwrap();
        let h264 = shared
            .h264
            .as_mut()
            .ok_or_else(|| std::io::Error::other("H.264 mode is not active"))?;

        ws::send_text(&mut stream, h264.config.as_bytes())?;

        for unit in &h264.recent {
            ws::send_binary(&mut stream, &unit.data)?;
        }

        let id = h264.next_client;
        h264.next_client += 1;
        h264.clients.push(H264Client { id, stream });

        Ok(id)
    }

    fn unregister_h264_client(&self, id: u64) {
        let mut shared = self.shared.lock().unwrap();

        if let Some(h264) = shared.h264.as_mut() {
            h264.clients.retain(|client| client.id != id);
        }
    }

    fn set_hub(&self, hub: Arc<webrtc_stream::Hub>) {
        self.shared.lock().unwrap().hub = Some(hub);
    }

    fn hub(&self) -> Option<Arc<webrtc_stream::Hub>> {
        self.shared.lock().unwrap().hub.clone()
    }

    fn set_mse_config(&self, config: String) {
        self.shared.lock().unwrap().mse = Some(MseState::new(config));
    }

    fn mse_clients(&self) -> usize {
        self.shared
            .lock()
            .unwrap()
            .mse
            .as_ref()
            .map_or(0, |mse| mse.clients.len())
    }

    /// Drops everything belonging to an encoder that is going away.
    fn reset_mse(&self) {
        let mut shared = self.shared.lock().unwrap();

        if let Some(mse) = shared.mse.as_mut() {
            mse.init = None;
            mse.recent.clear();
        }
    }

    /// Stores a piece of the stream and forwards it to every viewer.
    fn publish_mse_chunk(&self, chunk: Chunk) {
        let mut shared = self.shared.lock().unwrap();
        let Some(mse) = shared.mse.as_mut() else {
            return;
        };

        let mut keyframe = false;

        let bytes = match chunk {
            Chunk::Init(init) => {
                // Store it for viewers that arrive later, and send it to the ones already
                // connected: they connected before the encoder started, when there was
                // nothing to send them yet.
                let init = Arc::new(init);
                mse.init = Some(Arc::clone(&init));
                mse.recent.clear();
                init
            }
            Chunk::Fragment(fragment) => {
                // Fragments are cut by time now, so they are not key frame aligned and the
                // replay buffer has to start at one: a decoder cannot begin in the middle.
                keyframe = fmp4::contains_keyframe(&fragment);

                if keyframe {
                    mse.recent.clear();
                }

                Arc::new(fragment)
            }
        };

        mse.recent.push_back(Arc::clone(&bytes));

        while mse.recent.len() > REPLAY_FRAGMENTS {
            mse.recent.pop_front();
        }

        if mse.clients.is_empty() {
            return;
        }

        let before = mse.clients.len();

        mse.clients.retain_mut(|client| {
            // The page can only resume decoding at a key frame, so say when one is coming.
            if keyframe && ws::send_text(&mut client.stream, KEYFRAME_MARKER.as_bytes()).is_err() {
                return false;
            }

            ws::send_binary(&mut client.stream, &bytes).is_ok()
        });

        if mse.clients.len() != before {
            eprintln!(
                "dropped {} viewer(s): the socket would not take {} KB",
                before - mse.clients.len(),
                bytes.len() / 1024
            );
        }
    }

    /// Registers a MediaSource viewer: the MIME type, the track description, then the
    /// fragments buffered since the last init segment.
    fn register_mse_client(&self, mut stream: TcpStream) -> std::io::Result<u64> {
        let mut shared = self.shared.lock().unwrap();
        let mse = shared
            .mse
            .as_mut()
            .ok_or_else(|| std::io::Error::other("MSE mode is not active"))?;

        let config_len = mse.config.len();
        let sent = ws::send_text(&mut stream, mse.config.as_bytes());

        if std::env::var("OPENSCREEN_DEBUG").is_ok() {
            eprintln!("mse: config {config_len} bytes -> {sent:?}");
        }

        if let Some(init) = &mse.init {
            ws::send_binary(&mut stream, init)?;
        }

        for fragment in &mse.recent {
            ws::send_binary(&mut stream, fragment)?;
        }

        let id = mse.next_client;
        mse.next_client += 1;
        mse.clients.push(H264Client { id, stream });

        Ok(id)
    }

    fn unregister_mse_client(&self, id: u64) {
        let mut shared = self.shared.lock().unwrap();

        if let Some(mse) = shared.mse.as_mut() {
            mse.clients.retain(|client| client.id != id);
        }
    }
}

/// Prints the address (or addresses) another device on the LAN should open.
fn print_addresses(port: u16) {
    let addresses = sys::lan_addresses();

    match addresses.first() {
        Some(address) => println!("open http://{address}:{port}/ from the other machine"),
        None => println!("open http://127.0.0.1:{port}/ on this machine, and use its LAN IP elsewhere"),
    }

    if addresses.len() > 1 {
        let others: Vec<&str> = addresses.iter().skip(1).map(String::as_str).collect();
        println!("other addresses on this machine: {}", others.join(", "));
    }
}

/// Starts serving. `height` is the frame height, the width follows the screen aspect ratio.
pub fn run(port: u16, height: usize, fps: u32, quality: i32, ffmpeg: PathBuf, mode: Mode) -> Result<(), String> {
    // Before anything reads a size: at 125% scaling Windows otherwise describes this desktop as
    // 2048x1280, which caps the frame height and puts every injected pointer 20% off.
    sys::become_dpi_aware();

    // A height of zero means native: every later stage compares against the source size, so
    // passing the screen's own height is what makes the scaler skip its work entirely.
    //
    // ponytail: this uses the Win32 primary monitor size, while the duplicated output can be a
    // different monitor. When they differ, the encoder still sends what DXGI gives it (no
    // scaling, so quality is unaffected) and only the advertised level is conservative. Fix by
    // probing the DXGI output size once before the encoder starts.
    let height = if height == 0 {
        sys::screen_size().1.max(1) as usize
    } else {
        height
    };
    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|error| format!("cannot bind: {error}"))?;

    // Frame pacing sleeps for a few milliseconds; without a finer timer those waits round up
    // to the default 15.6 ms tick and a 60 fps target becomes 50.
    sys::begin_period();

    let settings = Settings {
        height,
        fps,
        quality,
        ffmpeg: if ffmpeg.as_os_str().is_empty() {
            default_ffmpeg_path()
        } else {
            ffmpeg
        },
        screen: None,
        mode,
    };

    let state = Arc::new(State::new(settings.clone()));

    println!(
        "serving {} on http://127.0.0.1:{port}/  ({}p, {fps} fps, {} kbps)",
        mode_name(&settings.mode),
        if settings.height == 0 {
            sys::screen_size().1.max(1) as usize
        } else {
            settings.height
        },
        settings.mode.bitrate_kbps()
    );

    // Which of this machine's addresses another device should use is not obvious: there are
    // usually virtual adapters from WSL, VMware or a hotspot as well.
    print_addresses(port);
    println!("open that address in a browser to change the settings");
    println!("remote control is off; turn it on in the panel, or type x + Enter here to stop it");

    // Creating the transport up front closes a race: a page that connects in the first moments
    // would find no hub, receive no offer, and sit there with a dead panel.
    let (screen_width, screen_height) = sys::screen_size();
    let (frame_width, frame_height) = scaler::target_size(
        screen_width.max(1) as usize,
        screen_height.max(1) as usize,
        height,
    );
    let (_, level_hex) = ffmpeg::level_for(frame_width, frame_height, fps);

    match webrtc_stream::Hub::new(
        format!("{}{level_hex}", settings.mode.profile().sdp_prefix()),
        fps,
    ) {
        Some(hub) => state.set_hub(hub),
        None => eprintln!("webrtc: the runtime did not start; that mode will not work"),
    }

    // One encoder per generation. When the panel changes something the counter moves, the running
    // encoder returns (dropping its ffmpeg child) and the next one starts with the new settings.
    let supervisor = Arc::clone(&state);

    std::thread::spawn(move || loop {
        let generation = supervisor.generation();
        let settings = supervisor.settings();
        let worker = Arc::clone(&supervisor);

        std::thread::spawn(move || encode_for(worker, settings, generation));

        while supervisor.generation() == generation {
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    {
        let cursor_state = Arc::clone(&state);
        std::thread::spawn(move || track_cursor(cursor_state));
    }

    // A way out that does not need a browser: the panel is one tab among many, and a viewer that
    // has the pointer should be stoppable from the window in front of the person being watched.
    {
        let stopper = Arc::clone(&state);

        std::thread::spawn(move || {
            let mut line = String::new();

            while std::io::stdin().read_line(&mut line).is_ok_and(|read| read > 0) {
                if line.trim().eq_ignore_ascii_case("x") {
                    stopper.set_control(false);
                }

                line.clear();
            }
        });
    }

    for connection in listener.incoming() {
        let socket = match connection {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!("accept failed: {error}");
                continue;
            }
        };

        let viewer_state = Arc::clone(&state);
        std::thread::spawn(move || handle_connection(viewer_state, socket));
    }

    sys::end_period();
    Ok(())
}

fn handle_connection(state: Arc<State>, mut socket: TcpStream) {
    let _ = socket.set_nodelay(true);
    let _ = socket.set_write_timeout(Some(Duration::from_millis(WRITE_TIMEOUT_MS)));

    let _ = socket.set_read_timeout(Some(Duration::from_secs(3)));
    let request = match ws::read_request(&mut socket) {
        Ok(request) => request,
        Err(_) => return,
    };
    let _ = socket.set_read_timeout(None);

    if request.is_empty() {
        return;
    }

    match ws::path(&request).as_str() {
        "/" | "/index.html" => serve_page(&mut socket, panel_page::PAGE),
        "/view" => serve_page(&mut socket, home(&state)),
        "/settings" => serve_settings(state, socket, &request),
        "/control" => serve_control(state, socket, &request),
        "/h264" => serve_page(&mut socket, h264_page::PAGE),
        "/h264/ws" => serve_h264_channel(state, socket, &request),
        "/mse" => serve_page(&mut socket, mse_page::PAGE),
        "/mse/ws" => serve_mse_channel(state, socket, &request),
        "/webrtc" => serve_page(&mut socket, webrtc_page::PAGE),
        "/webrtc/ws" => serve_webrtc_channel(state, socket, &request),
        "/player.js" => serve_script(&mut socket),
        "/ws" => serve_cursor_channel(state, socket, &request),
        _ => match state.kind() {
            Kind::Mjpeg => serve_stream(state, socket),
            _ => serve_page(&mut socket, home(&state)),
        },
    }
}

/// The cursor overlay every player page loads.
fn serve_script(socket: &mut TcpStream) {
    let body = player_js::JS;
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/javascript; charset=utf-8\r\n\
         Cache-Control: no-store\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );

    let _ = socket.write_all(response.as_bytes());
    let _ = socket.write_all(body.as_bytes());
    let _ = socket.flush();
}

/// Turns remote control on or off.
///
/// Deliberately not part of the settings form: those restart the encoder, and flipping a switch
/// should not cost the viewer its connection.
fn serve_control(state: Arc<State>, mut socket: TcpStream, request: &str) {
    if request.starts_with("POST") {
        let body = ws::body(&mut socket, request).unwrap_or_default();
        state.set_control(body.split('&').any(|pair| pair.trim() == "on=1"));
    }

    let body = state.control_message();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let _ = socket.write_all(response.as_bytes());
}

/// Reads the settings as JSON, or applies a form post and asks for a new encoder.
fn serve_settings(state: Arc<State>, mut socket: TcpStream, request: &str) {
    let settings = state.settings();

    if !request.starts_with("POST") {
        let addresses: Vec<String> = sys::lan_addresses()
            .into_iter()
            .map(|address| format!("\"{address}\""))
            .collect();

        let screens: Vec<String> = Duplicator::outputs()
            .into_iter()
            .map(|output| {
                format!(
                    "{{\"index\":{},\"left\":{},\"top\":{},\"width\":{},\"height\":{},\"primary\":{}}}",
                    output.index,
                    output.left,
                    output.top,
                    output.width,
                    output.height,
                    output.primary
                )
            })
            .collect();

        let body = format!(
            "{{\"mode\":\"{}\",\"height\":{},\"fps\":{},\"bitrate\":{},\"quality\":{},\"profile\":\"{}\",\"screen\":{},\"capturing\":{},\"viewers\":{},\"generation\":{},\"control\":{},\"controller\":{},\"addresses\":[{}],\"screens\":[{}]}}",
            match settings.mode {
                Mode::Mjpeg { .. } => "mjpeg",
                Mode::H264 { .. } => "h264",
                Mode::Mse { .. } => "mse",
                Mode::WebRtc { .. } => "webrtc",
            },
            settings.height,
            settings.fps,
            settings.mode.bitrate_kbps(),
            settings.quality,
            match settings.mode.profile() {
                ffmpeg::Profile::Baseline => "baseline",
                ffmpeg::Profile::Main => "main",
                ffmpeg::Profile::High => "high",
            },
            match settings.screen {
                Some(index) => index.to_string(),
                None => "null".to_string(),
            },
            match state.capturing() {
                Some(index) => index.to_string(),
                None => "null".to_string(),
            },
            state.viewers() + state.h264_clients() + state.mse_clients() + state.hub().map(|hub| hub.viewers()).unwrap_or(0),
            state.generation(),
            state.control(),
            match state.controller() {
                Some(who) => format!("\"{who}\""),
                None => "null".to_string(),
            },
            addresses.join(","),
            screens.join(",")
        );

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );

        let _ = socket.write_all(response.as_bytes());
        return;
    }

    let body = match ws::body(&mut socket, request) {
        Some(body) => body,
        None => return refuse(&mut socket, "cannot read the form"),
    };

    match apply_form(&settings, &body) {
        Ok(next) => {
            println!(
                "settings: {} at {}p, {} fps, {} kbps",
                mode_name(&next.mode),
                if next.height == 0 {
                    sys::screen_size().1.max(1) as usize
                } else {
                    next.height
                },
                next.fps,
                next.mode.bitrate_kbps()
            );

            state.set_settings(next);

            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
        Err(error) => refuse(&mut socket, &error),
    }
}

fn refuse(socket: &mut TcpStream, reason: &str) {
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{reason}",
        reason.len()
    );

    let _ = socket.write_all(response.as_bytes());
}

fn mode_name(mode: &Mode) -> &'static str {
    match mode {
        Mode::Mjpeg { .. } => "MJPG",
        Mode::H264 { .. } => "H.264/WebCodecs",
        Mode::Mse { .. } => "H.264/MediaSource",
        Mode::WebRtc { .. } => "H.264/WebRTC",
    }
}

/// Turns the form into settings, clamped: this endpoint is reachable from the whole LAN and a
/// nonsense value would otherwise reach ffmpeg as an argument.
fn apply_form(current: &Settings, body: &str) -> Result<Settings, String> {
    let mut fields: Vec<(&str, &str)> = Vec::new();

    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            fields.push((key, value));
        }
    }

    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    };

    let number = |name: &str, fallback: usize, low: usize, high: usize| {
        field(name)
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(fallback)
            .clamp(low, high)
    };

    let height = number("height", current.height, 0, 4320);

    let screens = Duplicator::outputs();

    let screen = match field("screen") {
        Some("auto") | None => current.screen,
        Some(value) => value
            .parse::<u32>()
            .ok()
            .filter(|index| screens.iter().any(|output| output.index == *index))
            .or(current.screen),
    };
    let fps = number("fps", current.fps as usize, 1, 240) as u32;
    let quality = number("quality", current.quality as usize, 30, 95) as i32;
    let bitrate = number("bitrate", current.mode.bitrate_kbps() as usize, 500, 100_000) as u32;
    let profile = field("profile")
        .and_then(ffmpeg::Profile::parse)
        .unwrap_or_else(|| current.mode.profile());

    let mode = match field("mode") {
        Some("mjpeg") => Mode::Mjpeg {
            subsampling: current.mode.subsampling(),
        },
        Some("h264") => Mode::H264 {
            bitrate_kbps: bitrate,
            ffmpeg: current.ffmpeg.clone(),
        },
        Some("mse") => Mode::Mse {
            bitrate_kbps: bitrate,
            gop: match current.mode {
                Mode::Mse { gop, .. } => gop,
                _ => 60,
            },
            fragment_ms: match current.mode {
                Mode::Mse { fragment_ms, .. } => fragment_ms,
                _ => 16,
            },
            ffmpeg: current.ffmpeg.clone(),
        },
        Some("webrtc") => Mode::WebRtc {
            bitrate_kbps: bitrate,
            profile,
            ffmpeg: current.ffmpeg.clone(),
        },
        _ => current.mode.clone(),
    };

    Ok(Settings {
        height,
        fps,
        quality,
        ffmpeg: current.ffmpeg.clone(),
        screen,
        mode,
    })
}

/// The page the active mode serves at `/`.
fn home(state: &State) -> &'static str {
    match state.kind() {
        Kind::Mjpeg => page::PAGE,
        Kind::WebCodecs => h264_page::PAGE,
        Kind::MediaSource => mse_page::PAGE,
        Kind::WebRtc => webrtc_page::PAGE,
    }
}

fn serve_page(socket: &mut TcpStream, content: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Cache-Control: no-store\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        content.len()
    );

    let _ = socket.write_all(response.as_bytes());
    let _ = socket.write_all(content.as_bytes());
    let _ = socket.flush();
}

/// The page's side channel: pointer shape and position out, and — once the panel allows it — the
/// viewer's own pointer and keyboard in.
fn serve_cursor_channel(state: Arc<State>, mut socket: TcpStream, request: &str) {
    let upgraded = ws::handshake(&mut socket, request).unwrap_or(false);

    if !upgraded {
        let _ = socket.write_all(page::NOT_FOUND.as_bytes());
        return;
    }

    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };

    let id = match state.register_cursor_client(writer) {
        Ok(id) => id,
        Err(_) => return,
    };

    // Whose input this is: the panel shows it, and the console says it out loud.
    let who = socket
        .peer_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_default();

    let channel = Arc::clone(&state);
    let from = who.clone();

    ws::watch(&mut socket, move |frame| {
        if let Some(message) = ws::text_payload(frame) {
            channel.inject(&from, &message);
        }
    });

    state.release_control(&who);
    state.unregister_cursor_client(id);
}

fn serve_h264_channel(state: Arc<State>, mut socket: TcpStream, request: &str) {
    let upgraded = ws::handshake(&mut socket, request).unwrap_or(false);

    if !upgraded {
        let _ = socket.write_all(page::NOT_FOUND.as_bytes());
        return;
    }

    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };

    let id = match state.register_h264_client(writer) {
        Ok(id) => id,
        Err(_) => return,
    };

    ws::watch(&mut socket, |_| {});

    state.unregister_h264_client(id);
}

fn serve_mse_channel(state: Arc<State>, mut socket: TcpStream, request: &str) {
    let upgraded = ws::handshake(&mut socket, request).unwrap_or(false);

    if !upgraded {
        let _ = socket.write_all(page::NOT_FOUND.as_bytes());
        return;
    }

    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };

    let id = match state.register_mse_client(writer) {
        Ok(id) => id,
        Err(_) => return,
    };

    ws::watch(&mut socket, |_| {});

    state.unregister_mse_client(id);
}

/// WebRTC signalling: one text message each way, the offer and then the answer.
fn serve_webrtc_channel(state: Arc<State>, mut socket: TcpStream, request: &str) {
    let upgraded = ws::handshake(&mut socket, request).unwrap_or(false);

    if !upgraded {
        let _ = socket.write_all(page::NOT_FOUND.as_bytes());
        return;
    }

    let hub = match state.hub() {
        Some(hub) => hub,
        None => return,
    };

    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };

    let id = hub.add_viewer(writer);

    // The answer arrives as one text frame; after that the connection only has to stay alive.
    ws::watch(&mut socket, |data| {
        if let Some(sdp) = ws::text_payload(data) {
            hub.accept_answer(id, sdp);
        }
    });
}

fn serve_stream(state: Arc<State>, mut socket: TcpStream) {
    state.count_viewer(1);

    if socket.write_all(mjpeg::RESPONSE_HEADERS.as_bytes()).is_err() {
        state.count_viewer(-1);
        return;
    }

    let mut sent = 0u64;

    while let Some((frame, version)) = state.wait_for_frame(sent) {
        let started = Instant::now();
        let header = mjpeg::frame_header(frame.len());

        if socket.write_all(header.as_bytes()).is_err()
            || socket.write_all(&frame).is_err()
            || socket.write_all(mjpeg::FRAME_TRAILER).is_err()
        {
            break;
        }

        sent = version;

        // A frame that cannot be handed to the network quickly means this viewer, or its link,
        // is the bottleneck. The threshold is generous on purpose: a single slow write is a
        // hiccup, and dropping the quality for it is what leaves the picture soft.
        if started.elapsed().as_millis() as u64 > 60 {
            state.mark_slow();
        }
    }

    state.count_viewer(-1);
}

/// Polls the pointer and pushes its position in frame coordinates when it moves.
/// Maps a pointer position in virtual desktop pixels onto the frame being sent.
///
/// `monitor` is (left, top, width, height) of the captured monitor. Returns None when the pointer
/// is not on that monitor, which is when nothing should be drawn.
fn cursor_in_frame(
    cursor: (i32, i32),
    monitor: (i32, i32, u32, u32),
    frame: (usize, usize),
) -> Option<(i32, i32)> {
    let (left, top, width, height) = monitor;

    if width == 0 || height == 0 || frame.0 == 0 || frame.1 == 0 {
        return None;
    }

    let local_x = cursor.0 as i64 - left as i64;
    let local_y = cursor.1 as i64 - top as i64;

    if local_x < 0 || local_y < 0 || local_x >= width as i64 || local_y >= height as i64 {
        return None;
    }

    Some((
        (local_x * frame.0 as i64 / width as i64) as i32,
        (local_y * frame.1 as i64 / height as i64) as i32,
    ))
}

/// Publishes the pointer position, mapped into the frame that is actually being sent.
///
/// The pointer is in virtual desktop pixels while the frame is one monitor, so the position has to
/// be made relative to the monitor being captured. Using the primary monitor for this (as this
/// once did) leaves the pointer stuck whenever another monitor is shared.
fn track_cursor(state: Arc<State>) {
    let mut last = (-1, -1);
    let mut last_shape = isize::MIN;
    let mut tick = 0u32;

    // Enumerating monitors creates a D3D11 device, so it is cached and refreshed slowly.
    let mut monitors = Duplicator::outputs();
    let mut monitors_at = Instant::now();

    loop {
        std::thread::sleep(CURSOR_POLL);

        // Reading the bitmap costs more than reading the position, so check the shape on a
        // slower cadence. The handle changes when the system swaps the pointer.
        tick += 1;

        if tick % 20 == 0 {
            if let Some(shape) = cursor::current() {
                if shape.handle != last_shape {
                    last_shape = shape.handle;
                    state.set_cursor_shape(&shape);
                }
            }
        }

        if monitors_at.elapsed() > Duration::from_secs(2) {
            monitors = Duplicator::outputs();
            monitors_at = Instant::now();
        }

        let Some((x, y)) = sys::cursor_position() else {
            continue;
        };

        let (frame_width, frame_height) = state.frame_size();

        if frame_width == 0 || frame_height == 0 {
            continue;
        }

        // The monitor being shared, or the primary one when the capture has not started yet.
        let monitor = state
            .capturing()
            .and_then(|index| monitors.iter().find(|output| output.index == index))
            .or_else(|| monitors.iter().find(|output| output.primary));

        let Some(monitor) = monitor else {
            continue;
        };

        let mapped = cursor_in_frame(
            (x, y),
            (monitor.left, monitor.top, monitor.width, monitor.height),
            (frame_width, frame_height),
        );

        // The pointer is on another monitor: there is nothing on this screen to point at.
        let Some((frame_x, frame_y)) = mapped else {
            state.hide_cursor();
            continue;
        };

        if (frame_x, frame_y) != last {
            last = (frame_x, frame_y);
            state.set_cursor(frame_x, frame_y);
        }
    }
}
/// Runs ffmpeg and pushes every access unit to the WebRTC viewers.
fn encode_webrtc(state: Arc<State>, source: (usize, usize), height: usize, fps: u32, bitrate_kbps: u32, profile: ffmpeg::Profile, output_index: u32, offset: (i32, i32), ffmpeg_path: PathBuf, generation: u64) {
    let (screen_width, screen_height) = source;
    let (frame_width, frame_height) = scaler::target_size(
        screen_width.max(1) as usize,
        screen_height.max(1) as usize,
        height,
    );

    state.set_frame_size(frame_width, frame_height);

    let encoder = ffmpeg::choose_encoder(&ffmpeg_path);

    let mut options = ffmpeg::Options {
        ffmpeg: ffmpeg_path,
        output: ffmpeg::Output::AnnexB,
        gop: fps as u32,
        fragment_ms: 16,
        profile,
        source: (screen_width.max(1) as usize, screen_height.max(1) as usize),
        output_index,
        offset,
        capture: ffmpeg::capture_from_env(),
        encoder,
        width: frame_width,
        height: frame_height,
        fps,
        bitrate_kbps,
    };

    let hub = match state.hub() {
        Some(hub) => hub,
        None => return,
    };

    let mut failures = 0u32;

    loop {
        if state.generation() != generation {
            return;
        }

        while hub.viewers() == 0 {
            std::thread::sleep(Duration::from_millis(100));
        }

        state.set_capturing(Some(options.output_index));

        let mut encoder = match ffmpeg::Encoder::spawn(&options) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        };

        let mut splitter = h264::Splitter::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut units = 0u64;

        loop {
            if state.generation() != generation {
                return;
            }

            if hub.viewers() == 0 {
                break;
            }

            match encoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    for unit in splitter.push(&buffer[..read]) {
                        units += 1;
                        hub.broadcast(&unit.data);
                    }
                }
                Err(error) => {
                    eprintln!("encoder read failed: {error}");
                    break;
                }
            }
        }

        if units == 0 && state.generation() == generation {
            // Try the next capture strategy before reporting a failure.
            if let Some(reason) = next_capture(&mut options) {
                eprintln!("capture: {reason}");
                drop(encoder);
                continue;
            }

            explain(&encoder, "WebRTC");
        }

        drop(encoder);
        pause(&mut failures);
    }
}

/// Runs ffmpeg in fragmented MP4 mode and forwards whole fragments to the viewers.
/// Prints what ffmpeg complained about, which is where the reason for a failure lives.
fn explain(encoder: &ffmpeg::Encoder, what: &str) {
    let error = encoder.last_error();

    if error.trim().is_empty() {
        eprintln!("{what}: the encoder stopped without producing anything");
    } else {
        eprintln!("{what}: the encoder stopped: {error}");
    }
}

/// Waits before trying again, backing off so a broken setup does not spin at full speed.
fn pause(failures: &mut u32) {
    *failures += 1;

    let seconds = 2u64.pow((*failures).min(5));
    eprintln!("retrying in {seconds}s");
    std::thread::sleep(Duration::from_secs(seconds));
}

/// The next capture strategy to try after a start produced nothing, or None when there is none
/// left: desktop duplication for the chosen monitor, then GDI, then any monitor that works.
fn next_capture(options: &mut ffmpeg::Options) -> Option<String> {
    match options.capture {
        ffmpeg::Capture::Desktop => {
            options.capture = ffmpeg::Capture::Gdi;
            Some("desktop duplication unavailable, falling back to GDI (slower)".to_string())
        }
        ffmpeg::Capture::Gdi => {
            let wanted = options.output_index;
            let working = crate::capture::working_output_index()?;

            if working == wanted {
                return None;
            }

            let monitors = Duplicator::outputs();
            let monitor = monitors.iter().find(|output| output.index == working)?;

            options.capture = ffmpeg::Capture::Desktop;
            options.output_index = working;
            options.offset = (monitor.left, monitor.top);
            options.source = (monitor.width as usize, monitor.height as usize);

            Some(format!(
                "monitor {wanted} cannot be captured at all, using monitor {working} instead"
            ))
        }
    }
}

/// Starts the encoder the current settings ask for.
fn encode_for(state: Arc<State>, settings: Settings, generation: u64) {
    // The chosen monitor, as a rectangle: its own size (not the primary one) and where it sits,
    // which is what GDI capture needs when desktop duplication is unavailable.
    let monitors = Duplicator::outputs();
    let monitor = settings
        .screen
        .and_then(|index| monitors.iter().find(|output| output.index == index));

    let (source_width, source_height) = match monitor {
        Some(output) => (output.width as usize, output.height as usize),
        None => {
            let (width, height) = sys::screen_size();
            (width.max(1) as usize, height.max(1) as usize)
        }
    };

    let offset = monitor
        .map(|output| (output.left, output.top))
        .unwrap_or((0, 0));

    // Zero means native: the screen's own height, so the scaler has nothing to do.
    let height = if settings.height == 0 {
        source_height
    } else {
        settings.height.min(source_height)
    };

    let fps = settings.fps;
    let quality = settings.quality;

    // The chosen monitor, or whichever one can be duplicated when the choice is automatic.
    let output_index = settings
        .screen
        .unwrap_or_else(|| crate::capture::working_output_index().unwrap_or(0));

    match &settings.mode {
        Mode::Mjpeg { subsampling } => produce(
            state,
            height,
            fps,
            quality,
            *subsampling,
            settings.screen,
            generation,
        ),
        Mode::H264 {
            bitrate_kbps,
            ffmpeg,
        } => encode_h264(
            state,
            (source_width, source_height),
            height,
            fps,
            *bitrate_kbps,
            output_index,
            offset,
            ffmpeg.clone(),
            generation,
        ),
        Mode::Mse {
            bitrate_kbps,
            gop,
            fragment_ms,
            ffmpeg,
        } => encode_mse(
            state,
            (source_width, source_height),
            height,
            fps,
            *bitrate_kbps,
            *gop,
            *fragment_ms,
            output_index,
            offset,
            ffmpeg.clone(),
            generation,
        ),
        Mode::WebRtc {
            bitrate_kbps,
            profile,
            ffmpeg,
        } => {
            // What the offer advertises has to describe the stream we send, and that depends on
            // the resolution the panel picked.
            let (frame_width, frame_height) =
                scaler::target_size(source_width, source_height, height);
            let (_, level_hex) = ffmpeg::level_for(frame_width, frame_height, fps);
            let advertised = format!("{}{level_hex}", profile.sdp_prefix());

            let hub = match state.hub() {
                Some(hub) => hub,
                None => {
                    let Some(hub) = webrtc_stream::Hub::new(advertised.clone(), fps) else {
                        eprintln!("cannot start the WebRTC runtime");
                        return;
                    };

                    state.set_hub(Arc::clone(&hub));
                    hub
                }
            };

            hub.set_profile_level_id(advertised);

            encode_webrtc(
                state,
                (source_width, source_height),
                height,
                fps,
                *bitrate_kbps,
                *profile,
                output_index,
                offset,
                ffmpeg.clone(),
                generation,
            )
        }
    }
}

/// Runs ffmpeg in Annex-B mode and hands whole access units to the WebCodecs page.
fn encode_h264(
    state: Arc<State>,
    source: (usize, usize),
    height: usize,
    fps: u32,
    bitrate_kbps: u32,
    output_index: u32,
    offset: (i32, i32),
    ffmpeg_path: PathBuf,
    generation: u64,
) {
    let (screen_width, screen_height) = source;
    let (frame_width, frame_height) =
        scaler::target_size(screen_width.max(1), screen_height.max(1), height);

    state.set_frame_size(frame_width, frame_height);
    state.set_h264_config(format!(
        "{{\"h264\":{{\"codec\":\"{}\",\"width\":{frame_width},\"height\":{frame_height},\"fps\":{fps}}}}}",
        ffmpeg::codec_string(frame_width, frame_height, fps)
    ));

    let encoder = ffmpeg::choose_encoder(&ffmpeg_path);

    let mut options = ffmpeg::Options {
        ffmpeg: ffmpeg_path,
        output: ffmpeg::Output::AnnexB,
        gop: 60,
        fragment_ms: 16,
        profile: ffmpeg::Profile::High,
        source: (screen_width.max(1), screen_height.max(1)),
        output_index,
        offset,
        capture: ffmpeg::capture_from_env(),
        encoder,
        width: frame_width,
        height: frame_height,
        fps,
        bitrate_kbps,
    };

    let mut failures = 0u32;

    loop {
        if state.generation() != generation {
            return;
        }

        while state.h264_clients() == 0 {
            std::thread::sleep(Duration::from_millis(200));
        }

        state.reset_h264_recent();
        state.set_capturing(Some(options.output_index));

        let mut encoder = match ffmpeg::Encoder::spawn(&options) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        };

        let mut splitter = h264::Splitter::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut units = 0u64;

        loop {
            if state.generation() != generation {
                return;
            }

            if state.h264_clients() == 0 {
                break;
            }

            match encoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    for unit in splitter.push(&buffer[..read]) {
                        units += 1;
                        state.publish_access_unit(Arc::new(unit));
                    }
                }
                Err(error) => {
                    eprintln!("encoder read failed: {error}");
                    break;
                }
            }
        }

        // Whatever the splitter still holds when ffmpeg stops.
        for unit in splitter.finish() {
            units += 1;
            state.publish_access_unit(Arc::new(unit));
        }

        if units == 0 && state.generation() == generation {
            // Try the next capture strategy before reporting a failure.
            if let Some(reason) = next_capture(&mut options) {
                eprintln!("capture: {reason}");
                drop(encoder);
                continue;
            }

            explain(&encoder, "H.264");
        }

        drop(encoder);
        pause(&mut failures);
    }
}
fn encode_mse(state: Arc<State>, source: (usize, usize), height: usize, fps: u32, bitrate_kbps: u32, gop: u32, fragment_ms: u32, output_index: u32, offset: (i32, i32), ffmpeg_path: PathBuf, generation: u64) {
    let (screen_width, screen_height) = source;
    let (frame_width, frame_height) = scaler::target_size(
        screen_width.max(1) as usize,
        screen_height.max(1) as usize,
        height,
    );

    state.set_frame_size(frame_width, frame_height);
    state.set_mse_config(format!(
        "{{\"mse\":{{\"mime\":\"{}\",\"width\":{frame_width},\"height\":{frame_height},\"fps\":{fps}}}}}",
        ffmpeg::mime_type(frame_width, frame_height, fps).replace('"', "\\\"")
    ));

    let encoder = ffmpeg::choose_encoder(&ffmpeg_path);

    let mut options = ffmpeg::Options {
        ffmpeg: ffmpeg_path,
        output: ffmpeg::Output::FragmentedMp4,
        gop,
        fragment_ms,
        profile: ffmpeg::Profile::High,
        source: (screen_width.max(1) as usize, screen_height.max(1) as usize),
        output_index,
        offset,
        capture: ffmpeg::capture_from_env(),
        encoder,
        width: frame_width,
        height: frame_height,
        fps,
        bitrate_kbps,
    };

    let mut failures = 0u32;

    loop {
        if state.generation() != generation {
            return;
        }

        while state.mse_clients() == 0 {
            std::thread::sleep(Duration::from_millis(200));
        }

        state.reset_mse();

        state.set_capturing(Some(options.output_index));

        let mut encoder = match ffmpeg::Encoder::spawn(&options) {
            Ok(encoder) => encoder,
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        };

        let mut splitter = fmp4::Splitter::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut chunks = 0u64;


        loop {
            if state.generation() != generation {
                return;
            }

            if state.mse_clients() == 0 {
                break;
            }

            match encoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    for chunk in splitter.push(&buffer[..read]) {
                        chunks += 1;
                        state.publish_mse_chunk(chunk);
                    }
                }
                Err(error) => {
                    eprintln!("encoder read failed: {error}");
                    break;
                }
            }
        }

        if chunks == 0 && state.generation() == generation {
            if let Some(reason) = next_capture(&mut options) {
                eprintln!("capture: {reason}");
                drop(encoder);
                continue;
            }

            explain(&encoder, "MSE");
        }

        drop(encoder);
        pause(&mut failures);
    }
}

fn produce(state: Arc<State>, height: usize, fps: u32, ceiling: i32, subsampling: crate::jpeg::Subsampling, screen: Option<u32>, generation: u64) {
    let mut duplicator = match Duplicator::new_at(screen) {
        Ok(duplicator) => duplicator,
        Err(error) => {
            eprintln!("screen capture unavailable: {error}");
            return;
        }
    };

    let (screen_width, screen_height) = match prime(&mut duplicator) {
        Ok(size) => size,
        Err(error) => {
            eprintln!("screen capture unavailable: {error}");
            return;
        }
    };

    // Which monitor this ended up being: not always the requested one, because a monitor the GPU
    // refuses to duplicate cannot be captured by this path at all.
    let capturing = duplicator.output_index();
    state.set_capturing(Some(capturing));

    if let Some(wanted) = screen {
        if wanted != capturing {
            eprintln!(
                "capture: monitor {wanted} cannot be duplicated, capturing monitor {capturing} instead"
            );
        }
    }

    let (frame_width, frame_height) = scaler::target_size(screen_width, screen_height, height);
    state.set_frame_size(frame_width, frame_height);

    let mut buffer = vec![0u8; frame_width * frame_height * 4];

    let encoder = match Encoder::new() {
        Ok(encoder) => encoder,
        Err(error) => {
            eprintln!("encoder unavailable: {error}");
            return;
        }
    };

    let period = Duration::from_millis((1000 / fps.max(1)) as u64);
    let mut next_frame = Instant::now();
    let mut next_adaptation = Instant::now() + Duration::from_secs(1);
    let mut quality = ceiling;
    let mut has_frame = false;
    let debug = std::env::var("OPENSCREEN_DEBUG").is_ok();
    let mut stats = StageStats::default();

    loop {
        if state.generation() != generation {
            return;
        }

        if state.viewers() == 0 {
            // Nobody is watching: do not capture at all.
            std::thread::sleep(Duration::from_millis(50));
            next_frame = Instant::now();
            has_frame = false;
            continue;
        }

        // Wait a moment for the very first frame, so a viewer never stares at a blank page
        // while the desktop happens to be still.
        let timeout = if has_frame { 0 } else { 250 };

        let started = Instant::now();

        match duplicator.acquire(timeout) {
            Ok(frame) => {
                let captured = started.elapsed();

                scaler::scale(
                    frame.pixels,
                    frame.width,
                    frame.height,
                    frame.stride,
                    &mut buffer,
                    frame_width,
                    frame_height,
                );
                drop(frame);
                duplicator.release();
                let scaled = started.elapsed();

                match encoder.encode(
                    &buffer,
                    frame_width,
                    frame_height,
                    frame_width * 4,
                    quality,
                    subsampling,
                ) {
                    Ok(jpeg) => {
                        stats.record(captured, scaled - captured, started.elapsed() - scaled, debug);
                        state.publish(jpeg);
                        has_frame = true;
                    }
                    Err(error) => eprintln!("encode failed: {error}"),
                }
            }
            Err(CaptureError::NoChange) => {}
            Err(error) => {
                eprintln!("capture failed: {error}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        if Instant::now() >= next_adaptation {
            quality = state.adapt(quality, ceiling);
            next_adaptation = Instant::now() + Duration::from_secs(1);
        }

        next_frame += period;
        let now = Instant::now();

        if next_frame > now {
            std::thread::sleep(next_frame - now);
        } else {
            // Behind schedule: skip the missed frames instead of queueing them up.
            next_frame = now;
        }
    }
}

/// Per-stage timings, printed when OPENSCREEN_DEBUG is set.
#[derive(Default)]
struct StageStats {
    frames: u32,
    capture_ms: f64,
    scale_ms: f64,
    encode_ms: f64,
    since: Option<Instant>,
}

impl StageStats {
    fn record(&mut self, capture: Duration, scale: Duration, encode: Duration, debug: bool) {
        self.frames += 1;
        self.capture_ms += capture.as_secs_f64() * 1000.0;
        self.scale_ms += scale.as_secs_f64() * 1000.0;
        self.encode_ms += encode.as_secs_f64() * 1000.0;

        let since = *self.since.get_or_insert_with(Instant::now);

        if since.elapsed() < Duration::from_secs(2) {
            return;
        }

        let frames = self.frames.max(1) as f64;

        if debug {
            println!(
                "frames {:3} in {:4.1}s | capture {:5.2} ms | scale {:5.2} ms | encode {:5.2} ms | total {:5.2} ms",
                self.frames,
                since.elapsed().as_secs_f64(),
                self.capture_ms / frames,
                self.scale_ms / frames,
                self.encode_ms / frames,
                (self.capture_ms + self.scale_ms + self.encode_ms) / frames
            );
        }

        self.frames = 0;
        self.capture_ms = 0.0;
        self.scale_ms = 0.0;
        self.encode_ms = 0.0;
        self.since = Some(Instant::now());
    }
}

/// Waits for the first frame, which is when DXGI reports the desktop size.
fn prime(duplicator: &mut Duplicator) -> Result<(usize, usize), CaptureError> {
    let deadline = Instant::now() + Duration::from_secs(3);

    while Instant::now() < deadline {
        match duplicator.acquire(100) {
            Ok(frame) => {
                let size = (frame.width, frame.height);
                drop(frame);
                duplicator.release();
                return Ok(size);
            }
            Err(CaptureError::NoChange) => continue,
            Err(error) => return Err(error),
        }
    }

    Err(CaptureError::Message(
        "no frame arrived within 3 seconds".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::cursor_in_frame;

    #[test]
    fn the_primary_monitor_is_the_identity_at_native_size() {
        let primary = (0, 0, 2560, 1600);

        assert_eq!(
            cursor_in_frame((1280, 800), primary, (2560, 1600)),
            Some((1280, 800))
        );
    }

    #[test]
    fn a_scaled_frame_still_maps_the_whole_monitor() {
        let primary = (0, 0, 2560, 1600);

        assert_eq!(
            cursor_in_frame((1280, 800), primary, (1280, 800)),
            Some((640, 400))
        );
        assert_eq!(
            cursor_in_frame((2559, 1599), primary, (1280, 800)),
            Some((1279, 799))
        );
    }

    #[test]
    fn a_second_monitor_is_mapped_relative_to_its_own_origin() {
        // The secondary monitor starts where the primary ends; without subtracting it, a pointer
        // at the far left of that screen would land at the frame's right edge.
        let secondary = (2560, 0, 2560, 1440);

        assert_eq!(
            cursor_in_frame((2560, 0), secondary, (2560, 1440)),
            Some((0, 0))
        );
        assert_eq!(
            cursor_in_frame((2560 + 100, 200), secondary, (2560, 1440)),
            Some((100, 200))
        );
    }

    #[test]
    fn a_pointer_on_another_monitor_maps_to_nothing() {
        let secondary = (2560, 0, 2560, 1440);

        assert_eq!(cursor_in_frame((100, 100), secondary, (2560, 1440)), None);
        assert_eq!(
            cursor_in_frame((2560 + 2560, 0), secondary, (2560, 1440)),
            None
        );
    }

    #[test]
    fn a_monitor_above_the_primary_works() {
        // Negative coordinates are legal in a virtual desktop, and a sign error here would hide
        // the pointer whenever the shared screen is positioned left of or above the primary.
        let above = (0, -1080, 1920, 1080);

        assert_eq!(
            cursor_in_frame((960, -1080), above, (1920, 1080)),
            Some((960, 0))
        );
    }

    #[test]
    fn a_frame_without_a_size_maps_to_nothing() {
        assert_eq!(cursor_in_frame((10, 10), (0, 0, 2560, 1600), (0, 0)), None);
    }
}