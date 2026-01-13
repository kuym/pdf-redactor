//! PDF object model, lexer, parser and serializer.
//!
//! The lexer is shared between file-level objects and content streams, and
//! reports the byte range of every token so callers can edit in place.

use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrFmt {
    Literal,
    Hex,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Object {
    Null,
    Bool(bool),
    Int(i64),
    /// Real numbers keep their source text so re-serializing never changes precision.
    Real(Vec<u8>),
    Name(Vec<u8>),
    Str(Vec<u8>, StrFmt),
    Array(Vec<Object>),
    Dict(Dict),
    Ref(u32, u16),
}

/// Dictionary that preserves key order.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Dict(pub Vec<(Vec<u8>, Object)>);

impl Dict {
    pub fn get(&self, key: &[u8]) -> Option<&Object> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut Object> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn set(&mut self, key: &[u8], value: Object) {
        match self.get_mut(key) {
            Some(v) => *v = value,
            None => self.0.push((key.to_vec(), value)),
        }
    }
    pub fn remove(&mut self, key: &[u8]) -> Option<Object> {
        let i = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(i).1)
    }
    pub fn has(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }
    pub fn name(&self, key: &[u8]) -> Option<&[u8]> {
        self.get(key).and_then(Object::as_name)
    }
}

impl Object {
    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Object::Name(n) => Some(n),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Object::Int(i) => Some(*i),
            Object::Real(_) => self.as_f64().map(|f| f as i64),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Object::Int(i) => Some(*i as f64),
            Object::Real(raw) => parse_real(raw),
            _ => None,
        }
    }
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Object::Dict(d) => Some(d),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Object]> {
        match self {
            Object::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&[u8]> {
        match self {
            Object::Str(s, _) => Some(s),
            _ => None,
        }
    }
    pub fn as_ref(&self) -> Option<u32> {
        match self {
            Object::Ref(n, _) => Some(*n),
            _ => None,
        }
    }
}

pub fn parse_real(raw: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(raw).ok()?;
    if let Ok(v) = s.parse::<f64>() {
        return v.is_finite().then_some(v);
    }
    // Tolerate oddities such as "--5", "1.2.3" or "4.-2" the way most readers do.
    let neg = s.starts_with('-');
    let digits = s.trim_start_matches(['+', '-']);
    let mut out = String::new();
    let mut seen_dot = false;
    for c in digits.chars() {
        match c {
            '0'..='9' => out.push(c),
            '.' if !seen_dot => {
                seen_dot = true;
                out.push(c)
            }
            _ => break,
        }
    }
    let v: f64 = if out.is_empty() || out == "." { return None } else { out.parse().ok()? };
    Some(if neg { -v } else { v })
}

pub fn is_whitespace(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}
pub fn is_delimiter(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}
pub fn is_regular(b: u8) -> bool {
    !is_whitespace(b) && !is_delimiter(b)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Token<'a> {
    Int(i64),
    Real(&'a [u8]),
    Name(Vec<u8>),
    Str(Vec<u8>, StrFmt),
    ArrayStart,
    ArrayEnd,
    DictStart,
    DictEnd,
    Keyword(&'a [u8]),
    Eof,
}

pub struct Lexer<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl<'a> Lexer<'a> {
    pub fn new(buf: &'a [u8], pos: usize) -> Self {
        Lexer { buf, pos }
    }

    pub fn skip_ws(&mut self) {
        while self.pos < self.buf.len() {
            let b = self.buf[self.pos];
            if is_whitespace(b) {
                self.pos += 1;
            } else if b == b'%' {
                while self.pos < self.buf.len() && !matches!(self.buf[self.pos], b'\n' | b'\r') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    /// Returns the next token together with its byte range `[start, end)`.
    pub fn next(&mut self) -> (Token<'a>, usize, usize) {
        self.skip_ws();
        let start = self.pos;
        let buf = self.buf;
        if start >= buf.len() {
            return (Token::Eof, start, start);
        }
        let tok = match buf[start] {
            b'/' => {
                self.pos += 1;
                let mut name = Vec::new();
                while self.pos < buf.len() && is_regular(buf[self.pos]) {
                    let b = buf[self.pos];
                    if b == b'#' {
                        if let (Some(h), Some(l)) = (
                            buf.get(self.pos + 1).copied().and_then(hex_val),
                            buf.get(self.pos + 2).copied().and_then(hex_val),
                        ) {
                            name.push(h << 4 | l);
                            self.pos += 3;
                            continue;
                        }
                    }
                    name.push(b);
                    self.pos += 1;
                }
                Token::Name(name)
            }
            b'(' => Token::Str(self.literal_string(), StrFmt::Literal),
            b'<' => {
                if buf.get(start + 1) == Some(&b'<') {
                    self.pos += 2;
                    Token::DictStart
                } else {
                    Token::Str(self.hex_string(), StrFmt::Hex)
                }
            }
            b'>' => {
                if buf.get(start + 1) == Some(&b'>') {
                    self.pos += 2;
                    Token::DictEnd
                } else {
                    self.pos += 1;
                    Token::Keyword(&buf[start..start + 1])
                }
            }
            b'[' => {
                self.pos += 1;
                Token::ArrayStart
            }
            b']' => {
                self.pos += 1;
                Token::ArrayEnd
            }
            b'{' | b'}' | b')' => {
                self.pos += 1;
                Token::Keyword(&buf[start..start + 1])
            }
            _ => {
                while self.pos < buf.len() && is_regular(buf[self.pos]) {
                    self.pos += 1;
                }
                let word = &buf[start..self.pos];
                classify_word(word)
            }
        };
        (tok, start, self.pos)
    }

    fn literal_string(&mut self) -> Vec<u8> {
        let buf = self.buf;
        let mut out = Vec::new();
        let mut depth = 1;
        self.pos += 1;
        while self.pos < buf.len() {
            let b = buf[self.pos];
            self.pos += 1;
            match b {
                b'(' => {
                    depth += 1;
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b);
                }
                b'\\' => {
                    let Some(&e) = buf.get(self.pos) else { break };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match buf.get(self.pos) {
                                    Some(&d @ b'0'..=b'7') => {
                                        v = v * 8 + (d - b'0') as u32;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        b'\r' => {
                            if buf.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                b'\r' => {
                    // An unescaped end-of-line inside a string always reads as LF.
                    if buf.get(self.pos) == Some(&b'\n') {
                        self.pos += 1;
                    }
                    out.push(b'\n');
                }
                _ => out.push(b),
            }
        }
        out
    }

    fn hex_string(&mut self) -> Vec<u8> {
        let buf = self.buf;
        let mut out = Vec::new();
        let mut hi: Option<u8> = None;
        self.pos += 1;
        while self.pos < buf.len() {
            let b = buf[self.pos];
            self.pos += 1;
            if b == b'>' {
                break;
            }
            if let Some(v) = hex_val(b) {
                match hi.take() {
                    Some(h) => out.push(h << 4 | v),
                    None => hi = Some(v),
                }
            }
        }
        if let Some(h) = hi {
            out.push(h << 4);
        }
        out
    }
}

fn classify_word(word: &[u8]) -> Token<'_> {
    let numeric = !word.is_empty()
        && word.iter().all(|b| matches!(b, b'0'..=b'9' | b'+' | b'-' | b'.'))
        && word.iter().any(u8::is_ascii_digit);
    if !numeric {
        return Token::Keyword(word);
    }
    if !word.contains(&b'.') {
        if let Some(v) = std::str::from_utf8(word).ok().and_then(|s| s.parse::<i64>().ok()) {
            return Token::Int(v);
        }
    }
    Token::Real(word)
}

/// Parses one object from the lexer, resolving `n g R` references.
pub fn parse_object(lex: &mut Lexer) -> Result<Object> {
    let (tok, start, _) = lex.next();
    parse_from_token(lex, tok, start, 0)
}

fn parse_from_token(lex: &mut Lexer, tok: Token, start: usize, depth: usize) -> Result<Object> {
    if depth > 200 {
        bail!("object nesting too deep at offset {start}");
    }
    Ok(match tok {
        Token::Int(n) => {
            let save = lex.pos;
            if n >= 0 {
                if let (Token::Int(g), _, _) = lex.next() {
                    if (0..=65535).contains(&g) {
                        if let (Token::Keyword(b"R"), _, _) = lex.next() {
                            return Ok(Object::Ref(n as u32, g as u16));
                        }
                    }
                }
            }
            lex.pos = save;
            Object::Int(n)
        }
        Token::Real(raw) => Object::Real(raw.to_vec()),
        Token::Name(n) => Object::Name(n),
        Token::Str(s, f) => Object::Str(s, f),
        Token::ArrayStart => {
            let mut items = Vec::new();
            loop {
                let (t, s, _) = lex.next();
                match t {
                    Token::ArrayEnd => break,
                    Token::Eof => bail!("unterminated array at offset {start}"),
                    // Stray keywords inside arrays are dropped rather than failing the file.
                    Token::Keyword(k) if !matches!(k, b"true" | b"false" | b"null") => {
                        if matches!(k, b"endobj" | b"stream" | b"obj") {
                            bail!("unterminated array at offset {start}");
                        }
                    }
                    t => items.push(parse_from_token(lex, t, s, depth + 1)?),
                }
            }
            Object::Array(items)
        }
        Token::DictStart => {
            let mut dict = Dict::default();
            loop {
                let (t, s, _) = lex.next();
                match t {
                    Token::DictEnd => break,
                    Token::Eof => bail!("unterminated dictionary at offset {start}"),
                    Token::Name(key) => {
                        let (vt, vs, _) = lex.next();
                        if vt == Token::DictEnd {
                            dict.0.push((key, Object::Null));
                            break;
                        }
                        let value = parse_from_token(lex, vt, vs, depth + 1)?;
                        dict.0.push((key, value));
                    }
                    Token::Keyword(k) if matches!(k, b"endobj" | b"stream" | b"obj") => {
                        bail!("unterminated dictionary at offset {start}")
                    }
                    _ => {
                        let _ = s; // skip malformed key
                    }
                }
            }
            Object::Dict(dict)
        }
        Token::Keyword(b"true") => Object::Bool(true),
        Token::Keyword(b"false") => Object::Bool(false),
        Token::Keyword(b"null") => Object::Null,
        Token::Keyword(k) => bail!(
            "unexpected keyword '{}' at offset {start}",
            String::from_utf8_lossy(k)
        ),
        Token::ArrayEnd | Token::DictEnd => bail!("unexpected closing delimiter at offset {start}"),
        Token::Eof => bail!("unexpected end of data"),
    })
}

fn sep(out: &mut Vec<u8>) {
    if out.last().is_some_and(|&b| is_regular(b)) {
        out.push(b' ');
    }
}

pub fn write_name(out: &mut Vec<u8>, name: &[u8]) {
    out.push(b'/');
    for &b in name {
        if b == b'#' || !(33..=126).contains(&b) || is_delimiter(b) {
            out.extend_from_slice(format!("#{b:02X}").as_bytes());
        } else {
            out.push(b);
        }
    }
}

pub fn write_string(out: &mut Vec<u8>, s: &[u8], fmt: StrFmt) {
    match fmt {
        StrFmt::Hex => {
            out.push(b'<');
            for b in s {
                out.extend_from_slice(format!("{b:02X}").as_bytes());
            }
            out.push(b'>');
        }
        StrFmt::Literal => {
            out.push(b'(');
            for &b in s {
                match b {
                    b'\\' | b'(' | b')' => {
                        out.push(b'\\');
                        out.push(b);
                    }
                    b'\r' => out.extend_from_slice(b"\\r"),
                    b'\n' => out.extend_from_slice(b"\\n"),
                    _ => out.push(b),
                }
            }
            out.push(b')');
        }
    }
}

/// Serializes compactly: whitespace only where two regular tokens would otherwise merge.
pub fn write_object(out: &mut Vec<u8>, obj: &Object) {
    match obj {
        Object::Null => {
            sep(out);
            out.extend_from_slice(b"null")
        }
        Object::Bool(b) => {
            sep(out);
            out.extend_from_slice(if *b { b"true" } else { b"false" })
        }
        Object::Int(i) => {
            sep(out);
            out.extend_from_slice(i.to_string().as_bytes())
        }
        Object::Real(raw) => {
            sep(out);
            out.extend_from_slice(raw)
        }
        Object::Name(n) => write_name(out, n),
        Object::Str(s, f) => write_string(out, s, *f),
        Object::Array(items) => {
            out.push(b'[');
            for item in items {
                write_object(out, item);
            }
            out.push(b']');
        }
        Object::Dict(d) => write_dict(out, d),
        Object::Ref(n, g) => {
            sep(out);
            out.extend_from_slice(format!("{n} {g} R").as_bytes())
        }
    }
}

pub fn write_dict(out: &mut Vec<u8>, d: &Dict) {
    out.extend_from_slice(b"<<");
    for (k, v) in &d.0 {
        write_name(out, k);
        write_object(out, v);
    }
    out.extend_from_slice(b">>");
}

/// Formats a number for content streams: shortest form with up to 4 decimals.
pub fn fmt_num(v: f64) -> String {
    let mut s = format!("{v:.4}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(src: &[u8]) -> Vec<u8> {
        let mut lex = Lexer::new(src, 0);
        let obj = parse_object(&mut lex).unwrap();
        let mut out = Vec::new();
        write_object(&mut out, &obj);
        out
    }

    #[test]
    fn parses_and_serializes() {
        assert_eq!(
            roundtrip(b"<< /Type /Page /Kids [ 1 0 R 2 0 R ] /N 3 /R 0.5000 /S (a\\(b\\)\\\\) /H <4a4B> >>"),
            b"<</Type/Page/Kids[1 0 R 2 0 R]/N 3/R 0.5000/S(a\\(b\\)\\\\)/H<4A4B>>>".to_vec()
        );
    }

    #[test]
    fn string_escapes() {
        let mut lex = Lexer::new(b"(a\\101\\n(x)\\\r\nb\rc)", 0);
        let (t, _, _) = lex.next();
        assert_eq!(t, Token::Str(b"aA\n(x)b\nc".to_vec(), StrFmt::Literal));
    }

    #[test]
    fn name_escapes() {
        assert_eq!(roundtrip(b"/A#20B#23"), b"/A#20B#23".to_vec());
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_real(b"-.5"), Some(-0.5));
        assert_eq!(parse_real(b"4."), Some(4.0));
        assert_eq!(classify_word(b"12"), Token::Int(12));
        assert_eq!(classify_word(b"-"), Token::Keyword(b"-"));
        assert_eq!(fmt_num(-277.50001), "-277.5");
    }
}
