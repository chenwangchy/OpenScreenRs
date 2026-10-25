//! Splits the fragmented MP4 that ffmpeg writes into the two things MediaSource wants.
//!
//! MediaSource cannot swallow a raw byte stream: it needs an init segment (the track
//! description) once, and then whole media fragments, appended one at a time. ffmpeg emits those
//! as ordinary MP4 boxes on stdout, so this is a small box reader and a state machine.
//!
//! With `frag_keyframe` every fragment starts with a key frame, which means fragments can be
//! dropped when a viewer falls behind without breaking the ones that follow.

/// What the player has to append next.
pub enum Chunk {
    /// `ftyp` plus `moov`: everything needed before the first frame.
    Init(Vec<u8>),
    /// `moof` plus `mdat`: one fragment, self contained.
    Fragment(Vec<u8>),
}

pub struct Splitter {
    buffer: Vec<u8>,
    init: Vec<u8>,
    fragment: Option<Vec<u8>>,
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
            init: Vec::new(),
            fragment: None,
        }
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<Chunk> {
        self.buffer.extend_from_slice(data);
        let mut chunks = Vec::new();
        let mut consumed = 0;

        while let Some((size, kind)) = box_header(&self.buffer[consumed..]) {
            if self.buffer.len() - consumed < size {
                break;
            }

            let body = self.buffer[consumed..consumed + size].to_vec();
            consumed += size;

            match &kind {
                b"ftyp" | b"moov" => {
                    self.init.extend_from_slice(&body);

                    // The track description is complete once the moov has been read.
                    if &kind == b"moov" && !self.init.is_empty() {
                        chunks.push(Chunk::Init(std::mem::take(&mut self.init)));
                    }
                }
                b"moof" => {
                    // A fragment starts with its movie fragment header.
                    self.fragment = Some(body);
                }
                b"mdat" => {
                    if let Some(mut fragment) = self.fragment.take() {
                        fragment.extend_from_slice(&body);
                        chunks.push(Chunk::Fragment(fragment));
                    }
                }
                // sidx, styp, free and anything else is not needed by MediaSource here.
                _ => {}
            }
        }

        if consumed > 0 {
            self.buffer.drain(..consumed);
        }

        chunks
    }
}

/// True when this fragment carries a key frame.
///
/// Fragments cut by time are not key frame aligned, so a viewer that joins, or one that had to
/// throw its buffer away, can only start again at a fragment that begins with an IDR slice. The
/// mdat holds length prefixed NAL units, which is all this needs: a NAL with type 5 is one.
pub fn contains_keyframe(fragment: &[u8]) -> bool {
    let mut offset = 0;
    let mut payload: Option<&[u8]> = None;

    while offset + 8 <= fragment.len() {
        let size =
            u32::from_be_bytes([fragment[offset], fragment[offset + 1], fragment[offset + 2], fragment[offset + 3]])
                as usize;

        if size < 8 || offset + size > fragment.len() {
            break;
        }

        if &fragment[offset + 4..offset + 8] == b"mdat" {
            payload = Some(&fragment[offset + 8..offset + size]);
            break;
        }

        offset += size;
    }

    let Some(bytes) = payload else {
        return false;
    };

    let mut cursor = 0;

    while cursor + 5 <= bytes.len() {
        let length = u32::from_be_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]) as usize;

        if length == 0 || cursor + 4 + length > bytes.len() {
            break;
        }

        if bytes[cursor + 4] & 0x1f == 5 {
            return true;
        }

        cursor += 4 + length;
    }

    false
}

/// Reads a box header: a big endian 32 bit size followed by a four character type.
fn box_header(data: &[u8]) -> Option<(usize, [u8; 4])> {
    if data.len() < 8 {
        return None;
    }

    let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&data[4..8]);

    // A size of 1 means a 64 bit size follows, 0 means "to the end"; neither appears in what
    // ffmpeg streams here, so treat both as a truncated stream instead of guessing.
    if size < 8 {
        return None;
    }

    Some((size, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_of(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (payload.len() + 8) as u32;
        let mut bytes = size.to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn sample_stream() -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend(box_of(b"ftyp", b"isomiso5"));
        stream.extend(box_of(b"moov", b"track description here"));
        stream.extend(box_of(b"moof", b"fragment header"));
        stream.extend(box_of(b"mdat", b"frame data"));
        stream.extend(box_of(b"moof", b"fragment header 2"));
        stream.extend(box_of(b"mdat", b"frame data 2"));
        stream
    }

    #[test]
    fn separates_the_init_segment_from_fragments() {
        let mut splitter = Splitter::new();
        let chunks = splitter.push(&sample_stream());

        assert_eq!(chunks.len(), 3, "one init segment and two fragments");

        match &chunks[0] {
            Chunk::Init(bytes) => {
                assert_eq!(&bytes[4..8], b"ftyp", "the init segment starts with ftyp");
                assert!(bytes.len() > 20);
            }
            Chunk::Fragment(_) => panic!("first chunk should be the init segment"),
        }

        for chunk in &chunks[1..] {
            match chunk {
                Chunk::Fragment(bytes) => assert!(bytes.len() > 8),
                Chunk::Init(_) => panic!("fragments must follow the init segment"),
            }
        }
    }

    #[test]
    fn handles_boxes_arriving_in_pieces() {
        let stream = sample_stream();
        let mut splitter = Splitter::new();
        let mut chunks = Vec::new();

        // One byte at a time: the worst case for a box reader.
        for byte in &stream {
            chunks.extend(splitter.push(std::slice::from_ref(byte)));
        }

        assert_eq!(chunks.len(), 3);
        assert!(matches!(chunks[0], Chunk::Init(_)));
    }

    #[test]
    fn waits_for_a_complete_mdat() {
        let mut stream = Vec::new();
        stream.extend(box_of(b"ftyp", b"isom"));
        stream.extend(box_of(b"moov", b"track"));
        stream.extend(box_of(b"moof", b"header"));

        let mut splitter = Splitter::new();
        let chunks = splitter.push(&stream);

        // The fragment has no payload yet, so only the init segment comes out.
        assert_eq!(chunks.len(), 1);

        let tail = box_of(b"mdat", b"payload");
        let chunks = splitter.push(&tail);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], Chunk::Fragment(_)));
    }
}

#[cfg(test)]
mod keyframe_tests {
    use super::*;

    fn fragment_with(mdat_payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        // a moof box, contents irrelevant here
        bytes.extend_from_slice(&12u32.to_be_bytes());
        bytes.extend_from_slice(b"moof");
        bytes.extend_from_slice(b"xxxx");
        // the mdat box
        bytes.extend_from_slice(&((mdat_payload.len() + 8) as u32).to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(mdat_payload);
        bytes
    }

    fn nal(nal_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
        bytes.push(nal_type);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn finds_an_idr_slice() {
        let mut payload = nal(1, &[0x80, 0x11]);  // an ordinary slice first
        payload.extend(nal(5, &[0x80, 0x22]));    // then an IDR
        assert!(contains_keyframe(&fragment_with(&payload)));
    }

    #[test]
    fn reports_no_keyframe_for_predicted_frames() {
        let mut payload = nal(1, &[0x80, 0x11]);
        payload.extend(nal(1, &[0x80, 0x22]));
        assert!(!contains_keyframe(&fragment_with(&payload)));
    }

    #[test]
    fn survives_a_truncated_fragment() {
        // A half written fragment must not be mistaken for a key frame.
        let mut payload = nal(5, &[0x80, 0x22]);
        payload.truncate(3);
        assert!(!contains_keyframe(&fragment_with(&payload)));
    }
}