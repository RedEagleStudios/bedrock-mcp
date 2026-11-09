//! JSON as the game reads pack files: `//` and `/* */` comments and trailing commas allowed,
//! with the line and column of every key and value kept for diagnostics. serde_json accepts
//! neither comments nor trailing commas and drops positions, so this is a small parser of its
//! own rather than a new dependency.

/// A 1-based line and column; the column counts characters, not bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug)]
pub enum Value {
    Null,
    /// `true` or `false`: no check needs to know which.
    Bool,
    /// The number as written.
    Number(String),
    String(String),
    Array(Vec<Node>),
    Object(Vec<Member>),
}

/// A value and where it starts.
#[derive(Debug)]
pub struct Node {
    pub pos: Pos,
    pub value: Value,
}

/// One `key: value` of an object, in file order; duplicate keys are all kept.
#[derive(Debug)]
pub struct Member {
    pub key: String,
    pub key_pos: Pos,
    pub value: Node,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SyntaxError {
    pub pos: Pos,
    pub message: String,
}

impl Node {
    /// The value of `key`; the last one when the key repeats, as the game reads it.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.members()
            .iter()
            .rev()
            .find(|member| member.key == key)
            .map(|member| &member.value)
    }

    /// The node at a path of keys.
    pub fn at(&self, keys: &[&str]) -> Option<&Node> {
        keys.iter().try_fold(self, |node, key| node.get(key))
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    /// An object's members; empty for anything else.
    pub fn members(&self) -> &[Member] {
        match &self.value {
            Value::Object(members) => members,
            _ => &[],
        }
    }

    /// An array's items; empty for anything else.
    pub fn items(&self) -> &[Node] {
        match &self.value {
            Value::Array(items) => items,
            _ => &[],
        }
    }
}

/// Nesting deeper than this is an error, so a hostile file cannot overflow the stack.
const MAX_DEPTH: usize = 256;

/// Parse one JSON document.
pub fn parse(text: &str) -> Result<Node, SyntaxError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
        pos: Pos { line: 1, column: 1 },
    };
    parser.skip_blank()?;
    let node = parser.value(0)?;
    parser.skip_blank()?;
    if parser.at < parser.bytes.len() {
        return Err(parser.error("unexpected text after the JSON value"));
    }
    Ok(node)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    pos: Pos,
}

impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> SyntaxError {
        SyntaxError {
            pos: self.pos,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Step over one byte, counting a column per character (UTF-8 continuation bytes add none).
    fn bump(&mut self) {
        let Some(&byte) = self.bytes.get(self.at) else {
            return;
        };
        self.at += 1;
        if byte == b'\n' {
            self.pos.line += 1;
            self.pos.column = 1;
        } else if byte & 0xC0 != 0x80 {
            self.pos.column += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), SyntaxError> {
        if self.peek() == Some(byte) {
            self.bump();
            Ok(())
        } else {
            Err(self.error(format!("expected '{}'", byte as char)))
        }
    }

    /// Skip whitespace and comments.
    fn skip_blank(&mut self) -> Result<(), SyntaxError> {
        loop {
            match (self.peek(), self.bytes.get(self.at + 1)) {
                (Some(b' ' | b'\t' | b'\n' | b'\r'), _) => self.bump(),
                (Some(b'/'), Some(b'/')) => {
                    while self.peek().is_some_and(|byte| byte != b'\n') {
                        self.bump();
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    let start = self.pos;
                    self.bump();
                    self.bump();
                    while !(self.peek() == Some(b'*') && self.bytes.get(self.at + 1) == Some(&b'/'))
                    {
                        if self.peek().is_none() {
                            return Err(SyntaxError {
                                pos: start,
                                message: "unclosed /* comment".into(),
                            });
                        }
                        self.bump();
                    }
                    self.bump();
                    self.bump();
                }
                _ => return Ok(()),
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Node, SyntaxError> {
        if depth > MAX_DEPTH {
            return Err(self.error(format!("nested deeper than {MAX_DEPTH} levels")));
        }
        let pos = self.pos;
        let value = match self.peek() {
            Some(b'{') => self.object(depth)?,
            Some(b'[') => self.array(depth)?,
            Some(b'"') => Value::String(self.string()?),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.word("true", Value::Bool)?,
            Some(b'f') => self.word("false", Value::Bool)?,
            Some(b'n') => self.word("null", Value::Null)?,
            Some(_) => return Err(self.error("expected a value")),
            None => return Err(self.error("unexpected end of file")),
        };
        Ok(Node { pos, value })
    }

    fn word(&mut self, word: &str, value: Value) -> Result<Value, SyntaxError> {
        if !self.bytes[self.at..].starts_with(word.as_bytes()) {
            return Err(self.error("expected a value"));
        }
        for _ in 0..word.len() {
            self.bump();
        }
        Ok(value)
    }

    fn number(&mut self) -> Value {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.bump();
        }
        Value::Number(String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned())
    }

    /// Items or members up to `close`, allowing a trailing comma.
    fn list(
        &mut self,
        close: u8,
        mut item: impl FnMut(&mut Self) -> Result<(), SyntaxError>,
    ) -> Result<(), SyntaxError> {
        self.bump();
        loop {
            self.skip_blank()?;
            if self.peek() == Some(close) {
                self.bump();
                return Ok(());
            }
            item(self)?;
            self.skip_blank()?;
            match self.peek() {
                Some(b',') => self.bump(),
                Some(byte) if byte == close => {}
                _ => return Err(self.error(format!("expected ',' or '{}'", close as char))),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, SyntaxError> {
        let mut items = Vec::new();
        self.list(b']', |parser| {
            items.push(parser.value(depth + 1)?);
            Ok(())
        })?;
        Ok(Value::Array(items))
    }

    fn object(&mut self, depth: usize) -> Result<Value, SyntaxError> {
        let mut members = Vec::new();
        self.list(b'}', |parser| {
            let key_pos = parser.pos;
            if parser.peek() != Some(b'"') {
                return Err(parser.error("expected a quoted key"));
            }
            let key = parser.string()?;
            parser.skip_blank()?;
            parser.expect(b':')?;
            parser.skip_blank()?;
            let value = parser.value(depth + 1)?;
            members.push(Member {
                key,
                key_pos,
                value,
            });
            Ok(())
        })?;
        Ok(Value::Object(members))
    }

    fn string(&mut self) -> Result<String, SyntaxError> {
        let start = self.pos;
        self.bump();
        let mut bytes = Vec::new();
        loop {
            match self.peek() {
                None => {
                    return Err(SyntaxError {
                        pos: start,
                        message: "unclosed string".into(),
                    });
                }
                Some(b'"') => {
                    self.bump();
                    return Ok(String::from_utf8_lossy(&bytes).into_owned());
                }
                Some(b'\\') => {
                    self.bump();
                    self.escape(&mut bytes)?;
                }
                Some(byte) => {
                    bytes.push(byte);
                    self.bump();
                }
            }
        }
    }

    /// The character after a backslash, appended to `bytes` as UTF-8.
    fn escape(&mut self, bytes: &mut Vec<u8>) -> Result<(), SyntaxError> {
        let simple = match self.peek() {
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => '\u{8}',
            Some(b'f') => '\u{c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'u') => {
                self.bump();
                let mut code = self.hex4()?;
                // A high surrogate followed by `\uDC00`-`\uDFFF` encodes one character.
                if (0xD800..0xDC00).contains(&code) && self.bytes[self.at..].starts_with(b"\\u") {
                    self.bump();
                    self.bump();
                    let low = self.hex4()?;
                    code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                }
                let character = char::from_u32(code).unwrap_or('\u{fffd}');
                bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
                return Ok(());
            }
            _ => return Err(self.error("invalid escape")),
        };
        self.bump();
        bytes.extend_from_slice(simple.encode_utf8(&mut [0; 4]).as_bytes());
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, SyntaxError> {
        let mut code = 0;
        for _ in 0..4 {
            let digit = self
                .peek()
                .and_then(|byte| (byte as char).to_digit(16))
                .ok_or_else(|| self.error("invalid \\u escape"))?;
            code = code * 16 + digit;
            self.bump();
        }
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, column: u32) -> Pos {
        Pos { line, column }
    }

    #[test]
    fn keeps_positions_of_keys_and_values() {
        let text = "{\n  \"a\": [1, \"x\"],\n  \"é\": {\"b\": true}\n}";
        let root = parse(text).unwrap();
        assert_eq!(root.pos, pos(1, 1));
        let a = &root.members()[0];
        assert_eq!((a.key.as_str(), a.key_pos), ("a", pos(2, 3)));
        assert_eq!(a.value.pos, pos(2, 8));
        assert_eq!(a.value.items()[1].pos, pos(2, 12));
        // Columns count characters, so the multi-byte key shifts nothing after it.
        let b = root.at(&["é", "b"]).unwrap();
        assert_eq!(b.pos, pos(3, 14));
    }

    #[test]
    fn accepts_comments_and_trailing_commas() {
        let text =
            "// header\n{ /* block\n comment */ \"a\": [1, 2,], // after\n \"b\": \"//not\",\n}";
        let root = parse(text).unwrap();
        assert_eq!(root.get("a").unwrap().items().len(), 2);
        assert_eq!(root.get("b").unwrap().as_str(), Some("//not"));
        assert_eq!(root.get("b").unwrap().pos, pos(4, 7));
    }

    #[test]
    fn decodes_escapes_and_a_bom() {
        let root = parse("\u{feff}[\"a\\\"b\\u00e9\\ud83d\\ude00\\n\"]").unwrap();
        assert_eq!(root.items()[0].as_str(), Some("a\"bé😀\n"));
    }

    #[test]
    fn the_last_duplicate_key_wins() {
        let root = parse(r#"{"a": 1, "a": "two"}"#).unwrap();
        assert_eq!(root.get("a").unwrap().as_str(), Some("two"));
        assert_eq!(root.members().len(), 2);
    }

    #[test]
    fn reports_where_syntax_breaks() {
        let error = parse("{\n  \"a\": 1\n  \"b\": 2\n}").unwrap_err();
        assert_eq!(error.pos, pos(3, 3));
        assert!(error.message.contains("expected ','"));
        assert_eq!(parse("{\"a\": \"x").unwrap_err().pos, pos(1, 7));
        assert_eq!(
            parse("[1] 2").unwrap_err().message,
            "unexpected text after the JSON value"
        );
        assert!(parse(&"[".repeat(1000)).is_err());
    }
}
