//! Font handling: character-code segmentation, code <-> Unicode mapping and
//! glyph widths. Enough to read the text a content stream renders and to
//! re-encode replacement text with glyphs the font is known to contain.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::object::{Dict, Lexer, Object, Token};
use crate::pdf::{Pdf, PdfObj};

pub struct Font {
    pub base_name: String,
    pub type0: bool,
    pub vertical: bool,
    pub subset: bool,
    /// Code length is unknown (unsupported predefined CMap); text is not searchable.
    pub opaque: bool,
    /// (byte length, low, high)
    codespace: Vec<(u8, u32, u32)>,
    to_text: HashMap<u32, String>,
    widths: HashMap<u32, f64>,
    default_width: f64,
    /// Codes that certainly have a glyph regardless of use (Type 3 CharProcs).
    trusted: Option<HashSet<u32>>,
    /// Codes observed in content streams: their glyphs certainly exist.
    pub used: BTreeSet<u32>,
    reverse: Option<HashMap<String, u32>>,
    max_key_chars: usize,
    pub style: Style,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub family: Family,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Family {
    #[default]
    Sans,
    Serif,
    Mono,
}

impl Style {
    /// Name of the matching standard-14 font, which every PDF reader provides.
    pub fn standard_font(&self) -> &'static str {
        match (self.family, self.bold, self.italic) {
            (Family::Sans, false, false) => "Helvetica",
            (Family::Sans, true, false) => "Helvetica-Bold",
            (Family::Sans, false, true) => "Helvetica-Oblique",
            (Family::Sans, true, true) => "Helvetica-BoldOblique",
            (Family::Serif, false, false) => "Times-Roman",
            (Family::Serif, true, false) => "Times-Bold",
            (Family::Serif, false, true) => "Times-Italic",
            (Family::Serif, true, true) => "Times-BoldItalic",
            (Family::Mono, false, false) => "Courier",
            (Family::Mono, true, false) => "Courier-Bold",
            (Family::Mono, false, true) => "Courier-Oblique",
            (Family::Mono, true, true) => "Courier-BoldOblique",
        }
    }
}

fn num(pdf: &Pdf, o: Option<&Object>) -> Option<f64> {
    pdf.resolve(o?).as_f64()
}

fn strip_subset_tag(name: &str) -> (&str, bool) {
    let b = name.as_bytes();
    if b.len() > 7 && b[6] == b'+' && b[..6].iter().all(u8::is_ascii_uppercase) {
        (&name[7..], true)
    } else {
        (name, false)
    }
}

fn style_of(name: &str, flags: i64, desc: Option<&Dict>, pdf: &Pdf) -> Style {
    let lower = name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let weight = desc.and_then(|d| num(pdf, d.get(b"FontWeight"))).unwrap_or(0.0);
    let angle = desc.and_then(|d| num(pdf, d.get(b"ItalicAngle"))).unwrap_or(0.0);
    let family = if flags & 1 != 0 || has(&["courier", "mono", "consolas", "menlo", "typewriter", "cmtt"]) {
        Family::Mono
    } else if has(&["sans", "helvetica", "arial", "calibri", "verdana", "tahoma", "roboto", "cmss"]) {
        Family::Sans
    } else if flags & 2 != 0
        || has(&[
            "times", "serif", "georgia", "garamond", "roman", "minion", "cambria", "palatino", "book", "cmr", "cmbx",
            "cmti", "sfrm", "century", "baskerville", "caslon", "charter", "nimbusrom", "libertine",
        ])
    {
        Family::Serif
    } else {
        Family::Sans
    };
    Style {
        family,
        bold: flags & (1 << 18) != 0 || weight >= 600.0 || has(&["bold", "black", "heavy", "cmbx", "-bd", "demi"])
            || lower.ends_with("medi")
            || lower.contains("medi-"),
        italic: flags & 64 != 0 || angle != 0.0 || has(&["italic", "oblique", "cmti", "-it"]),
    }
}

impl Font {
    pub fn load(pdf: &Pdf, dict: &Dict) -> Font {
        let subtype = dict.name(b"Subtype").unwrap_or(b"");
        let type0 = subtype == b"Type0";
        let type3 = subtype == b"Type3";
        let descendant: Option<&Dict> = if type0 {
            match pdf.resolve(dict.get(b"DescendantFonts").unwrap_or(&Object::Null)) {
                Object::Array(a) => a.first().and_then(|o| pdf.resolve(o).as_dict()),
                _ => None,
            }
        } else {
            None
        };
        let desc_owner = descendant.unwrap_or(dict);
        let descriptor = pdf.dict_at(desc_owner, b"FontDescriptor");
        let flags = descriptor.and_then(|d| num(pdf, d.get(b"Flags"))).unwrap_or(0.0) as i64;
        let embedded = descriptor
            .is_some_and(|d| d.has(b"FontFile") || d.has(b"FontFile2") || d.has(b"FontFile3"));
        let full_name =
            String::from_utf8_lossy(pdf.resolve(dict.get(b"BaseFont").unwrap_or(&Object::Null)).as_name().unwrap_or(b""))
                .into_owned();
        let (base, tagged) = strip_subset_tag(&full_name);
        let base_name = base.to_string();

        let mut font = Font {
            style: style_of(&base_name, flags, descriptor, pdf),
            base_name,
            type0,
            vertical: false,
            // A font without an embedded program is supplied in full by the reader.
            subset: tagged && embedded,
            opaque: false,
            codespace: vec![(1, 0, 255)],
            to_text: HashMap::new(),
            widths: HashMap::new(),
            default_width: 0.0,
            trusted: None,
            used: BTreeSet::new(),
            reverse: None,
            max_key_chars: 1,
        };

        let to_unicode = match dict.get(b"ToUnicode") {
            Some(Object::Ref(n, _)) => pdf.stream_data(*n).ok().map(|d| parse_cmap(&d)),
            _ => None,
        };

        if type0 {
            font.load_type0(pdf, dict, descendant, to_unicode.as_ref());
        } else {
            font.load_simple(pdf, dict, descriptor, flags, embedded, type3);
        }
        if let Some(cmap) = to_unicode {
            for (code, text) in cmap.map {
                font.to_text.insert(code, text);
            }
        }
        for text in font.to_text.values_mut() {
            normalize(text);
        }
        font.to_text.retain(|_, t| !t.is_empty());
        font
    }

    fn load_type0(&mut self, pdf: &Pdf, dict: &Dict, descendant: Option<&Dict>, to_unicode: Option<&CMap>) {
        let mut identity = false;
        self.codespace = vec![(2, 0, 0xFFFF)];
        match dict.get(b"Encoding") {
            Some(Object::Ref(n, _)) if matches!(pdf.get(*n), Some(PdfObj::Stream(_))) => {
                let cmap = pdf.stream_data(*n).map(|d| parse_cmap(&d)).unwrap_or_default();
                if !cmap.codespace.is_empty() {
                    self.codespace = cmap.codespace;
                } else if let Some(tu) = to_unicode.filter(|c| !c.codespace.is_empty()) {
                    self.codespace = tu.codespace.clone();
                }
                self.vertical = cmap.vertical;
            }
            other => {
                let name = other.map(|o| pdf.resolve(o)).and_then(Object::as_name).unwrap_or(b"Identity-H");
                let name = String::from_utf8_lossy(name);
                self.vertical = name.ends_with("-V");
                if name.starts_with("Identity") {
                    identity = true;
                } else if name.contains("UCS2") {
                    for c in 0..=0xFFFFu32 {
                        if let Some(ch) = char::from_u32(c).filter(|ch| !ch.is_control()) {
                            self.to_text.insert(c, ch.to_string());
                        }
                    }
                } else if let Some(tu) = to_unicode.filter(|c| !c.codespace.is_empty()) {
                    self.codespace = tu.codespace.clone();
                } else {
                    self.opaque = true;
                }
            }
        }
        self.default_width = 1000.0;
        if let Some(d) = descendant {
            self.default_width = num(pdf, d.get(b"DW")).unwrap_or(1000.0);
            // Width tables are keyed by CID, which equals the code only for Identity encodings.
            if identity {
                if let Object::Array(w) = pdf.resolve(d.get(b"W").unwrap_or(&Object::Null)) {
                    let mut i = 0;
                    while i < w.len() {
                        let Some(first) = pdf.resolve(&w[i]).as_int() else { break };
                        match w.get(i + 1).map(|o| pdf.resolve(o)) {
                            Some(Object::Array(list)) => {
                                for (k, wv) in list.iter().enumerate() {
                                    if let Some(v) = pdf.resolve(wv).as_f64() {
                                        self.widths.insert(first as u32 + k as u32, v);
                                    }
                                }
                                i += 2;
                            }
                            Some(last) => {
                                let (Some(last), Some(v)) =
                                    (last.as_int(), w.get(i + 2).and_then(|o| pdf.resolve(o).as_f64()))
                                else {
                                    break;
                                };
                                for c in first..=last.min(first + 0xFFFF) {
                                    self.widths.insert(c as u32, v);
                                }
                                i += 3;
                            }
                            None => break,
                        }
                    }
                }
            }
        }
    }

    fn load_simple(
        &mut self,
        pdf: &Pdf,
        dict: &Dict,
        descriptor: Option<&Dict>,
        flags: i64,
        embedded: bool,
        type3: bool,
    ) {
        let subtype = dict.name(b"Subtype").unwrap_or(b"");
        let symbolic = flags & 4 != 0 && flags & 32 == 0;
        let lower = self.base_name.to_ascii_lowercase();
        let is_symbol_font = lower.starts_with("symbol") || lower.starts_with("zapfdingbats");
        let default_base: Option<Base> = if type3 || is_symbol_font || (symbolic && embedded) {
            None
        } else if subtype == b"TrueType" {
            Some(Base::WinAnsi)
        } else {
            Some(Base::Standard)
        };
        let encoding = pdf.resolve(dict.get(b"Encoding").unwrap_or(&Object::Null));
        let named = |n: &[u8]| match n {
            b"WinAnsiEncoding" => Some(Base::WinAnsi),
            b"MacRomanEncoding" => Some(Base::MacRoman),
            b"StandardEncoding" => Some(Base::Standard),
            _ => None,
        };
        let mut glyph_names: HashMap<u32, Vec<u8>> = HashMap::new();
        let base = match encoding {
            Object::Name(n) => named(n),
            Object::Dict(d) => {
                if let Object::Array(diffs) = pdf.resolve(d.get(b"Differences").unwrap_or(&Object::Null)) {
                    let mut code = 0u32;
                    for item in diffs {
                        match pdf.resolve(item) {
                            Object::Name(n) => {
                                glyph_names.insert(code, n.clone());
                                code += 1;
                            }
                            other => {
                                if let Some(c) = other.as_int() {
                                    code = c.max(0) as u32;
                                }
                            }
                        }
                    }
                }
                match pdf.resolve(d.get(b"BaseEncoding").unwrap_or(&Object::Null)).as_name() {
                    Some(n) => named(n),
                    None => default_base,
                }
            }
            _ => default_base,
        };
        if let Some(base) = base {
            for code in 0..256u32 {
                if let Some(ch) = base.decode(code as u8) {
                    self.to_text.insert(code, ch.to_string());
                }
            }
        }
        for (code, name) in &glyph_names {
            match glyph_text(name) {
                Some(t) => self.to_text.insert(*code, t),
                None => self.to_text.remove(code),
            };
        }
        if type3 {
            let procs = pdf.dict_at(dict, b"CharProcs");
            self.trusted = Some(
                glyph_names
                    .iter()
                    .filter(|(_, name)| procs.is_some_and(|p| p.has(name)))
                    .map(|(c, _)| *c)
                    .collect(),
            );
            self.subset = true; // only codes with a CharProc are drawable
        }

        // Widths.
        let scale = if type3 {
            match pdf.resolve(dict.get(b"FontMatrix").unwrap_or(&Object::Null)) {
                Object::Array(m) => m.first().and_then(|o| pdf.resolve(o).as_f64()).unwrap_or(0.001) * 1000.0,
                _ => 1.0,
            }
        } else {
            1.0
        };
        self.default_width = descriptor.and_then(|d| num(pdf, d.get(b"MissingWidth"))).unwrap_or(0.0);
        let first = num(pdf, dict.get(b"FirstChar")).unwrap_or(0.0) as u32;
        if let Object::Array(ws) = pdf.resolve(dict.get(b"Widths").unwrap_or(&Object::Null)) {
            for (i, w) in ws.iter().enumerate() {
                if let Some(v) = pdf.resolve(w).as_f64() {
                    self.widths.insert(first + i as u32, v * scale);
                }
            }
        } else {
            // Standard-14 font without explicit metrics.
            for code in 32..127u32 {
                self.widths.insert(code, standard_width(self.style.family, code as u8 as char));
            }
            self.default_width = standard_width(self.style.family, 'n');
        }
    }

    /// Splits the next character code off `bytes`. Returns (code, byte length).
    pub fn next_code(&self, bytes: &[u8]) -> (u32, usize) {
        if !self.type0 {
            return (bytes[0] as u32, 1);
        }
        let mut v = 0u32;
        for (i, &b) in bytes.iter().take(4).enumerate() {
            v = v << 8 | b as u32;
            let len = (i + 1) as u8;
            if self.codespace.iter().any(|&(l, lo, hi)| l == len && lo <= v && v <= hi) {
                return (v, i + 1);
            }
        }
        let len = self.codespace.iter().map(|c| c.0 as usize).min().unwrap_or(1).min(bytes.len());
        (bytes[..len].iter().fold(0, |a, &b| a << 8 | b as u32), len)
    }

    fn code_len(&self, code: u32) -> usize {
        if !self.type0 {
            return 1;
        }
        self.codespace
            .iter()
            .filter(|&&(_, lo, hi)| lo <= code && code <= hi)
            .map(|c| c.0 as usize)
            .min()
            .unwrap_or(2)
    }

    pub fn code_bytes(&self, code: u32) -> Vec<u8> {
        let len = self.code_len(code);
        code.to_be_bytes()[4 - len..].to_vec()
    }

    pub fn text(&self, code: u32) -> Option<&str> {
        if self.opaque {
            return None;
        }
        self.to_text.get(&code).map(String::as_str)
    }

    /// Glyph advance in thousandths of the font size.
    pub fn width(&self, code: u32) -> f64 {
        self.widths.get(&code).copied().unwrap_or(self.default_width)
    }

    /// Width of a word space, for fonts where spaces must be synthesized as a gap.
    pub fn space_width(&self) -> f64 {
        self.to_text
            .iter()
            .filter(|(_, t)| t.as_str() == " ")
            .map(|(c, _)| self.width(*c))
            .find(|w| *w > 0.0)
            .unwrap_or(260.0)
    }

    pub fn note_used(&mut self, code: u32) {
        if self.used.insert(code) {
            self.reverse = None;
        }
    }

    fn usable(&self, code: u32) -> bool {
        if self.used.contains(&code) {
            return true;
        }
        match &self.trusted {
            Some(t) => t.contains(&code),
            // A subsetted font only holds the glyphs the document already uses.
            None => !self.subset,
        }
    }

    fn build_reverse(&mut self) {
        let mut map: HashMap<String, u32> = HashMap::new();
        let mut codes: Vec<u32> = self.to_text.keys().copied().filter(|c| self.usable(*c)).collect();
        // Prefer codes already in use, then the lowest code, for determinism.
        codes.sort_by_key(|c| (!self.used.contains(c), *c));
        let mut max = 1;
        for code in codes {
            let text = &self.to_text[&code];
            if text.contains('\u{FFFD}') {
                continue;
            }
            max = max.max(text.chars().count());
            map.entry(text.clone()).or_insert(code);
        }
        self.max_key_chars = max;
        self.reverse = Some(map);
    }

    /// Encodes `text` using only glyphs known to exist. `None` entries are
    /// spaces the font has no glyph for (the caller emits a positioning gap).
    pub fn encode(&mut self, text: &str) -> Option<Vec<Option<u32>>> {
        if self.opaque {
            return None;
        }
        if self.reverse.is_none() {
            self.build_reverse();
        }
        let map = self.reverse.as_ref().unwrap();
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        'outer: while i < chars.len() {
            for n in (1..=self.max_key_chars.min(chars.len() - i)).rev() {
                let key: String = chars[i..i + n].iter().collect();
                if let Some(code) = map.get(&key) {
                    out.push(Some(*code));
                    i += n;
                    continue 'outer;
                }
            }
            if chars[i] == ' ' || chars[i] == '\u{A0}' {
                out.push(None);
                i += 1;
                continue;
            }
            return None;
        }
        Some(out)
    }

    /// The first character of `text` that `encode` cannot represent.
    pub fn first_unencodable(&mut self, text: &str) -> Option<char> {
        text.chars().find(|c| self.encode(&c.to_string()).is_none())
    }
}

fn normalize(text: &mut String) {
    if text.chars().all(|c| c.is_ascii() && c != '\0') {
        return;
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            '\u{A0}' => out.push(' '),
            '\0' => {}
            c => out.push(c),
        }
    }
    *text = out;
}

// ------------------------------------------------------------------ CMaps

#[derive(Default)]
struct CMap {
    codespace: Vec<(u8, u32, u32)>,
    map: HashMap<u32, String>,
    vertical: bool,
}

fn code_of(bytes: &[u8]) -> u32 {
    bytes.iter().take(4).fold(0, |a, &b| a << 8 | b as u32)
}

fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks(2).map(|c| if c.len() == 2 { (c[0] as u16) << 8 | c[1] as u16 } else { c[0] as u16 }).collect();
    String::from_utf16_lossy(&units)
}

fn parse_cmap(data: &[u8]) -> CMap {
    let mut cmap = CMap::default();
    let mut lex = Lexer::new(data, 0);
    let mut prev_name: Option<Vec<u8>> = None;
    loop {
        let (tok, _, _) = lex.next();
        match tok {
            Token::Eof => break,
            Token::Name(n) => {
                prev_name = Some(n);
                continue;
            }
            Token::Int(v) => {
                if prev_name.as_deref() == Some(b"WMode") {
                    cmap.vertical = v == 1;
                }
            }
            Token::Keyword(b"begincodespacerange") => loop {
                let (Token::Str(lo, _), _, _) = lex.next() else { break };
                let (Token::Str(hi, _), _, _) = lex.next() else { break };
                if !lo.is_empty() && lo.len() <= 4 {
                    cmap.codespace.push((lo.len() as u8, code_of(&lo), code_of(&hi)));
                }
            },
            Token::Keyword(b"beginbfchar") => loop {
                let (Token::Str(src, _), _, _) = lex.next() else { break };
                match lex.next().0 {
                    Token::Str(dst, _) => {
                        cmap.map.insert(code_of(&src), utf16be(&dst));
                    }
                    Token::Name(_) => {}
                    _ => break,
                }
            },
            Token::Keyword(b"beginbfrange") => loop {
                let (Token::Str(lo, _), _, _) = lex.next() else { break };
                let (Token::Str(hi, _), _, _) = lex.next() else { break };
                let (lo, hi) = (code_of(&lo), code_of(&hi));
                match lex.next().0 {
                    Token::Str(dst, _) => {
                        let base: Vec<char> = utf16be(&dst).chars().collect();
                        for (k, code) in (lo..=hi.min(lo.saturating_add(0xFFFF))).enumerate() {
                            let mut chars = base.clone();
                            if let Some(last) = chars.last_mut() {
                                match char::from_u32(*last as u32 + k as u32) {
                                    Some(c) => *last = c,
                                    None => continue,
                                }
                            }
                            cmap.map.insert(code, chars.into_iter().collect());
                        }
                    }
                    Token::ArrayStart => {
                        let mut code = lo;
                        loop {
                            match lex.next().0 {
                                Token::Str(dst, _) => {
                                    if code <= hi {
                                        cmap.map.insert(code, utf16be(&dst));
                                    }
                                    code += 1;
                                }
                                Token::ArrayEnd | Token::Eof => break,
                                _ => {}
                            }
                        }
                    }
                    _ => break,
                }
            },
            _ => {}
        }
        prev_name = None;
    }
    cmap
}

// -------------------------------------------------------------- encodings

#[derive(Clone, Copy)]
enum Base {
    WinAnsi,
    MacRoman,
    Standard,
}

const WIN_80_9F: [u32; 32] = [
    0x20AC, 0x2022, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x2022,
    0x017D, 0x2022, 0x2022, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A,
    0x0153, 0x2022, 0x017E, 0x0178,
];

const MAC_80_FF: [u32; 128] = [
    0xC4, 0xC5, 0xC7, 0xC9, 0xD1, 0xD6, 0xDC, 0xE1, 0xE0, 0xE2, 0xE4, 0xE3, 0xE5, 0xE7, 0xE9, 0xE8, 0xEA, 0xEB, 0xED,
    0xEC, 0xEE, 0xEF, 0xF1, 0xF3, 0xF2, 0xF4, 0xF6, 0xF5, 0xFA, 0xF9, 0xFB, 0xFC, 0x2020, 0xB0, 0xA2, 0xA3, 0xA7,
    0x2022, 0xB6, 0xDF, 0xAE, 0xA9, 0x2122, 0xB4, 0xA8, 0x2260, 0xC6, 0xD8, 0x221E, 0xB1, 0x2264, 0x2265, 0xA5, 0xB5,
    0x2202, 0x2211, 0x220F, 0x03C0, 0x222B, 0xAA, 0xBA, 0x03A9, 0xE6, 0xF8, 0xBF, 0xA1, 0xAC, 0x221A, 0x0192, 0x2248,
    0x2206, 0xAB, 0xBB, 0x2026, 0xA0, 0xC0, 0xC3, 0xD5, 0x0152, 0x0153, 0x2013, 0x2014, 0x201C, 0x201D, 0x2018,
    0x2019, 0xF7, 0x25CA, 0xFF, 0x0178, 0x2044, 0x20AC, 0x2039, 0x203A, 0xFB01, 0xFB02, 0x2021, 0xB7, 0x201A, 0x201E,
    0x2030, 0xC2, 0xCA, 0xC1, 0xCB, 0xC8, 0xCD, 0xCE, 0xCF, 0xCC, 0xD3, 0xD4, 0xF8FF, 0xD2, 0xDA, 0xDB, 0xD9, 0x0131,
    0x02C6, 0x02DC, 0xAF, 0x02D8, 0x02D9, 0x02DA, 0xB8, 0x02DD, 0x02DB, 0x02C7,
];

/// StandardEncoding codes 0xA1..=0xFF (0 = unassigned).
const STD_A1_FF: &[u32] = &[
    0xA1, 0xA2, 0xA3, 0x2044, 0xA5, 0x0192, 0xA7, 0xA4, 0x27, 0x201C, 0xAB, 0x2039, 0x203A, 0xFB01, 0xFB02, 0, 0x2013,
    0x2020, 0x2021, 0xB7, 0, 0xB6, 0x2022, 0x201A, 0x201E, 0x201D, 0xBB, 0x2026, 0x2030, 0, 0xBF, 0, 0x60, 0xB4,
    0x02C6, 0x02DC, 0xAF, 0x02D8, 0x02D9, 0xA8, 0, 0x02DA, 0xB8, 0, 0x02DD, 0x02DB, 0x02C7, 0x2014, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xC6, 0, 0xAA, 0, 0, 0, 0, 0x0141, 0xD8, 0x0152, 0xBA, 0, 0, 0, 0, 0, 0xE6, 0, 0, 0,
    0x0131, 0, 0, 0x0142, 0xF8, 0x0153, 0xDF, 0, 0, 0, 0,
];

impl Base {
    fn decode(self, code: u8) -> Option<char> {
        let cp = match (self, code) {
            (_, 0..=31) | (_, 127) => return None,
            (Base::Standard, 0x27) => 0x2019,
            (Base::Standard, 0x60) => 0x2018,
            (_, 32..=126) => code as u32,
            (Base::WinAnsi, 0x80..=0x9F) => WIN_80_9F[code as usize - 0x80],
            (Base::WinAnsi, 0xAD) => 0x2D,
            (Base::WinAnsi, _) => code as u32,
            (Base::MacRoman, _) => MAC_80_FF[code as usize - 0x80],
            (Base::Standard, 0xA1..=0xFF) => STD_A1_FF.get(code as usize - 0xA1).copied().unwrap_or(0),
            (Base::Standard, _) => 0,
        };
        if cp == 0 { None } else { char::from_u32(cp) }
    }
}

const LATIN1_NAMES: [&str; 96] = [
    "nbspace", "exclamdown", "cent", "sterling", "currency", "yen", "brokenbar", "section", "dieresis", "copyright",
    "ordfeminine", "guillemotleft", "logicalnot", "sfthyphen", "registered", "macron", "degree", "plusminus",
    "twosuperior", "threesuperior", "acute", "mu", "paragraph", "periodcentered", "cedilla", "onesuperior",
    "ordmasculine", "guillemotright", "onequarter", "onehalf", "threequarters", "questiondown", "Agrave", "Aacute",
    "Acircumflex", "Atilde", "Adieresis", "Aring", "AE", "Ccedilla", "Egrave", "Eacute", "Ecircumflex", "Edieresis",
    "Igrave", "Iacute", "Icircumflex", "Idieresis", "Eth", "Ntilde", "Ograve", "Oacute", "Ocircumflex", "Otilde",
    "Odieresis", "multiply", "Oslash", "Ugrave", "Uacute", "Ucircumflex", "Udieresis", "Yacute", "Thorn",
    "germandbls", "agrave", "aacute", "acircumflex", "atilde", "adieresis", "aring", "ae", "ccedilla", "egrave",
    "eacute", "ecircumflex", "edieresis", "igrave", "iacute", "icircumflex", "idieresis", "eth", "ntilde", "ograve",
    "oacute", "ocircumflex", "otilde", "odieresis", "divide", "oslash", "ugrave", "uacute", "ucircumflex",
    "udieresis", "yacute", "thorn", "ydieresis",
];

const ASCII_NAMES: [&str; 33] = [
    "space", "exclam", "quotedbl", "numbersign", "dollar", "percent", "ampersand", "quotesingle", "parenleft",
    "parenright", "asterisk", "plus", "comma", "hyphen", "period", "slash", "zero", "one", "two", "three", "four",
    "five", "six", "seven", "eight", "nine", "colon", "semicolon", "less", "equal", "greater", "question", "at",
];

const OTHER_NAMES: &[(&str, u32)] = &[
    ("bracketleft", 0x5B), ("backslash", 0x5C), ("bracketright", 0x5D), ("asciicircum", 0x5E), ("underscore", 0x5F),
    ("grave", 0x60), ("braceleft", 0x7B), ("bar", 0x7C), ("braceright", 0x7D), ("asciitilde", 0x7E),
    ("quoteleft", 0x2018), ("quoteright", 0x2019), ("quotedblleft", 0x201C), ("quotedblright", 0x201D),
    ("quotesinglbase", 0x201A), ("quotedblbase", 0x201E), ("endash", 0x2013), ("emdash", 0x2014), ("bullet", 0x2022),
    ("ellipsis", 0x2026), ("dagger", 0x2020), ("daggerdbl", 0x2021), ("perthousand", 0x2030),
    ("guilsinglleft", 0x2039), ("guilsinglright", 0x203A), ("fraction", 0x2044), ("Euro", 0x20AC),
    ("trademark", 0x2122), ("florin", 0x0192), ("fi", 0xFB01), ("fl", 0xFB02), ("ff", 0xFB00), ("ffi", 0xFB03),
    ("ffl", 0xFB04), ("OE", 0x0152), ("oe", 0x0153), ("Scaron", 0x0160), ("scaron", 0x0161), ("Zcaron", 0x017D),
    ("zcaron", 0x017E), ("Ydieresis", 0x0178), ("Lslash", 0x0141), ("lslash", 0x0142), ("dotlessi", 0x0131),
    ("circumflex", 0x02C6), ("tilde", 0x02DC), ("breve", 0x02D8), ("dotaccent", 0x02D9), ("ring", 0x02DA),
    ("hungarumlaut", 0x02DD), ("ogonek", 0x02DB), ("caron", 0x02C7), ("minus", 0x2212),
    ("nonbreakingspace", 0xA0), ("softhyphen", 0xAD), ("middot", 0xB7), ("guillemetleft", 0xAB),
    ("guillemetright", 0xBB), ("Abreve", 0x0102), ("abreve", 0x0103), ("Aogonek", 0x0104), ("aogonek", 0x0105),
    ("Cacute", 0x0106), ("cacute", 0x0107), ("Ccaron", 0x010C), ("ccaron", 0x010D), ("Dcaron", 0x010E),
    ("dcaron", 0x010F), ("Dcroat", 0x0110), ("dcroat", 0x0111), ("Eogonek", 0x0118), ("eogonek", 0x0119),
    ("Ecaron", 0x011A), ("ecaron", 0x011B), ("Gbreve", 0x011E), ("gbreve", 0x011F), ("Idotaccent", 0x0130),
    ("Lacute", 0x0139), ("lacute", 0x013A), ("Lcaron", 0x013D), ("lcaron", 0x013E), ("Nacute", 0x0143),
    ("nacute", 0x0144), ("Ncaron", 0x0147), ("ncaron", 0x0148), ("Ohungarumlaut", 0x0150),
    ("ohungarumlaut", 0x0151), ("Racute", 0x0154), ("racute", 0x0155), ("Rcaron", 0x0158), ("rcaron", 0x0159),
    ("Sacute", 0x015A), ("sacute", 0x015B), ("Scedilla", 0x015E), ("scedilla", 0x015F), ("Tcaron", 0x0164),
    ("tcaron", 0x0165), ("Uring", 0x016E), ("uring", 0x016F), ("Uhungarumlaut", 0x0170), ("uhungarumlaut", 0x0171),
    ("Zacute", 0x0179), ("zacute", 0x017A), ("Zdotaccent", 0x017B), ("zdotaccent", 0x017C), ("Amacron", 0x0100),
    ("amacron", 0x0101), ("Emacron", 0x0112), ("emacron", 0x0113), ("Imacron", 0x012A), ("imacron", 0x012B),
    ("Omacron", 0x014C), ("omacron", 0x014D), ("Umacron", 0x016A), ("umacron", 0x016B), ("lessequal", 0x2264),
    ("greaterequal", 0x2265), ("notequal", 0x2260), ("infinity", 0x221E), ("Delta", 0x2206), ("apple", 0xF8FF),
];

fn component_text(name: &str) -> Option<String> {
    if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
        return Some(name.to_string());
    }
    if let Some(i) = ASCII_NAMES.iter().position(|n| *n == name) {
        return char::from_u32(32 + i as u32).map(String::from);
    }
    if let Some(i) = LATIN1_NAMES.iter().position(|n| *n == name) {
        return char::from_u32(0xA0 + i as u32).map(String::from);
    }
    if let Some((_, cp)) = OTHER_NAMES.iter().find(|(n, _)| *n == name) {
        return char::from_u32(*cp).map(String::from);
    }
    let hex_ok = |h: &str| !h.is_empty() && h.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b));
    if let Some(h) = name.strip_prefix("uni").filter(|h| hex_ok(h) && h.len() % 4 == 0) {
        let units: Vec<u16> = (0..h.len() / 4).filter_map(|i| u16::from_str_radix(&h[i * 4..i * 4 + 4], 16).ok()).collect();
        return String::from_utf16(&units).ok();
    }
    if let Some(h) = name.strip_prefix('u').filter(|h| hex_ok(h) && (4..=6).contains(&h.len())) {
        return u32::from_str_radix(h, 16).ok().and_then(char::from_u32).map(String::from);
    }
    None
}

/// Unicode text for an Adobe glyph name (`A`, `eacute`, `f_i`, `uni20AC`, `a.sc`).
fn glyph_text(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    let stem = match name.find('.') {
        Some(0) => return None,
        Some(i) => &name[..i],
        None => name,
    };
    let mut out = String::new();
    for part in stem.split('_') {
        out.push_str(&component_text(part)?);
    }
    (!out.is_empty()).then_some(out)
}

const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556,
    833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500,
    556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500,
    334, 260, 334, 584,
];

const TIMES: [u16; 95] = [
    250, 333, 408, 500, 500, 833, 778, 180, 333, 333, 500, 564, 250, 333, 250, 278, 500, 500, 500, 500, 500, 500, 500,
    500, 500, 500, 278, 278, 564, 564, 564, 444, 921, 722, 667, 667, 722, 611, 556, 722, 722, 333, 389, 722, 611, 889,
    722, 722, 556, 722, 667, 556, 611, 722, 722, 944, 722, 722, 611, 333, 278, 333, 469, 500, 333, 444, 500, 444, 500,
    444, 333, 500, 500, 278, 278, 500, 278, 778, 500, 500, 500, 500, 333, 389, 278, 500, 500, 722, 500, 500, 444, 480,
    200, 480, 541,
];

/// Approximate advance of `ch` in the standard font of the given family.
pub fn standard_width(family: Family, ch: char) -> f64 {
    let idx = (ch as u32).checked_sub(32).filter(|i| *i < 95).map(|i| i as usize);
    match family {
        Family::Mono => 600.0,
        Family::Sans => idx.map_or(556.0, |i| HELVETICA[i] as f64),
        Family::Serif => idx.map_or(500.0, |i| TIMES[i] as f64),
    }
}

/// Encodes text for a standard font using WinAnsiEncoding.
pub fn encode_win_ansi(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| match c as u32 {
            32..=126 | 0xA1..=0xFF => Some(c as u32 as u8),
            0xA0 => Some(32),
            cp => WIN_80_9F.iter().position(|&w| w == cp && cp != 0x2022).map(|i| 0x80 + i as u8).or(match cp {
                0x2022 => Some(0x95),
                0x2212 => Some(b'-'),
                _ => None,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_names() {
        assert_eq!(glyph_text(b"A").as_deref(), Some("A"));
        assert_eq!(glyph_text(b"eacute").as_deref(), Some("\u{e9}"));
        assert_eq!(glyph_text(b"f_i").as_deref(), Some("fi"));
        assert_eq!(glyph_text(b"uni20AC").as_deref(), Some("\u{20ac}"));
        assert_eq!(glyph_text(b"a.sc").as_deref(), Some("a"));
        assert_eq!(glyph_text(b"g123"), None);
    }

    #[test]
    fn tounicode() {
        let cmap = parse_cmap(
            b"1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
              2 beginbfchar <0003> <0020> <0010> <00660069> endbfchar\n\
              2 beginbfrange <0024> <0026> <0041> <0030> <0031> [<0058> <0059>] endbfrange",
        );
        assert_eq!(cmap.codespace, vec![(2, 0, 0xFFFF)]);
        assert_eq!(cmap.map[&3], " ");
        assert_eq!(cmap.map[&0x10], "fi");
        assert_eq!(cmap.map[&0x25], "B");
        assert_eq!(cmap.map[&0x31], "Y");
    }

    #[test]
    fn base_encodings() {
        assert_eq!(Base::WinAnsi.decode(0x93), Some('\u{201C}'));
        assert_eq!(Base::Standard.decode(0xAE), Some('\u{FB01}'));
        assert_eq!(Base::MacRoman.decode(0x8E), Some('\u{e9}'));
        assert_eq!(encode_win_ansi("a\u{2014}\u{e9}").unwrap(), vec![b'a', 0x97, 0xE9]);
        assert_eq!(encode_win_ansi("\u{4e2d}"), None);
    }
}
