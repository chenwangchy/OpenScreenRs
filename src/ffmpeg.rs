//! The H.264 encoder, which is ffmpeg in a child process.
//!
//! Hand written Media Foundation interop or OpenH264's C++ virtual classes would avoid the
//! dependency, but both are a lot of fragile FFI for the same hardware encoder. ffmpeg reaches
//! NVENC (and the Intel equivalent, and a software fallback) through one command line, and the
//! hard part, cutting the byte stream into what the browser can eat, is ours either way.

use std::collections::VecDeque;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

/// H.264 profile. High compresses screen content best, but a strict receiver (iOS Safari is one)
/// may refuse it, and a connection that carries no media looks exactly like a broken stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profile {
    High,
    Main,
    Baseline,
}

impl Profile {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "high" => Some(Self::High),
            "main" => Some(Self::Main),
            "baseline" => Some(Self::Baseline),
            _ => None,
        }
    }

    fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Main => "main",
            Self::Baseline => "baseline",
        }
    }

    /// The first two bytes of `profile-level-id`, which is three bytes written as six hex digits:
    /// profile idc, constraint flags, then the level. Four digits is a malformed attribute and
    /// strict receivers reject the whole media section over it.
    pub fn sdp_prefix(self) -> &'static str {
        match self {
            Self::High => "6400",
            Self::Main => "4D40",
            // Constrained baseline: the constraint flag is what receivers actually look for.
            Self::Baseline => "42E0",
        }
    }
}

/// Where the frames come from. Desktop duplication runs on the GPU and is the fast path;
/// GDI needs no D3D11 at all, which is what makes it the fallback for machines where duplication
/// reports "function not implemented" (virtual machines, basic display adapters).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Desktop,
    Gdi,
}

/// What the encoder writes on stdout.
#[derive(Clone, Copy)]
pub enum Output {
    /// Raw Annex-B H.264, cut into access units for WebCodecs.
    AnnexB,
    /// Fragmented MP4, appended to a MediaSource. This is the one that works over plain http,
    /// because MediaSource is not a secure context API.
    FragmentedMp4,
}

pub struct Options {
    pub ffmpeg: PathBuf,
    pub output: Output,
    /// Key frame interval in frames.
    pub gop: u32,
    /// Longest a fragment may last, in milliseconds. This, not the key frame interval, is the
    /// floor on latency: a viewer can only be shown a fragment once all of it has arrived.
    pub fragment_ms: u32,
    /// Size of the captured desktop, before any scaling.
    pub source: (usize, usize),
    /// Which DXGI output to capture. Not always 0: with a second monitor plugged in, the primary
    /// is not necessarily the one that can be duplicated.
    pub output_index: u32,
    /// Where the chosen monitor sits in the virtual desktop; GDI capture aims with it.
    pub offset: (i32, i32),
    pub profile: Profile,
    pub capture: Capture,
    /// Which H.264 encoder to use: the machine's own hardware one, chosen at startup.
    pub encoder: String,
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub struct Encoder {
    child: Child,
    stdout: ChildStdout,
    /// The tail of what ffmpeg complained about, which is where the reason for a failure lives.
    complaints: Arc<Mutex<VecDeque<String>>>,
}

impl Encoder {
    pub fn spawn(options: &Options) -> Result<Self, String> {
        let arguments = arguments(options);

        if std::env::var("OPENSCREEN_DEBUG").is_ok() {
            println!("{} {}", options.ffmpeg.display(), arguments.join(" "));
        }

        let mut child = Command::new(&options.ffmpeg)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", options.ffmpeg.display()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "ffmpeg produced no stdout".to_string())?;

        // Drain stderr on its own thread: a full pipe would otherwise stall ffmpeg, and the
        // lines are the only explanation for a failed start.
        let complaints = Arc::new(Mutex::new(VecDeque::new()));

        if let Some(stderr) = child.stderr.take() {
            let collector = Arc::clone(&complaints);

            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut tail = collector.lock().unwrap();
                    tail.push_back(line);
                    if tail.len() > 6 {
                        tail.pop_front();
                    }
                }
            });
        }

        Ok(Self {
            child,
            stdout,
            complaints,
        })
    }

    /// What ffmpeg last complained about, empty when it kept quiet.
    pub fn last_error(&self) -> String {
        self.complaints
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// Reads the next chunk of the bitstream.
    pub fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stdout.read(buffer)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The H.264 level a frame size and rate needs: (value for `-level`, codec string suffix).
///
/// The level is a hard limit on macroblocks per frame and per second, and the encoder refuses to
/// start when it is exceeded: 2560x1600 at 60 fps needs level 5.1, where 1080p is happy with 4.2.
pub fn level_for(width: usize, height: usize, fps: u32) -> (&'static str, &'static str) {
    let blocks = width.div_ceil(16) * height.div_ceil(16);
    let per_second = blocks * fps as usize;

    for (level, hex, max_blocks, max_per_second) in [
        ("4.2", "2A", 8704, 522_240),
        ("5.0", "32", 22_080, 589_824),
        ("5.1", "33", 36_864, 983_040),
        ("5.2", "34", 36_864, 2_073_600),
    ] {
        if blocks <= max_blocks && per_second <= max_per_second {
            return (level, hex);
        }
    }

    ("5.2", "34")
}

/// The codec string WebCodecs and MediaSource both want: `avc1.` then the profile byte (0x64 is
/// High) and the level byte.
pub fn codec_string(width: usize, height: usize, fps: u32) -> String {
    format!(
        "avc1.6400{}",
        level_for(width, height, fps).1
    )
}

/// The MIME type a MediaSource is configured with.
pub fn mime_type(width: usize, height: usize, fps: u32) -> String {
    format!("video/mp4; codecs=\"{}\"", codec_string(width, height, fps))
}

/// `OPENSCREEN_CAPTURE=gdi` forces the GDI backend: the workaround for a machine where desktop
/// duplication is unavailable, and how the fallback path gets exercised on a machine where it is.
pub fn capture_from_env() -> Capture {
    match std::env::var("OPENSCREEN_CAPTURE").as_deref() {
        Ok("gdi") => Capture::Gdi,
        _ => Capture::Desktop,
    }
}

/// Picks the best available H.264 encoder. Hardware first, but every Windows machine has
/// MediaFoundation, so there is always something to fall back to.
pub fn choose_encoder(ffmpeg: &std::path::Path) -> String {
    static CHOSEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

    CHOSEN
        .get_or_init(|| {
            let output = std::process::Command::new(ffmpeg)
                .args(["-hide_banner", "-encoders"])
                .stderr(std::process::Stdio::null())
                .output();

            let listing = match output {
                Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
                Err(_) => String::new(),
            };

            // Being listed only means it was compiled in: a machine without the matching GPU
            // still lists h264_nvenc. A two frame null encode is the cheapest real answer.
            for candidate in ["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf", "libx264"] {
                if listing.contains(candidate) && encoder_works(ffmpeg, candidate) {
                    println!("encoder: {candidate}");
                    return candidate.to_string();
                }
            }

            // Nothing listed: let ffmpeg decide when it starts.
            "h264".to_string()
        })
        .clone()
}

/// Runs the encoder once on a synthetic frame. Anything but success means it cannot be used here.
fn encoder_works(ffmpeg: &std::path::Path, encoder: &str) -> bool {
    std::process::Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=320x240:r=10",
            "-frames:v",
            "2",
            "-c:v",
            encoder,
            "-f",
            "null",
            "-",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn arguments(options: &Options) -> Vec<String> {
    let bitrate = format!("{}k", options.bitrate_kbps);
    let hardware = options.encoder != "h264_mf" && options.encoder != "libx264";

    // draw_mouse=0 is essential: the pointer is drawn by the viewer as an overlay, so baking it
    // into the frame would show two cursors and make one of them lag behind the video.
    //
    // When no scaling is asked for, the graph stops at the capture: the frames stay on the GPU
    // and go straight into the encoder. Downloading them, running a scale that changes nothing
    // and uploading them again cost two full frame transfers and about 5 fps.
    let native = (options.width, options.height) == options.source;
    let fps = options.fps.to_string();
    let scale = format!(
        "scale={width}:{height}:flags=lanczos,format=nv12",
        width = options.width,
        height = options.height
    );

    // Access units for WebCodecs have no fragment constraint, so they keep a long GOP; fragments
    // are only sent once complete, so there the GOP is the latency floor.
    let gop = match options.output {
        Output::AnnexB => 60,
        Output::FragmentedMp4 => options.gop.max(1),
    }
    .to_string();
    let mut arguments: Vec<String> = ["-hide_banner", "-loglevel", "error"]
        .iter()
        .map(|argument| argument.to_string())
        .collect();

    // Latency first: no input buffering, no B frames, short GOP.
    arguments.extend(
        ["-fflags", "nobuffer"]
            .iter()
            .map(|argument| argument.to_string()),
    );

    // The two capture backends do not share a shape: desktop duplication is a filter (ddagrab)
    // and needs a D3D11 device, while GDI is an input device with its own options.
    match options.capture {
        Capture::Desktop => {
            arguments.extend(
                ["-init_hw_device", "d3d11va", "-filter_complex"]
                    .iter()
                    .map(|argument| argument.to_string()),
            );

            arguments.push(if native {
                format!(
                    "ddagrab=output_idx={output}:framerate={fps}:draw_mouse=0",
                    output = options.output_index,
                    fps = fps
                )
            } else {
                format!(
                    "ddagrab=output_idx={output}:framerate={fps}:draw_mouse=0,hwdownload,format=bgra,{scale}",
                    output = options.output_index,
                    fps = fps
                )
            });
        }
        Capture::Gdi => {
            // "desktop" alone would be the whole virtual desktop, which on a two monitor machine
            // is wider than an encoder can take, so capture exactly the primary screen.
            let size = format!(
                "{width}x{height}",
                width = options.source.0,
                height = options.source.1
            );
            let offset_x = options.offset.0.to_string();
            let offset_y = options.offset.1.to_string();

            arguments.extend(
                [
                    "-f",
                    "gdigrab",
                    "-framerate",
                    &fps,
                    "-draw_mouse",
                    "0",
                    "-video_size",
                    &size,
                    "-offset_x",
                    &offset_x,
                    "-offset_y",
                    &offset_y,
                    "-i",
                    "desktop",
                ]
                .iter()
                .map(|argument| argument.to_string()),
            );

            if !native {
                arguments.extend(["-vf", &scale].iter().map(|argument| argument.to_string()));
            }
        }
    }
    // Rate control is common, latency tuning is not: the option names differ per encoder, and an
    // unknown option makes ffmpeg refuse to start at all.
    let mut tuning: Vec<&str> = Vec::new();

    match options.encoder.as_str() {
        "h264_nvenc" => tuning.extend(["-preset", "p1", "-tune", "ull", "-rc", "cbr", "-bf", "0"]),
        "h264_qsv" => tuning.extend(["-preset", "veryfast", "-look_ahead", "0", "-bf", "0"]),
        "h264_amf" => tuning.extend(["-quality", "speed", "-rc", "cbr", "-bf", "0"]),
        // MediaFoundation and the built in encoder have no equivalent knobs.
        _ => {}
    }

    let level = level_for(options.width, options.height, options.fps).0.to_string();
    let mut common: Vec<&str> = vec!["-c:v", &options.encoder, "-b:v", &bitrate, "-maxrate", &bitrate, "-bufsize", "1000k", "-g", &gop];

    if hardware {
        common.extend(["-profile:v", options.profile.ffmpeg_name(), "-level", &level]);
    }

    arguments.extend(
        common
            .iter()
            .chain(tuning.iter())
            .map(|argument| argument.to_string())
            .collect::<Vec<_>>(),
    );

    let tail: Vec<String> = match options.output {
        Output::AnnexB => [
            // Access unit delimiters let our splitter cut the stream into frames without
            // parsing slice headers. `dump_extra` repeats the parameter sets before every key
            // frame, which is what lets a viewer that joins mid stream start decoding at once.
            "-bsf:v",
            "h264_metadata=aud=insert,dump_extra=freq=keyframe",
            "-f",
            "h264",
            "pipe:1",
        ]
        .iter()
        .map(|argument| argument.to_string())
        .collect(),
        Output::FragmentedMp4 => [
            // Empty moov so playback can start after the first fragment, no index needed.
            // `frag_keyframe` makes every key frame begin a fragment, which is what lets a viewer
            // start (or restart) at one; `frag_duration` then cuts the rest by time so latency is
            // a frame or two rather than a whole group of pictures. Flushing every packet keeps a
            // finished fragment from sitting in a buffer.
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-frag_duration",
            &(options.fragment_ms.max(1) as u64 * 1000).to_string(),
            "-flush_packets",
            "1",
            "-f",
            "mp4",
            "pipe:1",
        ]
        .iter()
        .map(|argument| argument.to_string())
        .collect(),
    };

    arguments.extend(tail);
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_level_that_covers_the_frame() {
        // 1080p at 60 fps is what level 4.2 was made for.
        assert_eq!(level_for(1728, 1080, 60).0, "4.2");

        // 2560x1600 at 60 fps is 16000 macroblocks a frame, beyond level 4.2's 8704: this is the
        // case that used to make the encoder refuse to start.
        assert_eq!(level_for(2560, 1600, 60).0, "5.1");

        // The same size at 30 fps fits level 5.0.
        assert_eq!(level_for(2560, 1600, 30).0, "5.0");
    }

    #[test]
    fn profile_level_id_is_six_hex_digits() {
        // Three bytes: profile idc, constraint flags, level. Four digits looks plausible and is
        // rejected by strict receivers, which is how the WebRTC mode once went black.
        for profile in [Profile::High, Profile::Main, Profile::Baseline] {
            let id = format!("{}{}", profile.sdp_prefix(), "2A");

            assert_eq!(id.len(), 6, "{profile:?} produced {id}");
            assert!(
                id.chars().all(|character| character.is_ascii_hexdigit()),
                "{profile:?} produced {id}"
            );
        }

        assert_eq!(format!("{}2A", Profile::High.sdp_prefix()), "64002A");
        assert_eq!(format!("{}2A", Profile::Baseline.sdp_prefix()), "42E02A");
    }

    #[test]
    fn codec_string_matches_the_level() {
        assert_eq!(codec_string(1728, 1080, 60), "avc1.64002A");
        assert_eq!(codec_string(2560, 1600, 60), "avc1.640033");
    }
}