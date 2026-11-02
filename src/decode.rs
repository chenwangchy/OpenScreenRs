//! The H.264 decoder, which is ffmpeg in a child process.
//!
//! Same reasoning as the encoder on the other side: the alternative is hand written Media
//! Foundation interop, and the ffmpeg that already ships next to this program has the decoder.
//! What comes back is BGRA at a size this side asked for, which is exactly what a Windows bitmap
//! needs.

use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

/// One decoded frame, BGRA, top down.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// The size to decode at: the stream, capped so a raw frame stays cheap to move and to paint.
///
/// The cap is not about the decoder's arithmetic. Raw frames travel through a pipe, and 2560x1600
/// at 60 fps is about a gigabyte per second, which is fine in theory and a stutter in practice.
/// The window scales whatever arrives onto its client area, so a smaller decode costs sharpness
/// only when the window is bigger than the cap.
pub fn decode_size(source: (usize, usize), cap: usize) -> (usize, usize) {
    let (width, height) = source;

    if cap == 0 || width == 0 || height == 0 || width <= cap {
        return (width.max(2), height.max(2));
    }

    // An even height: every H.264 frame has one, and rawvideo is happier.
    let scaled = (height * cap / width) & !1;

    (cap, scaled.max(2))
}

pub struct Decoder {
    child: Child,
    stdin: ChildStdin,
}

impl Decoder {
    /// Starts ffmpeg and a thread that keeps the newest frame in `frame`, calling `arrived` after
    /// each one so the window knows there is something to paint.
    pub fn spawn(
        ffmpeg: &Path,
        source: (usize, usize),
        cap: usize,
        frame: Arc<Mutex<Option<Frame>>>,
        arrived: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        let (width, height) = decode_size(source, cap);

        let mut arguments: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            // Latency first: no input buffering, and no frame reordering on the way out.
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-an",
            "-sn",
            "-dn",
        ]
        .iter()
        .map(|argument| argument.to_string())
        .collect();

        if (width, height) != source {
            arguments.push("-vf".to_string());
            arguments.push(format!("scale={width}:{height}:flags=fast_bilinear"));
        }

        arguments.extend(
            [
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgra",
                "pipe:1",
            ]
            .iter()
            .map(|argument| argument.to_string()),
        );

        let mut child = Command::new(ffmpeg)
            .args(&arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", ffmpeg.display()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "the decoder took no input".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the decoder produced no output".to_string())?;

        // ffmpeg explains itself on stderr, and a full pipe would stall it.
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("decoder: {line}");
                }
            });
        }

        let slot = Arc::clone(&frame);

        std::thread::spawn(move || read_frames(stdout, width, height, slot, arrived));

        Ok(Self { child, stdin })
    }

    /// Hands one access unit to the decoder.
    ///
    /// ponytail: this writes straight to ffmpeg's stdin, so a decoder that falls behind pushes back
    /// on the socket instead of dropping frames. ffmpeg decodes faster than the network delivers,
    /// and the host drops a viewer whose writes stall for five seconds, so there is a recovery
    /// path; the upgrade is a bounded queue that drops units until the next key frame.
    pub fn push(&mut self, access_unit: &[u8]) -> std::io::Result<()> {
        self.stdin.write_all(access_unit)?;
        self.stdin.flush()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Copies frames out of the decoder until it stops, always keeping only the newest.
fn read_frames(
    mut stdout: impl Read,
    width: usize,
    height: usize,
    latest: Arc<Mutex<Option<Frame>>>,
    arrived: impl Fn(),
) {
    let size = width * height * 4;
    let mut spare: Vec<u8> = Vec::new();

    loop {
        // Reuse the buffer of a frame the painter has finished with; the alternative is a few
        // hundred megabytes a second of allocation churn.
        if spare.len() != size {
            spare = vec![0u8; size];
        }

        let mut pixels = std::mem::take(&mut spare);
        let mut filled = 0;

        while filled < size {
            match stdout.read(&mut pixels[filled..]) {
                Ok(0) => return,
                Ok(read) => filled += read,
                Err(_) => return,
            }
        }

        let mut slot = latest.lock().unwrap();

        if let Some(previous) = slot.take() {
            spare = previous.pixels;
        }

        *slot = Some(Frame {
            width,
            height,
            pixels,
        });

        drop(slot);
        arrived();
    }
}

#[cfg(test)]
mod tests {
    use super::decode_size;

    #[test]
    fn the_cap_keeps_the_aspect_ratio_and_an_even_height() {
        assert_eq!(decode_size((2560, 1600), 1920), (1920, 1200));
        assert_eq!(decode_size((2560, 1600), 1911), (1911, 1194));
    }

    #[test]
    fn nothing_is_scaled_up_or_forced() {
        // A smaller stream is decoded as it is: scaling it up would cost and gain nothing.
        assert_eq!(decode_size((1280, 800), 1920), (1280, 800));
        // Zero means native, which is the switch for a machine that can afford the pipe.
        assert_eq!(decode_size((2560, 1600), 0), (2560, 1600));
    }
}
