//! A small JSON reader.
//!
//! The browser pages call `JSON.parse`; the native client has no browser, and the channels it
//! listens to are JSON because that is what those pages expect. So this reads the same text:
//! objects, arrays, strings, numbers, booleans and null, with the standard escapes.
//!
//! It only reads. Nothing on this side ever has to produce JSON, and the messages that are rich
//! enough to need writing (the cursor image, the settings form) are already written by the server.

/// A parsed JSON value.
pub enum Value {
    Null,
    Bool(bool),
    /// All the numbers in these messages are coordinates, frame counts or clock readings, so one
    /// type is enough and there is no need to tell integers from floats.
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// A field of an object, or None for anything else.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(number) => Some(*number),
            _ => None,
        }
    }

    pub fn as_i32(&self) -> Option<i32> {
        self.as_f64().map(|number| number as i32)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// Parses one JSON value. Trailing text after it is ignored, because the callers only ever ask for
/// a message they already know the shape of.
pub fn parse(text: &str) -> Option<Value> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
        depth: 0,
    };

    reader.value()
}

/// Nesting deeper than this is not something the server sends, and refusing it keeps a broken
/// stream from turning into a stack overflow.
const MAX_DEPTH: usize = 32;

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn value(&mut self) -> Option<Value> {
        self.skip_space();

        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Value::String),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'n' => self.literal("null", Value::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Option<Value> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Some(value)
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<Value> {
        self.enter()?;
        self.at += 1; // '{'

        let mut fields = Vec::new();
        self.skip_space();

        if self.peek() == Some(b'}') {
            self.at += 1;
            self.depth -= 1;
            return Some(Value::Object(fields));
        }

        loop {
            self.skip_space();
            let name = self.string()?;

            self.skip_space();

            if self.peek() != Some(b':') {
                return None;
            }

            self.at += 1;
            fields.push((name, self.value()?));

            self.skip_space();

            match self.peek()? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    self.depth -= 1;
                    return Some(Value::Object(fields));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Value> {
        self.enter()?;
        self.at += 1; // '['

        let mut items = Vec::new();
        self.skip_space();

        if self.peek() == Some(b']') {
            self.at += 1;
            self.depth -= 1;
            return Some(Value::Array(items));
        }

        loop {
            items.push(self.value()?);

            self.skip_space();

            match self.peek()? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    self.depth -= 1;
                    return Some(Value::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;

        if self.depth > MAX_DEPTH {
            return None;
        }

        Some(())
    }

    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }

        self.at += 1;

        let mut text = String::new();

        loop {
            let byte = self.peek()?;
            self.at += 1;

            match byte {
                b'"' => return Some(text),
                b'\\' => {
                    let escape = self.peek()?;
                    self.at += 1;

                    match escape {
                        b'"' => text.push('"'),
                        b'\\' => text.push('\\'),
                        b'/' => text.push('/'),
                        b'b' => text.push('\u{8}'),
                        b'f' => text.push('\u{c}'),
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'u' => text.push(self.unicode_escape()?),
                        _ => return None,
                    }
                }
                // Anything else is UTF-8 that can be taken as it comes, one byte at a time: the
                // result is still valid because the input was.
                other => text.push(other as char),
            }
        }
    }

    fn unicode_escape(&mut self) -> Option<char> {
        let unit = self.hex4()?;

        // Surrogate pairs arrive as two escapes; anything else is one code unit.
        if (0xD800..0xDC00).contains(&unit) {
            if self.peek()? != b'\\' {
                return None;
            }

            self.at += 1;

            if self.peek()? != b'u' {
                return None;
            }

            self.at += 1;

            let low = self.hex4()?;
            let combined = 0x1_0000 + ((unit - 0xD800) << 10) + (low - 0xDC00);

            return char::from_u32(combined);
        }

        char::from_u32(unit)
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut value = 0u32;

        for _ in 0..4 {
            let digit = (self.peek()? as char).to_digit(16)?;
            self.at += 1;
            value = value * 16 + digit;
        }

        Some(value)
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.at;

        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
        ) {
            self.at += 1;
        }

        if self.at == start {
            return None;
        }

        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()?
            .parse::<f64>()
            .ok()
            .map(Value::Number)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Value};

    #[test]
    fn reads_a_cursor_position() {
        let value = parse("{\"cursor\":[1150,1022]}").unwrap();
        let cursor = value.get("cursor").and_then(Value::as_array).unwrap();

        assert_eq!(cursor.len(), 2);
        assert_eq!(cursor[0].as_i32(), Some(1150));
        assert_eq!(cursor[1].as_i32(), Some(1022));
    }

    #[test]
    fn tells_null_apart_from_a_missing_field() {
        // The pointer being hidden and the pointer never having been sent mean different things
        // to the window, so this distinction is load bearing.
        let hidden = parse("{\"cursor\":null}").unwrap();
        assert!(hidden.get("cursor").unwrap().is_null());

        let absent = parse("{\"control\":true,\"controller\":null}").unwrap();
        assert!(absent.get("cursor").is_none());
        assert_eq!(absent.get("control").and_then(Value::as_bool), Some(true));
        assert!(absent.get("controller").unwrap().is_null());
    }

    #[test]
    fn reads_nested_objects_and_strings_with_escapes() {
        let value = parse(
            "{\"h264\":{\"codec\":\"avc1.640033\",\"width\":2560,\"height\":1600,\"fps\":60}}",
        )
        .unwrap();
        let h264 = value.get("h264").unwrap();

        assert_eq!(h264.get("codec").and_then(Value::as_str), Some("avc1.640033"));
        assert_eq!(h264.get("width").and_then(Value::as_i32), Some(2560));

        let escaped = parse("{\"note\":\"a\\\"b\\\\c\\u4e2d\"}").unwrap();
        assert_eq!(
            escaped.get("note").and_then(Value::as_str),
            Some("a\"b\\c中")
        );
    }

    #[test]
    fn refuses_what_it_cannot_trust() {
        assert!(parse("").is_none());
        assert!(parse("{\"cursor\":[1,").is_none());
        assert!(parse("{\"cursor\":[1,2}").is_none());
        assert!(parse("not json").is_none());
        // Deep nesting is not something the server sends.
        assert!(parse(&"[".repeat(64)).is_none());
    }
}
