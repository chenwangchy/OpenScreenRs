//! OpenScreen-rs: screen sharing from one machine to a browser over the network.
//!
//! The pipeline is deliberately the same shape as the C# implementation that was measured
//! before it: one producer captures and encodes, and every viewer only ever receives the
//! newest frame. What changes is the work per frame:
//!
//! * capture: DXGI desktop duplication, about 4-6 ms for a 2560x1600 desktop
//! * scale:   box filter on all cores, about 1 ms
//! * encode:  libjpeg-turbo, about 3 ms

mod capture;
mod cursor;
mod ffmpeg;
mod fmp4;
mod h264;
mod h264_page;
mod mse_page;
mod jpeg;
mod mjpeg;
mod page;
mod panel_page;
mod player_js;
mod scaler;
mod server;
mod sys;
mod webrtc_page;
mod webrtc_stream;
mod ws;

use std::error::Error;
use std::time::Instant;

fn main() {
    // The console on a Chinese Windows is code page 936, while everything here writes UTF-8:
    // without this, the one thing a user needs to read (an error) is unreadable.
    set_console_utf8();

    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Double clicking the executable must do something useful: sharing the screen and showing the
    // panel. Reading the help is only what an explicit `help` asks for.
    let command = args.first().map(String::as_str).unwrap_or("serve");

    match command {
        "bench" => {
            let height = number(&args, "--height", 1080) as usize;
            let frames = number(&args, "--frames", 120) as usize;
            let quality = number(&args, "--quality", 70) as i32;

            benchmark(height, frames, quality)
        }
        "serve" => {
            let port = number(&args, "--port", 3032) as u16;
            // 0 is native: downscaling is the one thing the viewer can never undo, so the
            // default is to send the screen exactly as it is.
            let height = number(&args, "--height", 0) as usize;
            let fps = number(&args, "--fps", 60);
            let quality = number(&args, "--quality", 70) as i32;

            let ffmpeg = ffmpeg_path(&args);

            // No arguments at all means a double click: pick the mode that gives the best
            // experience when ffmpeg is there, and open the panel so there is something to see.
            let double_click = args.is_empty();
            let default_mode = if double_click && ffmpeg.exists() {
                server::Mode::WebRtc {
                    bitrate_kbps: 16000,
                    profile: profile(&args),
                    ffmpeg: ffmpeg.clone(),
                }
            } else {
                server::Mode::Mjpeg {
                    subsampling: subsampling(&args),
                }
            };

            let mode = if flag(&args, "--webrtc") {
                server::Mode::WebRtc {
                    bitrate_kbps: number(&args, "--bitrate", 16000),
                    profile: profile(&args),
                    ffmpeg: ffmpeg_path(&args),
                }
            } else if flag(&args, "--mse") {
                server::Mode::Mse {
                    bitrate_kbps: number(&args, "--bitrate", 8000),
                    gop: number(&args, "--gop", 60),
                    fragment_ms: number(&args, "--fragment-ms", 16),
                    ffmpeg: ffmpeg_path(&args),
                }
            } else if flag(&args, "--h264") {
                server::Mode::H264 {
                    bitrate_kbps: number(&args, "--bitrate", 8000),
                    ffmpeg: ffmpeg_path(&args),
                }
            } else {
                default_mode
            };

            if double_click {
                open_panel(port);
            }

            server::run(port, height, fps, quality, ffmpeg, mode).map_err(|error| error.into())
        }
        "selftest" => {
            println!("{}", selftest());
            Ok(())
        }
        "info" => info(),
        _ => {
            println!("openscreen-rs");
            println!();
            println!("usage:");
            println!("  openscreen-rs serve [options]        stream to a browser");
            println!("  openscreen-rs selftest               check the WebSocket handshake");
            println!("  openscreen-rs info                  show desktop size and encoder status");
            println!("  openscreen-rs bench [options]        measure one frame end to end");
            println!();
            println!("options:");
            println!("  --port N      HTTP port for serve (default 3032)");
            println!("  --fps N       frame rate limit (default 60)");
            println!("  --height N    frame height, keeps the aspect ratio (default 0 = native)");
            println!("  --frames N    number of frames to measure (default 120)");
            println!("  --quality N   JPEG quality (default 70)");
            println!("  --subsampling 444|420  colour detail for MJPEG; 444 keeps coloured text sharp");
            println!("  --h264        stream H.264 instead of MJPEG (needs ffmpeg, WebCodecs page)");
            println!("  --mse         stream H.264 as fragmented MP4; works over http on any IP");
            println!("  --webrtc      stream H.264 over WebRTC (UDP); the lowest latency mode");
            println!("  --profile P   H.264 profile for --webrtc: baseline (default), main, high");
            println!("  --bitrate N   H.264 bitrate in kbps (default 8000, WebRTC 16000)");
            println!("  --fragment-ms N  longest fragment for --mse; this is the latency floor (default 16)");
            println!("  --gop N       key frame interval for --mse, i.e. the recovery interval (default 60)");
            println!("  --ffmpeg P    path to ffmpeg.exe (default: next to this program, or PATH)");
            Ok(())
        }
    }
}

fn info() -> Result<(), Box<dyn Error>> {
    let mut duplicator = capture::Duplicator::new()?;
    let (width, height) = prime(&mut duplicator)?;
    println!("desktop: {width}x{height} (DXGI desktop duplication)");

    match jpeg::Encoder::new() {
        Ok(_) => println!("encoder: libjpeg-turbo"),
        Err(error) => println!("encoder: unavailable ({error})"),
    }

    Ok(())
}

fn benchmark(target_height: usize, frames: usize, quality: i32) -> Result<(), Box<dyn Error>> {
    let mut duplicator = capture::Duplicator::new()?;
    let (source_width, source_height) = prime(&mut duplicator)?;

    let (frame_width, frame_height) = scaler::target_size(source_width, source_height, target_height);
    let mut target = vec![0u8; frame_width * frame_height * 4];
    let encoder = jpeg::Encoder::new().map_err(|error| error.to_string())?;

    println!(
        "desktop {source_width}x{source_height} -> frame {frame_width}x{frame_height}, quality {quality}"
    );

    let mut capture_times = Vec::with_capacity(frames);
    let mut scale_times = Vec::with_capacity(frames);
    let mut encode_times = Vec::with_capacity(frames);
    let mut sizes = Vec::with_capacity(frames);
    let mut attempts = 0usize;

    while capture_times.len() < frames {
        attempts += 1;

        let started = Instant::now();
        let frame = match duplicator.acquire(16) {
            Ok(frame) => frame,
            Err(capture::CaptureError::NoChange) => {
                if attempts > frames * 20 {
                    return Err("the desktop never changed, nothing to measure".into());
                }

                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let captured = started.elapsed();

        scaler::scale(
            frame.pixels,
            frame.width,
            frame.height,
            frame.stride,
            &mut target,
            frame_width,
            frame_height,
        );
        let scaled = started.elapsed();

        let jpeg = encoder
            .encode(
                &target,
                frame_width,
                frame_height,
                frame_width * 4,
                quality,
                jpeg::Subsampling::Full,
            )
            .map_err(|error| error.to_string())?;
        let encoded = started.elapsed();

        capture_times.push(captured.as_secs_f64() * 1000.0);
        scale_times.push((scaled - captured).as_secs_f64() * 1000.0);
        encode_times.push((encoded - scaled).as_secs_f64() * 1000.0);
        sizes.push(jpeg.len());

        drop(frame);
        duplicator.release();
    }

    let capture_ms = median(&mut capture_times);
    let scale_ms = median(&mut scale_times);
    let encode_ms = median(&mut encode_times);
    let total_ms = capture_ms + scale_ms + encode_ms;
    let average_size = sizes.iter().sum::<usize>() / sizes.len();

    println!();
    println!("capture+DXGI     {:8.1} ms", capture_ms);
    println!("scale (parallel) {:8.1} ms", scale_ms);
    println!("encode (turbo)   {:8.1} ms", encode_ms);
    println!("frame total      {:8.1} ms  ->  {:.0} fps", total_ms, 1000.0 / total_ms);
    println!(
        "frame size       {:8} KB  ->  {:.1} MB/s at that rate",
        average_size / 1024,
        average_size as f64 * (1000.0 / total_ms) / (1024.0 * 1024.0)
    );
    println!("frames measured  {frames} of {attempts} attempts");

    Ok(())
}

/// Waits for the first frame, which is when DXGI reports the desktop size.
fn prime(duplicator: &mut capture::Duplicator) -> Result<(usize, usize), Box<dyn Error>> {
    let deadline = Instant::now() + std::time::Duration::from_secs(3);

    while Instant::now() < deadline {
        match duplicator.acquire(100) {
            Ok(frame) => {
                let size = (frame.width, frame.height);
                drop(frame);
                duplicator.release();
                return Ok(size);
            }
            Err(capture::CaptureError::NoChange) => continue,
            Err(error) => return Err(error.into()),
        }
    }

    Err("no frame arrived within 3 seconds".into())
}

fn number(args: &[String], flag: &str, default: u32) -> u32 {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Full colour detail unless asked otherwise: half resolution colour is what makes screen
/// content look smeared.
fn subsampling(args: &[String]) -> jpeg::Subsampling {
    match args.iter().position(|argument| argument == "--subsampling") {
        Some(index) => match args.get(index + 1).map(String::as_str) {
            Some("420") => jpeg::Subsampling::Half,
            Some("444") => jpeg::Subsampling::Full,
            Some(other) => {
                eprintln!("unknown --subsampling {other}, using 444 (only 444 and 420 exist)");
                jpeg::Subsampling::Full
            }
            None => jpeg::Subsampling::Full,
        },
        None => jpeg::Subsampling::Full,
    }
}

/// Opens the control panel in the default browser, after giving the server a moment to listen.
fn open_panel(port: u16) {
    let url = format!("http://127.0.0.1:{port}/");

    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &url])
        .spawn();
}

/// H.264 profile for the WebRTC mode. High is the default; `baseline` is the one to try when a
/// strict receiver accepts the connection but shows no picture.
fn profile(args: &[String]) -> ffmpeg::Profile {
    args.iter()
        .position(|argument| argument == "--profile")
        .and_then(|index| args.get(index + 1))
        .and_then(|name| ffmpeg::Profile::parse(name))
        // Constrained baseline: every receiver supports it. High compresses better, but a
        // receiver that does not accept it rejects the media section and shows nothing at all,
        // which is a much worse outcome than a slightly larger stream.
        .unwrap_or(ffmpeg::Profile::Baseline)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|argument| argument == name)
}

/// Where ffmpeg lives: the flag, then the environment, then next to this program.
fn ffmpeg_path(args: &[String]) -> std::path::PathBuf {
    if let Some(index) = args.iter().position(|argument| argument == "--ffmpeg") {
        if let Some(value) = args.get(index + 1) {
            return std::path::PathBuf::from(value);
        }
    }

    if let Ok(value) = std::env::var("OPENSCREEN_FFMPEG") {
        return std::path::PathBuf::from(value);
    }

    if let Ok(program) = std::env::current_exe() {
        if let Some(directory) = program.parent() {
            let candidate = directory.join("ffmpeg.exe");

            if candidate.is_file() {
                return candidate;
            }
        }
    }

    std::path::PathBuf::from("ffmpeg.exe")
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Checks the pieces that are easy to get silently wrong.
fn selftest() -> String {
    // Test vectors from RFC 3174 (SHA-1) and RFC 6455 (WebSocket accept key).
    let mut failures = Vec::new();

    let empty = ws::sha1(b"");
    if hex(&empty) != "da39a3ee5e6b4b0d3255bfef95601890afd80709" {
        failures.push(format!("sha1(\"\") = {}", hex(&empty)));
    }

    let abc = ws::sha1(b"abc");
    if hex(&abc) != "a9993e364706816aba3e25717850c26c9cd0d89d" {
        failures.push(format!("sha1(\"abc\") = {}", hex(&abc)));
    }

    if ws::base64(b"") != "" {
        failures.push("base64(\"\") is not empty".to_string());
    }

    if ws::base64(b"f") != "Zg==" || ws::base64(b"fo") != "Zm8=" || ws::base64(b"foo") != "Zm9v" {
        failures.push("base64 padding is wrong".to_string());
    }

    let accept = ws::accept_key("dGhlIHNhbXBsZSBub25jZQ==");
    if accept != "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=" {
        failures.push(format!("accept key = {accept}"));
    }

    // The address printed for the other device is the one thing a user cannot verify themselves,
    // so it belongs in the self test: an empty list means the app would print nothing useful.
    let addresses = sys::lan_addresses();
    if addresses.is_empty() {
        failures.push("no LAN address found to print".to_string());
    }

    if failures.is_empty() {
        format!(
            "selftest: all checks passed (LAN address: {})",
            addresses.first().cloned().unwrap_or_default()
        )
    } else {
        format!("selftest: {} failed -> {}", failures.len(), failures.join(", "))
    }
}

/// Switches the console to UTF-8, so OS error messages are not mojibake.
fn set_console_utf8() {
    type SetCodePageFn = unsafe extern "system" fn(u32) -> i32;

    let Some(module) = sys::load_library("kernel32.dll") else {
        return;
    };

    for name in [b"SetConsoleOutputCP\0".as_slice(), b"SetConsoleCP\0".as_slice()] {
        if let Some(address) = sys::symbol(module, name) {
            let call: SetCodePageFn = unsafe { std::mem::transmute(address) };
            unsafe { call(65001) };
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}