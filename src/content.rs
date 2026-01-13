//! Content-stream interpretation: finds every text-showing operator, decodes
//! its glyphs and tracks where each glyph lands on the page.
//!
//! Nothing here re-serializes a content stream. Each text operator records the
//! byte range it occupies so edits can be spliced in, leaving all other
//! bytes of the stream exactly as they were.

use std::collections::HashMap;

use crate::font::Font;
use crate::object::{Lexer, StrFmt, Token, is_whitespace};

pub type Matrix = [f64; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `a` applied first, then `b`.
fn mul(a: &Matrix, b: &Matrix) -> Matrix {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

#[derive(Clone, Debug)]
pub struct Glyph {
    pub code: u32,
    pub len: u8,
    /// Origin and post-advance position in device space.
    pub p0: [f64; 2],
    pub p1: [f64; 2],
    /// Advance in text space before horizontal scaling.
    pub adv: f64,
}

#[derive(Clone, Debug)]
pub enum Item {
    Str { glyphs: Vec<Glyph>, hex: bool, start: usize, end: usize },
    Adj { value: f64, start: usize, end: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShowKind {
    Tj,
    TJ,
    Quote,
    DQuote,
}

#[derive(Clone, Debug)]
pub struct TextOp {
    /// Index of the stream (within the unit) holding this operator.
    pub stream: usize,
    /// Byte range from the first operand through the operator keyword.
    pub start: usize,
    pub end: usize,
    pub kind: ShowKind,
    pub items: Vec<Item>,
    pub font: Option<usize>,
    /// Resource name the font was selected with; empty when it came from an ExtGState.
    pub font_res: Vec<u8>,
    pub size: f64,
    pub size_raw: Vec<u8>,
    pub tc: f64,
    pub tw: f64,
    pub th: f64,
    pub vertical: bool,
    /// Device-space vectors for one em of advance and one em across the line.
    pub dir: [f64; 2],
    pub norm: [f64; 2],
    /// Source text of the `aw ac` operands of a `"` operator.
    pub dquote_args: Option<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone)]
struct GState {
    ctm: Matrix,
    font: Option<usize>,
    font_res: Vec<u8>,
    size: f64,
    size_raw: Vec<u8>,
    tc: f64,
    tw: f64,
    th: f64,
    tl: f64,
}

enum Opd {
    Num(f64, usize, usize),
    Name(Vec<u8>),
    Str(Vec<u8>, bool, usize, usize),
    Array(Vec<Opd>, usize),
    /// A dictionary operand; only `/ActualText` strings inside it are kept.
    Dict(Vec<(Vec<u8>, usize, usize)>, usize),
    Other(usize),
}

impl Opd {
    fn num(&self) -> Option<f64> {
        match self {
            Opd::Num(v, ..) => Some(*v),
            _ => None,
        }
    }
    fn start(&self) -> Option<usize> {
        match self {
            Opd::Num(_, s, _) | Opd::Str(_, _, s, _) | Opd::Array(_, s) | Opd::Dict(_, s) | Opd::Other(s) => Some(*s),
            Opd::Name(_) => None,
        }
    }
}

pub struct FontTable<'a> {
    pub fonts: &'a mut [Font],
    pub by_name: &'a HashMap<Vec<u8>, usize>,
    /// ExtGState name -> (font, size) for graphics states that select a font.
    pub by_gs: &'a HashMap<Vec<u8>, (usize, f64)>,
    /// Record every code shown as "in use" for its font.
    pub note_used: bool,
}

struct Interp<'a, 'b> {
    table: &'a mut FontTable<'b>,
    gs: GState,
    stack: Vec<GState>,
    tm: Matrix,
    tlm: Matrix,
    ops: Vec<TextOp>,
    actual: Vec<ActualText>,
}

/// An `/ActualText` string in a marked-content property list: the text that
/// copy/paste and search report in place of the glyphs it wraps.
#[derive(Clone, Debug)]
pub struct ActualText {
    pub stream: usize,
    pub start: usize,
    pub end: usize,
    pub bytes: Vec<u8>,
}

pub struct Parsed {
    pub ops: Vec<TextOp>,
    pub actual: Vec<ActualText>,
}

/// Finds the end of inline image data that starts at `pos` (just after `ID`).
fn skip_inline_image(buf: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i + 1 < buf.len() {
        if buf[i] == b'E' && buf[i + 1] == b'I' && (i == pos || is_whitespace(buf[i - 1])) {
            let after = &buf[i + 2..];
            let boundary = after.first().is_none_or(|&b| is_whitespace(b));
            let sane = after.iter().take(24).all(|&b| b == b'\n' || b == b'\r' || b == b'\t' || (32..127).contains(&b));
            if boundary && sane {
                return i + 2;
            }
        }
        i += 1;
    }
    buf.len()
}

fn read_operand(lex: &mut Lexer, tok: Token, start: usize, end: usize) -> Opd {
    match tok {
        Token::Int(v) => Opd::Num(v as f64, start, end),
        Token::Real(raw) => Opd::Num(crate::object::parse_real(raw).unwrap_or(0.0), start, end),
        Token::Name(n) => Opd::Name(n),
        Token::Str(s, fmt) => Opd::Str(s, fmt == StrFmt::Hex, start, end),
        Token::ArrayStart => {
            let mut items = Vec::new();
            loop {
                let (t, s, e) = lex.next();
                match t {
                    Token::ArrayEnd | Token::Eof => break,
                    Token::Keyword(_) => {}
                    t => items.push(read_operand(lex, t, s, e)),
                }
            }
            Opd::Array(items, start)
        }
        Token::DictStart => {
            let mut depth = 1;
            let mut key: Option<Vec<u8>> = None;
            let mut found = Vec::new();
            while depth > 0 {
                let (t, s, e) = lex.next();
                match t {
                    Token::DictStart => {
                        depth += 1;
                        key = None;
                    }
                    Token::DictEnd => depth -= 1,
                    Token::Eof => break,
                    Token::Name(n) if depth == 1 => key = if key.is_none() { Some(n) } else { None },
                    Token::Str(bytes, _) if depth == 1 => {
                        if key.as_deref() == Some(b"ActualText") {
                            found.push((bytes, s, e));
                        }
                        key = None;
                    }
                    _ => {
                        if depth == 1 {
                            key = None;
                        }
                    }
                }
            }
            Opd::Dict(found, start)
        }
        _ => Opd::Other(start),
    }
}

impl Interp<'_, '_> {
    fn begin_op(&self, stream: usize, start: usize, end: usize, kind: ShowKind) -> TextOp {
        let m = mul(&self.tm, &self.gs.ctm);
        let lin = |x: f64, y: f64| [m[0] * x + m[2] * y, m[1] * x + m[3] * y];
        let vertical = self.gs.font.is_some_and(|f| self.table.fonts[f].vertical);
        let size = self.gs.size;
        let (dir, norm) = if vertical {
            (lin(0.0, -size), lin(size, 0.0))
        } else {
            (lin(size * self.gs.th, 0.0), lin(0.0, size))
        };
        TextOp {
            stream,
            start,
            end,
            kind,
            items: Vec::new(),
            font: self.gs.font,
            font_res: self.gs.font_res.clone(),
            size,
            size_raw: self.gs.size_raw.clone(),
            tc: self.gs.tc,
            tw: self.gs.tw,
            th: self.gs.th,
            vertical,
            dir,
            norm,
            dquote_args: None,
        }
    }

    fn origin(&self) -> [f64; 2] {
        let c = &self.gs.ctm;
        let (x, y) = (self.tm[4], self.tm[5]);
        [c[0] * x + c[2] * y + c[4], c[1] * x + c[3] * y + c[5]]
    }

    fn advance(&mut self, amount: f64, vertical: bool) {
        let t = if vertical { [1.0, 0.0, 0.0, 1.0, 0.0, amount] } else { [1.0, 0.0, 0.0, 1.0, amount * self.gs.th, 0.0] };
        self.tm = mul(&t, &self.tm);
    }

    fn show(&mut self, bytes: &[u8], vertical: bool) -> Vec<Glyph> {
        let Some(fi) = self.gs.font else { return Vec::new() };
        let mut glyphs = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let (code, len) = self.table.fonts[fi].next_code(&bytes[i..]);
            i += len;
            if self.table.note_used {
                self.table.fonts[fi].note_used(code);
            }
            let word = if len == 1 && code == 32 { self.gs.tw } else { 0.0 };
            let adv = if vertical {
                -self.gs.size + self.gs.tc + word
            } else {
                self.table.fonts[fi].width(code) / 1000.0 * self.gs.size + self.gs.tc + word
            };
            let p0 = self.origin();
            self.advance(adv, vertical);
            glyphs.push(Glyph { code, len: len as u8, p0, p1: self.origin(), adv });
        }
        glyphs
    }

    fn next_line(&mut self, tx: f64, ty: f64) {
        self.tlm = mul(&[1.0, 0.0, 0.0, 1.0, tx, ty], &self.tlm);
        self.tm = self.tlm;
    }

    fn run(&mut self, stream: usize, buf: &[u8]) {
        let mut lex = Lexer::new(buf, 0);
        let mut opds: Vec<Opd> = Vec::new();
        loop {
            let (tok, start, end) = lex.next();
            let op = match tok {
                Token::Eof => break,
                Token::Keyword(op) => op,
                Token::ArrayEnd | Token::DictEnd => continue,
                tok => {
                    opds.push(read_operand(&mut lex, tok, start, end));
                    continue;
                }
            };
            let nums: Vec<f64> = opds.iter().filter_map(Opd::num).collect();
            match op {
                b"q" => self.stack.push(self.gs.clone()),
                b"Q" => {
                    if let Some(g) = self.stack.pop() {
                        self.gs = g;
                    }
                }
                b"cm" if nums.len() == 6 => {
                    let m: Matrix = nums[..6].try_into().unwrap();
                    self.gs.ctm = mul(&m, &self.gs.ctm);
                }
                b"BT" => {
                    self.tm = IDENTITY;
                    self.tlm = IDENTITY;
                }
                b"Tf" => {
                    if let (Some(Opd::Name(name)), Some(Opd::Num(size, s, e))) =
                        (opds.len().checked_sub(2).map(|i| &opds[i]), opds.last())
                    {
                        self.gs.font = self.table.by_name.get(name).copied();
                        self.gs.font_res = name.clone();
                        self.gs.size = *size;
                        self.gs.size_raw = buf[*s..*e].to_vec();
                    }
                }
                b"gs" => {
                    if let Some(Opd::Name(name)) = opds.last() {
                        if let Some(&(font, size)) = self.table.by_gs.get(name) {
                            self.gs.font = Some(font);
                            self.gs.font_res = Vec::new();
                            self.gs.size = size;
                            self.gs.size_raw = crate::object::fmt_num(size).into_bytes();
                        }
                    }
                }
                b"Tc" if !nums.is_empty() => self.gs.tc = nums[0],
                b"Tw" if !nums.is_empty() => self.gs.tw = nums[0],
                b"Tz" if !nums.is_empty() => self.gs.th = nums[0] / 100.0,
                b"TL" if !nums.is_empty() => self.gs.tl = nums[0],
                b"Td" if nums.len() >= 2 => self.next_line(nums[0], nums[1]),
                b"TD" if nums.len() >= 2 => {
                    self.gs.tl = -nums[1];
                    self.next_line(nums[0], nums[1]);
                }
                b"Tm" if nums.len() == 6 => {
                    self.tm = nums[..6].try_into().unwrap();
                    self.tlm = self.tm;
                }
                b"T*" => self.next_line(0.0, -self.gs.tl),
                b"Tj" | b"'" | b"\"" => {
                    let is_dq = op == b"\"";
                    let want = if is_dq { 3 } else { 1 };
                    if opds.len() >= want {
                        if let Some(Opd::Str(bytes, hex, s, e)) = opds.last() {
                            let first = &opds[opds.len() - want];
                            let op_start = first.start().unwrap_or(*s);
                            let mut dq = None;
                            if is_dq {
                                if let (Opd::Num(aw, s1, e1), Opd::Num(ac, s2, e2)) =
                                    (&opds[opds.len() - 3], &opds[opds.len() - 2])
                                {
                                    self.gs.tw = *aw;
                                    self.gs.tc = *ac;
                                    dq = Some((buf[*s1..*e1].to_vec(), buf[*s2..*e2].to_vec()));
                                }
                            }
                            if op != b"Tj" {
                                self.next_line(0.0, -self.gs.tl);
                            }
                            let kind = match op {
                                b"Tj" => ShowKind::Tj,
                                b"'" => ShowKind::Quote,
                                _ => ShowKind::DQuote,
                            };
                            let mut top = self.begin_op(stream, op_start, end, kind);
                            top.dquote_args = dq;
                            let glyphs = self.show(bytes, top.vertical);
                            top.items.push(Item::Str { glyphs, hex: *hex, start: *s, end: *e });
                            self.ops.push(top);
                        }
                    }
                }
                b"TJ" => {
                    if let Some(Opd::Array(items, astart)) = opds.last() {
                        let mut top = self.begin_op(stream, *astart, end, ShowKind::TJ);
                        for item in items {
                            match item {
                                Opd::Str(bytes, hex, s, e) => {
                                    let glyphs = self.show(bytes, top.vertical);
                                    top.items.push(Item::Str { glyphs, hex: *hex, start: *s, end: *e });
                                }
                                Opd::Num(v, s, e) => {
                                    self.advance(-v / 1000.0 * self.gs.size, top.vertical);
                                    top.items.push(Item::Adj { value: *v, start: *s, end: *e });
                                }
                                _ => {}
                            }
                        }
                        self.ops.push(top);
                    }
                }
                b"BDC" => {
                    if let Some(Opd::Dict(found, _)) = opds.last() {
                        for (bytes, s, e) in found {
                            self.actual.push(ActualText { stream, start: *s, end: *e, bytes: bytes.clone() });
                        }
                    }
                }
                b"BI" => {
                    loop {
                        match lex.next().0 {
                            Token::Keyword(b"ID") | Token::Eof => break,
                            _ => {}
                        }
                    }
                    // Exactly one whitespace byte separates ID from the image data.
                    let data_start = (lex.pos + 1).min(buf.len());
                    lex.pos = skip_inline_image(buf, data_start);
                }
                _ => {}
            }
            opds.clear();
        }
    }
}

/// Interprets the streams of one content unit in order, carrying graphics
/// state from one stream to the next as a page's `/Contents` array requires.
pub fn parse(streams: &[Vec<u8>], table: &mut FontTable) -> Parsed {
    let gs = GState {
        ctm: IDENTITY,
        font: None,
        font_res: Vec::new(),
        size: 0.0,
        size_raw: Vec::new(),
        tc: 0.0,
        tw: 0.0,
        th: 1.0,
        tl: 0.0,
    };
    let mut interp = Interp { table, gs, stack: Vec::new(), tm: IDENTITY, tlm: IDENTITY, ops: Vec::new(), actual: Vec::new() };
    for (i, s) in streams.iter().enumerate() {
        interp.run(i, s);
    }
    Parsed { ops: interp.ops, actual: interp.actual }
}
