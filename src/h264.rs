//! Turns the raw H.264 byte stream that the encoder writes into access units.
//!
//! The wire format fed to the browser is Annex-B (start codes 00 00 01). One access unit per
//! WebSocket message, because that is the granularity `EncodedVideoChunk` wants. Boundaries come
//! from access unit delimiters (the encoder is asked to insert them) with a fallback for streams
//! that have none.

/// One access unit: everything the decoder needs to produce a frame.
#[derive(Clone)]
pub struct AccessUnit {
    /// Annex-B bytes, start codes included.
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

pub struct Splitter {
    buffer: Vec<u8>,
    /// Offset the next start code search begins at, so each byte is scanned once.
    scanned: usize,
    pending: Vec<u8>,
    pending_has_slice: bool,
    pending_is_keyframe: bool,
}

impl Default for Splitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Splitter {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(256 * 1024),
            scanned: 0,
            pending: Vec::with_capacity(32 * 1024),
            pending_has_slice: false,
            pending_is_keyframe: false,
        }
    }

    /// Feeds a chunk of the stream and returns the access units that are complete.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AccessUnit> {
        self.buffer.extend_from_slice(chunk);
        let mut units = Vec::new();

        loop {
            let Some(start) = find_start_code(&self.buffer, self.scanned) else {
                break;
            };

            // A NAL unit runs until the next start code, so it is only complete once we can see
            // the next one.
            let Some(next) = find_start_code(&self.buffer, start + 3) else {
                self.scanned = start;
                break;
            };

            // Copied out so the splitter stays borrowable; a NAL is small next to a frame.
            let nal = self.buffer[start..next].to_vec();
            self.push_nal(&nal, &mut units);
            self.scanned = next;
        }

        // Drop everything already turned into NAL units, keeping the unfinished tail.
        if self.scanned > 0 {
            self.buffer.drain(..self.scanned);
            self.scanned = 0;
        }

        units
    }

    fn push_nal(&mut self, nal: &[u8], units: &mut Vec<AccessUnit>) {
        // Slices always start at a three byte start code, so the header follows it directly.
        if nal.len() < 4 {
            return;
        }

        let nal_type = nal[3] & 0x1f;

        // A delimiter, or a slice that begins a picture, closes the unit in progress. Nothing
        // opens one before its first slice, otherwise the parameter sets would land in an
        // access unit of their own.
        if self.pending_has_slice && (nal_type == 9 || is_first_slice(nal)) {
            self.flush(units);
        }

        self.pending.extend_from_slice(nal);

        if matches!(nal_type, 1 | 2 | 5) {
            self.pending_has_slice = true;
        }

        if nal_type == 5 {
            self.pending_is_keyframe = true;
        }
    }

    fn flush(&mut self, units: &mut Vec<AccessUnit>) {
        if self.pending.len() <= 4 {
            self.pending.clear();
            self.pending_has_slice = false;
            self.pending_is_keyframe = false;
            return;
        }

        units.push(AccessUnit {
            data: std::mem::take(&mut self.pending),
            is_keyframe: self.pending_is_keyframe,
        });

        self.pending_has_slice = false;
        self.pending_is_keyframe = false;
    }

    /// Ends the stream: the trailing NAL never saw a following start code, so it is turned
    /// into a NAL unit here, and whatever is still pending is flushed.
    pub fn finish(&mut self) -> Vec<AccessUnit> {
        let mut units = Vec::new();

        if self.buffer.len() > self.scanned {
            let tail = self.buffer[self.scanned..].to_vec();

            if tail.len() >= 4 && find_start_code(&tail, 0).is_some() {
                self.push_nal(&tail, &mut units);
            }
        }

        self.flush(&mut units);
        units
    }
}

/// Finds the next Annex-B start code at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut index = from;

    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            return Some(index);
        }

        index += 1;
    }

    None
}

/// True when this NAL begins a new picture, the fallback boundary for streams without AUDs.
///
/// `first_mb_in_slice` is the first value in the slice header, encoded as an unsigned
/// Exp-Golomb code: a picture starts when it is zero.
fn is_first_slice(nal: &[u8]) -> bool {
    let nal_type = nal[3] & 0x1f;

    if !matches!(nal_type, 1 | 2 | 5) || nal.len() < 5 {
        return false;
    }

    // Skip the start code (3 or 4 bytes) and the NAL header byte.
    let payload_start = if nal.starts_with(&[0, 0, 0, 1]) { 5 } else { 4 };

    if nal.len() <= payload_start {
        return false;
    }

    let first = nal[payload_start];

    // ue(v) of zero is a single '1' bit, i.e. the most significant bit of the byte.
    first & 0x80 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(nal_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 1, nal_type];
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn splits_on_access_unit_delimiters() {
        let mut stream = Vec::new();
        // AUD, SPS, PPS, IDR slice
        stream.extend(nal(9, &[0xf0]));
        stream.extend(nal(7, &[1, 2, 3]));
        stream.extend(nal(8, &[4, 5]));
        stream.extend(nal(5, &[0x80, 0x11]));
        // AUD, P slice
        stream.extend(nal(9, &[0xf0]));
        stream.extend(nal(1, &[0x80, 0x22]));

        let mut splitter = Splitter::new();
        let mut units = splitter.push(&stream);
        // The last unit only ends when the stream does, or when another one begins.
        units.extend(splitter.finish());

        assert_eq!(units.len(), 2, "expected two access units");
        assert!(units[0].is_keyframe, "first unit holds the IDR slice");
        assert!(!units[1].is_keyframe, "second unit is a predicted frame");
        assert!(units[0].data.len() > units[1].data.len());
    }

    #[test]
    fn copes_with_chunk_boundaries_inside_start_codes() {
        let mut stream = Vec::new();
        stream.extend(nal(9, &[0xf0]));
        stream.extend(nal(5, &[0x80, 0x11]));
        stream.extend(nal(9, &[0xf0]));
        stream.extend(nal(5, &[0x80, 0x11]));

        // Feed one byte at a time: worst case for a stream parser.
        let mut splitter = Splitter::new();
        let mut units = Vec::new();

        for byte in &stream {
            units.extend(splitter.push(std::slice::from_ref(byte)));
        }
        units.extend(splitter.finish());

        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|unit| unit.is_keyframe));
    }

    #[test]
    fn falls_back_to_slice_headers_without_delimiters() {
        // No AUDs at all: a new picture is detected by first_mb_in_slice == 0 (0x80).
        let mut stream = Vec::new();
        stream.extend(nal(7, &[1, 2, 3]));
        stream.extend(nal(8, &[4, 5]));
        stream.extend(nal(5, &[0x80, 0x11]));
        stream.extend(nal(1, &[0x80, 0x22]));

        let mut splitter = Splitter::new();
        let mut units = splitter.push(&stream);
        units.extend(splitter.finish());

        assert_eq!(units.len(), 2);
        assert!(units[0].is_keyframe);
    }
}
