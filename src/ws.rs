//! The little bit of WebSocket the cursor overlay needs: the opening handshake and text frames.
//!
//! Hand writing it keeps the crate dependency free. SHA-1 and base64 are here because the
//! handshake answer is `base64(sha1(key + guid))`, and neither is in the standard library.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Fixed GUID from RFC 6455, appended to the client key before hashing.
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Computes the `Sec-WebSocket-Accept` value for a client key.
pub fn accept_key(key: &str) -> String {
    let mut input = String::with_capacity(key.len() + WEBSOCKET_GUID.len());
    input.push_str(key.trim());
    input.push_str(WEBSOCKET_GUID);

    base64(&sha1(input.as_bytes()))
}

/// Answers the upgrade request. Returns false when the request is not a WebSocket handshake.
pub fn handshake(stream: &mut TcpStream, request: &str) -> std::io::Result<bool> {
    let key = match header(request, "sec-websocket-key") {
        Some(key) => key,
        None => return Ok(false),
    };

    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(&key)
    );

    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(true)
}

/// Sends one text frame.
pub fn send_text(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    send_frame(stream, 0x1, payload)
}

/// Sends one binary frame. Used for the H.264 access units.
pub fn send_binary(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    send_frame(stream, 0x2, payload)
}

/// Server to client frames are never masked.
fn send_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode); // FIN + opcode

    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }

    frame.extend_from_slice(payload);
    stream.write_all(&frame)?;
    stream.flush()
}

/// Reads the request headers, up to and including the blank line.
pub fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];

    loop {
        let read = stream.read(&mut chunk)?;

        if read == 0 {
            break;
        }

        buffer.extend_from_slice(&chunk[..read]);

        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() > 16 * 1024 {
            break;
        }
    }

    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Decodes a text frame sent by a client (client frames are always masked), if that is what this
/// is. Answer SDPs are a couple of kilobytes, so both length forms are supported.
pub fn text_payload(frame: &[u8]) -> Option<String> {
    if frame.len() < 6 || frame[0] & 0x0f != 0x1 {
        return None;
    }

    let masked = frame[1] & 0x80 != 0;

    if !masked {
        return None;
    }

    let (length, header) = match frame[1] & 0x7f {
        126 => {
            if frame.len() < 8 {
                return None;
            }

            (u16::from_be_bytes([frame[2], frame[3]]) as usize, 4)
        }
        127 => return None,
        short => (short as usize, 2),
    };

    if frame.len() < header + 4 + length {
        return None;
    }

    let mask = &frame[header..header + 4];
    let body: Vec<u8> = frame[header + 4..header + 4 + length]
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask[index % 4])
        .collect();

    String::from_utf8(body).ok()
}

/// A WebSocket ping. Browsers answer it with a pong without any script involved.
pub fn send_ping(socket: &mut TcpStream) -> std::io::Result<()> {
    // FIN + opcode 9, no payload: two bytes is a complete control frame.
    socket.write_all(&[0x89, 0x00])?;
    socket.flush()
}

/// Waits for the client to go away, noticing the ones that never say so.
///
/// A viewer that loses WiFi never closes its socket: the read blocks forever, the connection
/// leaks a thread and the encoder keeps running for nobody. Pings fix that, because every
/// browser answers them automatically; three unanswered ones mean the client is gone.
pub fn watch<F: FnMut(&[u8])>(socket: &mut TcpStream, mut on_data: F) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));

    let mut pending: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 16 * 1024];
    let mut quiet = 0;

    loop {
        match std::io::Read::read(socket, &mut scratch) {
            Ok(0) => return,
            Ok(read) => {
                quiet = 0;
                pending.extend_from_slice(&scratch[..read]);

                // Hand over whole frames only. An answer SDP is a couple of kilobytes and TCP is
                // free to deliver it in pieces, so a partially received frame must wait.
                while let Some(total) = frame_length(&pending) {
                    if pending.len() < total {
                        break;
                    }

                    let frame: Vec<u8> = pending.drain(..total).collect();
                    on_data(&frame);
                }

                if pending.len() > 1024 * 1024 {
                    // Nothing a viewer sends is this big; do not let it grow without bound.
                    return;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                quiet += 1;

                if quiet >= 3 || send_ping(socket).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Total length of the first frame in `buffer`, once its header is complete.
fn frame_length(buffer: &[u8]) -> Option<usize> {
    if buffer.len() < 2 {
        return None;
    }

    let masked = buffer[1] & 0x80 != 0;
    let mut total = 2;
    let length = match buffer[1] & 0x7f {
        126 => {
            if buffer.len() < 4 {
                return None;
            }

            total += 2;
            u16::from_be_bytes([buffer[2], buffer[3]]) as usize
        }
        // 64 bit lengths are not something a viewer sends; wait for more instead of guessing.
        127 => return None,
        short => short as usize,
    };

    if masked {
        total += 4;
    }

    Some(total + length)
}

/// The body of a request: whatever already followed the head, plus enough bytes to reach the
/// declared content length.
pub fn body(stream: &mut TcpStream, request: &str) -> Option<String> {
    let head_end = request.find("\r\n\r\n")? + 4;
    let mut body = request[head_end..].to_string();

    let length: usize = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;

            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    let mut chunk = [0u8; 2048];

    while body.len() < length {
        let read = std::io::Read::read(stream, &mut chunk).ok()?;

        if read == 0 {
            break;
        }

        body.push_str(&String::from_utf8_lossy(&chunk[..read]));
    }

    Some(body)
}

/// Case insensitive header lookup from a raw request.
pub fn header(request: &str, name: &str) -> Option<String> {
    for line in request.lines() {
        let mut parts = line.splitn(2, ':');

        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            if key.trim().eq_ignore_ascii_case(name) {
                return Some(value.trim().to_string());
            }
        }
    }

    None
}

/// The request path from `GET /path HTTP/1.1`.
pub fn path(request: &str) -> String {
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|path| path.split('?').next().unwrap_or(path).to_string())
        .unwrap_or_else(|| "/".to_string())
}

/// SHA-1, needed only for the handshake.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];

    let length_bits = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);

    while message.len() % 64 != 56 {
        message.push(0);
    }

    message.extend_from_slice(&length_bits.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 80];

        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }

        for index in 16..80 {
            words[index] = (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                .rotate_left(1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];

        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDCu32),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };

            let temporary = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);

            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temporary;
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut digest = [0u8; 20];

    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }

    digest
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let first = chunk[0] as u32;
        let second = *chunk.get(1).unwrap_or(&0) as u32;
        let third = *chunk.get(2).unwrap_or(&0) as u32;
        let combined = (first << 16) | (second << 8) | third;

        output.push(ALPHABET[(combined >> 18) as usize & 63] as char);
        output.push(ALPHABET[(combined >> 12) as usize & 63] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(combined >> 6) as usize & 63] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[combined as usize & 63] as char
        } else {
            '='
        });
    }

    output
}

/// One message from a server.
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
}

/// A WebSocket client: the handshake out, masked frames out, server frames in.
///
/// It exists because the native viewer listens to the same two channels the browser pages use, and
/// those are WebSocket. This is the other half of the functions above, not a second protocol.
pub struct Client {
    stream: TcpStream,
    /// Bytes of frames that have started to arrive.
    pending: Vec<u8>,
    /// Masking keys. Masking is not security, it only stops a cache from mistaking a frame for an
    /// HTTP request, so a shifting value is enough.
    noise: u32,
}

impl Client {
    /// Connects and upgrades. `address` is `host:port`, `path` includes the leading slash.
    pub fn connect(address: &str, path: &str) -> std::io::Result<Self> {
        let mut stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;

        let key = base64(&nonce());
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {address}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );

        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let response = read_request(&mut stream)?;

        if !response.starts_with("HTTP/1.1 101") {
            return Err(std::io::Error::other(format!(
                "not a WebSocket: {}",
                response.lines().next().unwrap_or("no answer")
            )));
        }

        match header(&response, "sec-websocket-accept") {
            Some(answer) if answer == accept_key(&key) => {}
            _ => return Err(std::io::Error::other("the server answered the wrong accept key")),
        }

        Ok(Self {
            stream,
            pending: Vec::new(),
            noise: seed(),
        })
    }

    /// Bounds a blocking read, which is what lets one thread also drain a queue of input.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    pub fn send_text(&mut self, payload: &[u8]) -> std::io::Result<()> {
        self.send(0x1, payload)
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        self.noise ^= self.noise << 13;
        self.noise ^= self.noise >> 17;
        self.noise ^= self.noise << 5;

        self.stream
            .write_all(&masked_frame(opcode, payload, self.noise.to_le_bytes()))?;
        self.stream.flush()
    }

    /// The next message, answering pings on the way.
    pub fn read(&mut self) -> std::io::Result<Message> {
        loop {
            if let Some(message) = self.take_message()? {
                return Ok(message);
            }

            let mut scratch = [0u8; 64 * 1024];
            let read = std::io::Read::read(&mut self.stream, &mut scratch)?;

            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }

            self.pending.extend_from_slice(&scratch[..read]);
        }
    }

    fn take_message(&mut self) -> std::io::Result<Option<Message>> {
        loop {
            let Some((opcode, payload)) = take_frame(&mut self.pending) else {
                return Ok(None);
            };

            match opcode {
                0x1 => {
                    return Ok(Some(Message::Text(
                        String::from_utf8_lossy(&payload).into_owned(),
                    )))
                }
                0x2 => return Ok(Some(Message::Binary(payload))),
                0x8 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                // A ping must be answered: the host drops a viewer after three unanswered ones,
                // and it counts ten seconds of silence each.
                0x9 => self.send(0xA, &payload)?,
                _ => {}
            }
        }
    }
}

/// One masked frame, the only shape a client is allowed to send.
fn masked_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);

    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }

    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );

    frame
}

/// Splits the first whole frame off a buffer of bytes from a server, which are never masked.
fn take_frame(buffer: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if buffer.len() < 2 {
        return None;
    }

    let opcode = buffer[0] & 0x0f;

    let (length, header) = match buffer[1] & 0x7f {
        126 => {
            if buffer.len() < 4 {
                return None;
            }

            (u16::from_be_bytes([buffer[2], buffer[3]]) as usize, 4)
        }
        127 => {
            if buffer.len() < 10 {
                return None;
            }

            let mut size = [0u8; 8];
            size.copy_from_slice(&buffer[2..10]);

            (usize::try_from(u64::from_be_bytes(size)).ok()?, 10)
        }
        short => (short as usize, 2),
    };

    if buffer.len() < header + length {
        return None;
    }

    let payload = buffer[header..header + length].to_vec();
    buffer.drain(..header + length);

    Some((opcode, payload))
}

/// Sixteen bytes for `Sec-WebSocket-Key`: it only has to be unpredictable enough that two clients
/// do not pick the same key.
fn nonce() -> [u8; 16] {
    let mut state = seed();
    let mut bytes = [0u8; 16];

    for chunk in bytes.chunks_mut(4) {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        chunk.copy_from_slice(&state.to_le_bytes());
    }

    bytes
}

/// A starting value that differs between runs, for the mask and the handshake key.
fn seed() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() ^ elapsed.as_secs() as u32)
        .unwrap_or(0x811C_9DC5);

    // xorshift32 never leaves zero, so the seed must not be it either.
    (nanos ^ std::process::id()).max(1)
}

#[cfg(test)]
mod tests {
    use super::{frame_length, masked_frame, take_frame, text_payload};

    /// A masked text frame of `payload` bytes, the shape a browser sends an answer in.
    fn masked_text(payload: usize) -> Vec<u8> {
        let mut frame = vec![0x81, 0x80 | 126];
        frame.extend_from_slice(&(payload as u16).to_be_bytes());
        frame.extend_from_slice(&[1, 2, 3, 4]);
        frame.extend(std::iter::repeat(0u8).take(payload));
        frame
    }

    #[test]
    fn a_client_frame_is_masked_and_reads_back() {
        // The browser path and the native client path share one parser: a frame built by the
        // client must come out of `text_payload` unchanged.
        let frame = masked_frame(0x1, b"m 5000 5000", [0x11, 0x22, 0x33, 0x44]);

        assert_eq!(frame[1] & 0x80, 0x80, "a client frame must be masked");
        assert_eq!(text_payload(&frame).as_deref(), Some("m 5000 5000"));
        assert_eq!(frame_length(&frame), Some(frame.len()));
    }

    #[test]
    fn a_server_frame_is_split_off_whole() {
        let mut buffer = vec![0x82, 3, 1, 2, 3];

        // A 600 byte binary frame: long enough to use the 16 bit length, still unmasked.
        let mut long = vec![0x82, 0x80 | 126];
        long.extend_from_slice(&600u16.to_be_bytes());
        long.extend(std::iter::repeat(7u8).take(600));
        buffer.extend_from_slice(&long);

        assert_eq!(take_frame(&mut buffer), Some((0x2, vec![1, 2, 3])));
        assert_eq!(
            take_frame(&mut buffer).map(|(opcode, payload)| (opcode, payload.len())),
            Some((0x2, 600))
        );

        assert_eq!(take_frame(&mut buffer), None);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_control_frame_is_surfaced_for_the_ping_answer() {
        let mut buffer = vec![0x89, 0x00];

        assert_eq!(take_frame(&mut buffer), Some((0x9, Vec::new())));
        assert_eq!(take_frame(&mut buffer), None);
    }

    #[test]
    fn frame_length_counts_the_mask_and_the_extended_length() {
        // 2 header + 2 extended length + 4 mask + payload
        assert_eq!(frame_length(&masked_text(2000)), Some(2008));
    }

    #[test]
    fn frame_length_waits_for_a_complete_header() {
        assert_eq!(frame_length(&[]), None);
        assert_eq!(frame_length(&[0x81]), None);
        // extended length announced but not delivered yet
        assert_eq!(frame_length(&[0x81, 0xfe]), None);
        assert_eq!(frame_length(&[0x81, 0xfe, 0x07]), None);
    }

    #[test]
    fn an_answer_split_over_two_reads_is_one_frame() {
        let frame = masked_text(1500);

        // The delivery boundary lands in the middle of the header, which is where a naive reader
        // gives up and the viewer's answer is lost.
        assert_eq!(frame_length(&frame[..3]), None);

        // Once the header is there the length is known, and the watcher keeps reading until that
        // many bytes have arrived: 2 header + 2 extended length + 4 mask + 1500 payload.
        assert_eq!(frame_length(&frame[..1500]), Some(1508));
        assert_eq!(frame.len(), 1508);
    }
}