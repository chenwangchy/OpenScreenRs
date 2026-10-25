//! The MJPEG wire format.
//!
//! A browser renders this by pointing an `<img src>` at the URL: the response is an endless
//! multipart body and every part is one JPEG frame. The line endings and the boundary name
//! matter here, a mismatch makes the browser show nothing at all.

/// Response headers. Caching has to be off or browsers keep showing the first frame.
pub const RESPONSE_HEADERS: &str = concat!(
    "HTTP/1.1 200 OK\r\n",
    "Content-Type: multipart/x-mixed-replace; boundary=boundary\r\n",
    "Cache-Control: no-store, no-cache, must-revalidate, private\r\n",
    "Pragma: no-cache\r\n",
    "Connection: close\r\n",
    "\r\n"
);

/// One frame: the part header, the JPEG itself, then a blank line.
pub fn frame_header(length: usize) -> String {
    format!("--boundary\r\nContent-Type: image/jpeg\r\nContent-Length: {length}\r\n\r\n")
}

pub const FRAME_TRAILER: &[u8] = b"\r\n";
