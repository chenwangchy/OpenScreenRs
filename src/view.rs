//! The native viewer: two WebSocket channels, a decoder, and a window.
//!
//! The channels are the ones the browser pages already use, so the host needs no new port and no
//! new protocol: `/h264/ws` carries the H.264 access units, `/ws` carries the pointer, the control
//! state and the input going back. What is different is everything the browser could not do —
//! the keys reach the host instead of the browser, the physical scan code goes out unchanged so the
//! host's own input method composes Chinese, and the picture fills a real fullscreen window.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cursor::Shape;
use crate::decode::{Decoder, Frame};
use crate::json::{self, Value};
use crate::window::{self, Target};
use crate::{sys, ws};

const DEFAULT_PORT: u16 = 3032;
/// How long to wait before dialling the host again.
const RETRY: Duration = Duration::from_secs(1);

struct Viewer {
    /// Input on its way to the host. The control thread owns the socket, so it also owns every
    /// write: two threads writing to one socket can interleave two frames into nonsense.
    outgoing: Mutex<mpsc::Sender<String>>,
    cursor: Mutex<Option<(Shape, (i32, i32))>>,
    control: AtomicBool,
    frames: AtomicU64,
    /// Time from handing a unit to the decoder to the frame appearing, in tenths of a millisecond.
    decode_tenths: AtomicU64,
    rtt_ms: AtomicU64,
    size: Mutex<(usize, usize)>,
    started: Instant,
}

impl Target for Viewer {
    fn send(&self, message: &str) {
        let _ = self.outgoing.lock().unwrap().send(message.to_string());
    }

    fn cursor(&self) -> Option<(Shape, (i32, i32))> {
        self.cursor
            .lock()
            .unwrap()
            .as_ref()
            .map(|(shape, position)| (shape.clone(), *position))
    }

    fn closed(&self) {}
}

/// Connects and runs the window. Blocks until the user closes it.
pub fn run(
    address: &str,
    decode_width: usize,
    fullscreen: bool,
    ffmpeg: PathBuf,
) -> Result<(), String> {
    sys::become_dpi_aware();

    let address = with_port(address);

    // What the host serves decides whether this client can work at all, and guessing wrong would
    // leave a black window with no explanation.
    let settings = get(&address, "/settings")?;
    let settings =
        json::parse(&settings).ok_or_else(|| format!("{address} answered something that is not the settings json"))?;

    match settings.get("mode").and_then(Value::as_str) {
        Some("h264") | Some("webrtc") => {}
        Some(other) => {
            return Err(format!(
                "the host is sharing as {other}, which has no native channel; set it to WebRTC or H.264 in its panel"
            ))
        }
        None => return Err(format!("{address} is not an OpenScreen host")),
    }

    let frame: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));
    let (sender, queue) = mpsc::channel();

    let viewer = Arc::new(Viewer {
        outgoing: Mutex::new(sender),
        cursor: Mutex::new(None),
        control: AtomicBool::new(false),
        frames: AtomicU64::new(0),
        decode_tenths: AtomicU64::new(0),
        rtt_ms: AtomicU64::new(0),
        size: Mutex::new((0, 0)),
        started: Instant::now(),
    });

    println!("connecting to {address}");

    start_video(
        address.clone(),
        ffmpeg,
        decode_width,
        Arc::clone(&frame),
        Arc::clone(&viewer),
    );
    start_control(address.clone(), queue, Arc::clone(&viewer));
    start_title(Arc::clone(&viewer));

    window::run(viewer, frame, fullscreen)
}

/// `10.0.0.5` and `10.0.0.5:3035` both work; only the second may carry a port.
fn with_port(address: &str) -> String {
    if address.contains(':') {
        address.to_string()
    } else {
        format!("{address}:{DEFAULT_PORT}")
    }
}

// ---------------------------------------------------------------------------------------------
// The video channel
// ---------------------------------------------------------------------------------------------

fn start_video(
    address: String,
    ffmpeg: PathBuf,
    decode_width: usize,
    frame: Arc<Mutex<Option<Frame>>>,
    viewer: Arc<Viewer>,
) {
    std::thread::spawn(move || {
        loop {
            let mut client = match ws::Client::connect(&address, "/h264/ws") {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("video: {error}");
                    std::thread::sleep(RETRY);
                    continue;
                }
            };

            let Some((width, height)) = read_config(&mut client) else {
                eprintln!("video: the host did not say what it is sending");
                std::thread::sleep(RETRY);
                continue;
            };

            *viewer.size.lock().unwrap() = (width, height);

            let pending: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
            let arrived = {
                let viewer = Arc::clone(&viewer);
                let pending = Arc::clone(&pending);

                move || {
                    if let Some(started) = pending.lock().unwrap().take() {
                        viewer
                            .decode_tenths
                            .store(started.elapsed().as_micros() as u64 / 100, Ordering::Relaxed);
                    }

                    viewer.frames.fetch_add(1, Ordering::Relaxed);
                    window::invalidate();
                }
            };

            let mut decoder =
                match Decoder::spawn(&ffmpeg, (width, height), decode_width, Arc::clone(&frame), arrived) {
                    Ok(decoder) => decoder,
                    Err(error) => {
                        eprintln!("{error}");
                        std::thread::sleep(RETRY);
                        continue;
                    }
                };

            println!("video: {width}x{height}");

            loop {
                match client.read() {
                    Ok(ws::Message::Binary(unit)) => {
                        *pending.lock().unwrap() = Some(Instant::now());

                        if decoder.push(&unit).is_err() {
                            eprintln!("video: the decoder stopped");
                            break;
                        }
                    }
                    // A new configuration means the host changed resolution: the decoder has to
                    // start over, and so does the window's idea of the picture.
                    Ok(ws::Message::Text(text)) => {
                        eprintln!("video: the host changed the stream, reconnecting");
                        if stream_size(&text).is_some() {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("video: {error}");
                        break;
                    }
                }
            }

            drop(decoder);
            *frame.lock().unwrap() = None;
            std::thread::sleep(RETRY);
        }
    });
}

/// Waits for the first text message, which describes the stream.
fn read_config(client: &mut ws::Client) -> Option<(usize, usize)> {
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        match client.read() {
            Ok(ws::Message::Text(text)) => {
                if let Some(size) = stream_size(&text) {
                    return Some(size);
                }
            }
            Ok(ws::Message::Binary(_)) => continue,
            Err(_) => return None,
        }
    }

    None
}

/// The size out of `{"h264":{"codec":…,"width":…,"height":…,"fps":…}}`.
fn stream_size(config: &str) -> Option<(usize, usize)> {
    let message = json::parse(config)?;
    let h264 = message.get("h264")?;

    let width = h264.get("width")?.as_f64()? as usize;
    let height = h264.get("height")?.as_f64()? as usize;

    if width == 0 || height == 0 {
        return None;
    }

    Some((width, height))
}

// ---------------------------------------------------------------------------------------------
// The control channel
// ---------------------------------------------------------------------------------------------

fn start_control(address: String, queue: mpsc::Receiver<String>, viewer: Arc<Viewer>) {
    std::thread::spawn(move || {
        // The pointer image arrives as a description followed by its pixels.
        let mut pending_image: Option<(usize, usize, i32, i32)> = None;
        let mut shapes: isize = 0;

        loop {
            let mut client = match ws::Client::connect(&address, "/ws") {
                Ok(client) => client,
                Err(error) => {
                    eprintln!("control: {error}");
                    std::thread::sleep(RETRY);
                    continue;
                }
            };

            // Short reads so one thread can both listen and send. Four milliseconds is the cost
            // of not needing a second socket handle or a second writer.
            let _ = client.set_read_timeout(Some(Duration::from_millis(4)));

            let mut last_ping = Instant::now();

            loop {
                while let Ok(message) = queue.try_recv() {
                    if client.send_text(message.as_bytes()).is_err() {
                        break;
                    }
                }

                if last_ping.elapsed() > Duration::from_secs(1) {
                    let clock = viewer.started.elapsed().as_millis() as u64;
                    let _ = client.send_text(format!("p {clock}").as_bytes());
                    last_ping = Instant::now();
                }

                match client.read() {
                    Ok(ws::Message::Text(text)) => {
                        pending_image = handle(&viewer, &text, pending_image, &mut shapes);
                    }
                    Ok(ws::Message::Binary(pixels)) => {
                        if let Some((width, height, hotspot_x, hotspot_y)) = pending_image.take() {
                            shapes += 1;

                            *viewer.cursor.lock().unwrap() = Some((
                                Shape {
                                    width,
                                    height,
                                    hotspot_x,
                                    hotspot_y,
                                    rgba: pixels,
                                    handle: shapes,
                                },
                                (0, 0),
                            ));
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        }
    });
}

/// Applies one message from the host. Returns the image description still waiting for its pixels.
fn handle(
    viewer: &Viewer,
    text: &str,
    pending: Option<(usize, usize, i32, i32)>,
    shapes: &mut isize,
) -> Option<(usize, usize, i32, i32)> {
    let Some(message) = json::parse(text) else {
        return pending;
    };

    if let Some(clock) = message.get("pong").and_then(Value::as_f64) {
        let now = viewer.started.elapsed().as_millis() as f64;
        viewer
            .rtt_ms
            .store((now - clock).max(0.0) as u64, Ordering::Relaxed);

        return pending;
    }

    if let Some(control) = message.get("control").and_then(Value::as_bool) {
        viewer.control.store(control, Ordering::Relaxed);
    }

    if let Some(cursor) = message.get("cursor") {
        // A hidden pointer and a pointer that moved mean different things: the first one clears
        // the overlay, the second keeps its shape and only moves it.
        if cursor.is_null() {
            *viewer.cursor.lock().unwrap() = None;
            window::invalidate();

            return pending;
        }

        if let Some(position) = cursor.as_array().and_then(|items| {
            Some((items.first()?.as_i32()?, items.get(1)?.as_i32()?))
        }) {
            let mut slot = viewer.cursor.lock().unwrap();

            match slot.as_mut() {
                Some((_, current)) => *current = position,
                // The position can arrive before the first image; keep it until there is one.
                None => {
                    *slot = Some((
                        Shape {
                            width: 0,
                            height: 0,
                            hotspot_x: 0,
                            hotspot_y: 0,
                            rgba: Vec::new(),
                            handle: *shapes,
                        },
                        position,
                    ))
                }
            }

            drop(slot);
            window::invalidate();
        }
    }

    if let Some(image) = message.get("cursorImage") {
        let width = image.get("width").and_then(Value::as_f64).unwrap_or(0.0) as usize;
        let height = image.get("height").and_then(Value::as_f64).unwrap_or(0.0) as usize;
        let hotspot_x = image.get("hotspotX").and_then(Value::as_i32).unwrap_or(0);
        let hotspot_y = image.get("hotspotY").and_then(Value::as_i32).unwrap_or(0);

        return Some((width, height, hotspot_x, hotspot_y));
    }

    pending
}

// ---------------------------------------------------------------------------------------------
// The title bar
// ---------------------------------------------------------------------------------------------

fn start_title(viewer: Arc<Viewer>) {
    std::thread::spawn(move || {
        let mut previous = 0u64;

        loop {
            std::thread::sleep(Duration::from_secs(1));

            let frames = viewer.frames.load(Ordering::Relaxed);
            let rate = frames.saturating_sub(previous);
            previous = frames;

            let (width, height) = *viewer.size.lock().unwrap();
            let tenths = viewer.decode_tenths.load(Ordering::Relaxed);
            let rtt = viewer.rtt_ms.load(Ordering::Relaxed);

            let mut line = format!(
                "OpenScreen {width}x{height} · {rate} fps · decode {}.{} ms · rtt {rtt} ms",
                tenths / 10,
                tenths % 10
            );

            if !viewer.control.load(Ordering::Relaxed) {
                line.push_str(" · 只读（在主机面板开启远程控制）");
            }

            println!("{line}");
            window::set_title(&line);
        }
    });
}

// ---------------------------------------------------------------------------------------------
// A single HTTP request, for the settings that say whether this can work
// ---------------------------------------------------------------------------------------------

fn get(address: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(address)
        .map_err(|error| format!("cannot reach {address}: {error}"))?;

    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));

    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");

    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("cannot ask {address}: {error}"))?;

    let mut answer = String::new();

    std::io::BufReader::new(stream)
        .read_to_string(&mut answer)
        .map_err(|error| format!("cannot read from {address}: {error}"))?;

    let (head, body) = answer
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{address} did not answer like HTTP"))?;

    if !head.starts_with("HTTP/1.1 200") {
        return Err(format!(
            "{address} answered {}",
            head.lines().next().unwrap_or("nothing")
        ));
    }

    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::{stream_size, with_port};

    #[test]
    fn the_stream_size_comes_out_of_the_configuration_message() {
        let config = "{\"h264\":{\"codec\":\"avc1.640033\",\"width\":2560,\"height\":1600,\"fps\":60}}";

        assert_eq!(stream_size(config), Some((2560, 1600)));
        // Anything else is not a configuration, and must not be read as one.
        assert_eq!(stream_size("{\"cursor\":[1,2]}"), None);
        assert_eq!(stream_size("{\"h264\":{\"width\":0,\"height\":0}}"), None);
    }

    #[test]
    fn a_bare_address_gets_the_default_port() {
        assert_eq!(with_port("10.0.0.5"), "10.0.0.5:3032");
        assert_eq!(with_port("10.0.0.5:3035"), "10.0.0.5:3035");
    }
}
