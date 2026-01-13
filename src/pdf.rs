//! PDF file layer: cross-reference loading, object access, and a writer that
//! copies every untouched object byte-for-byte.
//!
//! Object numbers, generations, file order, object-stream membership and the
//! cross-reference style of the input are all preserved. Only objects that
//! were actually modified are re-serialized.

use anyhow::{Context, Result, anyhow, bail};
use memchr::memmem;
use std::collections::{BTreeMap, HashSet};

use crate::crypt::Crypt;
use crate::filters::{self, Compression, Filter};
use crate::object::{Dict, Lexer, Object, Token, parse_object, write_dict, write_object};

#[derive(Clone, Debug)]
pub enum StreamData {
    /// Encoded bytes as they sit in the input file.
    Raw(usize, usize),
    /// Replacement content, not yet filter-encoded. `compress` mirrors whether
    /// the original stream was compressed.
    Decoded { data: Vec<u8>, compress: bool },
    /// Final bytes, ready to be written.
    Encoded(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Stream {
    pub dict: Dict,
    pub data: StreamData,
}

#[derive(Clone, Debug)]
pub enum PdfObj {
    Plain(Object),
    Stream(Stream),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    File { start: usize, end: usize },
    ObjStm { stm: u32, idx: usize },
    New,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub obj: PdfObj,
    pub generation: u16,
    pub loc: Loc,
    pub dirty: bool,
}

#[derive(Clone, Copy, Debug)]
enum XrefEntry {
    Free,
    Offset(usize, u16),
    Compressed(u32, usize),
}

struct ObjStm {
    data: Vec<u8>,
    /// (object number, start, end) within `data`, in header order.
    members: Vec<(u32, usize, usize)>,
}

pub struct Pdf {
    pub data: Vec<u8>,
    pub objects: BTreeMap<u32, Entry>,
    pub trailer: Dict,
    pub crypt: Option<Crypt>,
    pub compression: Compression,
    /// True when the input needed its cross-reference table reconstructed.
    pub recovered: bool,
    objstms: BTreeMap<u32, ObjStm>,
    xref_stream: bool,
    xref_stream_num: Option<u32>,
    password: Vec<u8>,
}

const XREF_ONLY_KEYS: [&[u8]; 8] =
    [b"Prev", b"XRefStm", b"Type", b"W", b"Index", b"Length", b"Filter", b"DecodeParms"];

impl Pdf {
    pub fn load(data: Vec<u8>, password: &[u8]) -> Result<Pdf> {
        if memmem::find(&data[..data.len().min(1024)], b"%PDF-").is_none() {
            bail!("not a PDF file (missing %PDF- header)");
        }
        let mut pdf = Pdf {
            data,
            objects: BTreeMap::new(),
            trailer: Dict::default(),
            crypt: None,
            compression: Compression::Max,
            recovered: false,
            objstms: BTreeMap::new(),
            xref_stream: false,
            xref_stream_num: None,
            password: password.to_vec(),
        };
        let normal = pdf.read_xref().and_then(|xref| pdf.load_objects(&xref, true)).and_then(|()| {
            let root = pdf.trailer.get(b"Root").and_then(Object::as_ref);
            match root.and_then(|r| pdf.dict_of(r)) {
                Some(_) => Ok(()),
                None => Err(anyhow!("trailer has no usable /Root")),
            }
        });
        if let Err(e) = normal {
            // A password problem is not damage; scanning for objects would not help.
            if pdf.trailer.has(b"Encrypt") && format!("{e:#}").contains("password") {
                return Err(e);
            }
            pdf.objects.clear();
            pdf.objstms.clear();
            pdf.crypt = None;
            pdf.recovered = true;
            let xref = pdf
                .rebuild_xref()
                .with_context(|| format!("cross-reference data is unusable ({e:#}) and recovery failed"))?;
            pdf.load_objects(&xref, false)?;
        }
        if !pdf.trailer.has(b"Root") {
            bail!("PDF has no document catalog (/Root)");
        }
        Ok(pdf)
    }

    // ---------------------------------------------------------------- xref

    fn read_xref(&mut self) -> Result<BTreeMap<u32, XrefEntry>> {
        let tail_start = self.data.len().saturating_sub(4096);
        let p = memmem::rfind(&self.data[tail_start..], b"startxref")
            .ok_or_else(|| anyhow!("startxref not found"))?;
        let mut lex = Lexer::new(&self.data, tail_start + p + 9);
        let (Token::Int(first), _, _) = lex.next() else { bail!("malformed startxref") };

        let mut xref = BTreeMap::new();
        let mut visited = HashSet::new();
        let mut queue = vec![first as usize];
        let mut first_section = true;
        while let Some(offset) = queue.pop() {
            if !visited.insert(offset) || offset >= self.data.len() {
                continue;
            }
            let mut lex = Lexer::new(&self.data, offset);
            let save = lex.pos;
            let (tok, _, _) = lex.next();
            let trailer = if tok == Token::Keyword(b"xref") {
                self.read_xref_table(&mut lex, &mut xref)?
            } else {
                lex.pos = save;
                let (num, dict) = self.read_xref_stream(offset, &mut xref)?;
                if first_section {
                    self.xref_stream = true;
                    self.xref_stream_num = Some(num);
                }
                dict
            };
            // Lookup order within one section: its own table, then /XRefStm, then /Prev.
            if let Some(prev) = trailer.get(b"Prev").and_then(Object::as_int) {
                queue.push(prev as usize);
            }
            if let Some(stm) = trailer.get(b"XRefStm").and_then(Object::as_int) {
                queue.push(stm as usize);
            }
            if first_section {
                self.trailer = trailer;
                first_section = false;
            }
        }
        Ok(xref)
    }

    fn read_xref_table(&self, lex: &mut Lexer, xref: &mut BTreeMap<u32, XrefEntry>) -> Result<Dict> {
        loop {
            let (tok, _, _) = lex.next();
            match tok {
                Token::Keyword(b"trailer") => break,
                Token::Int(start) => {
                    let (Token::Int(count), _, _) = lex.next() else { bail!("malformed xref subsection") };
                    for i in 0..count {
                        let (Token::Int(a), _, _) = lex.next() else { bail!("malformed xref entry") };
                        let (Token::Int(b), _, _) = lex.next() else { bail!("malformed xref entry") };
                        let (Token::Keyword(kind), _, _) = lex.next() else { bail!("malformed xref entry") };
                        let num = (start + i) as u32;
                        let entry = match kind {
                            b"n" => XrefEntry::Offset(a as usize, b as u16),
                            _ => XrefEntry::Free,
                        };
                        xref.entry(num).or_insert(entry);
                    }
                }
                _ => bail!("malformed xref table"),
            }
        }
        match parse_object(lex)? {
            Object::Dict(d) => Ok(d),
            _ => bail!("malformed trailer"),
        }
    }

    fn read_xref_stream(&self, offset: usize, xref: &mut BTreeMap<u32, XrefEntry>) -> Result<(u32, Dict)> {
        let (num, _, obj, _) = self.parse_indirect_at(offset, &BTreeMap::new())?;
        let PdfObj::Stream(stream) = obj else { bail!("expected cross-reference stream at {offset}") };
        if stream.dict.name(b"Type") != Some(b"XRef") {
            bail!("object at {offset} is not a cross-reference stream");
        }
        let StreamData::Raw(s, e) = stream.data else { unreachable!() };
        let data = filters::decode(&direct_filters(&stream.dict), &self.data[s..e])?;
        let w: Vec<usize> = stream
            .dict
            .get(b"W")
            .and_then(Object::as_array)
            .ok_or_else(|| anyhow!("xref stream without /W"))?
            .iter()
            .map(|o| o.as_int().unwrap_or(0).max(0) as usize)
            .collect();
        if w.len() != 3 || w.iter().sum::<usize>() == 0 {
            bail!("malformed /W in xref stream");
        }
        let size = stream.dict.get(b"Size").and_then(Object::as_int).unwrap_or(0);
        let index: Vec<i64> = match stream.dict.get(b"Index").and_then(Object::as_array) {
            Some(a) => a.iter().filter_map(Object::as_int).collect(),
            None => vec![0, size],
        };
        let row = w[0] + w[1] + w[2];
        let mut rows = data.chunks_exact(row);
        let field = |bytes: &[u8]| bytes.iter().fold(0u64, |a, &b| a << 8 | b as u64);
        for pair in index.chunks_exact(2) {
            for i in 0..pair[1] {
                let Some(r) = rows.next() else { break };
                let kind = if w[0] == 0 { 1 } else { field(&r[..w[0]]) };
                let f2 = field(&r[w[0]..w[0] + w[1]]);
                let f3 = field(&r[w[0] + w[1]..]);
                let entry = match kind {
                    0 => XrefEntry::Free,
                    1 => XrefEntry::Offset(f2 as usize, f3 as u16),
                    2 => XrefEntry::Compressed(f2 as u32, f3 as usize),
                    _ => continue,
                };
                xref.entry((pair[0] + i) as u32).or_insert(entry);
            }
        }
        Ok((num, stream.dict))
    }

    /// Reconstructs the cross-reference data by scanning for `n g obj`.
    fn rebuild_xref(&mut self) -> Result<BTreeMap<u32, XrefEntry>> {
        let data = &self.data;
        let mut xref: BTreeMap<u32, XrefEntry> = BTreeMap::new();
        let mut trailer: Option<Dict> = None;
        let mut objstms: Vec<(u32, usize)> = Vec::new();
        let mut catalog: Option<u32> = None;
        let empty = BTreeMap::new();
        let mut pos = 0;
        let finder = memmem::Finder::new(b"obj");
        while let Some(rel) = finder.find(&data[pos..]) {
            let at = pos + rel;
            pos = at + 3;
            let Some(start) = indirect_header_start(data, at) else { continue };
            let Ok((num, generation, obj, end)) = self.parse_indirect_at(start, &empty) else { continue };
            xref.insert(num, XrefEntry::Offset(start, generation));
            if matches!(&obj, PdfObj::Plain(Object::Dict(d)) if d.name(b"Type") == Some(b"Catalog") && d.has(b"Pages")) {
                catalog = Some(num);
            }
            if let PdfObj::Stream(s) = &obj {
                match s.dict.name(b"Type") {
                    Some(b"XRef") if s.dict.has(b"Root") => trailer = Some(s.dict.clone()),
                    Some(b"ObjStm") => objstms.push((num, start)),
                    _ => {}
                }
            }
            pos = end.max(pos);
        }
        // Classic trailers; the last one carrying /Root wins.
        let mut tpos = 0;
        let tfinder = memmem::Finder::new(b"trailer");
        let mut best: Option<(usize, Dict)> = None;
        while let Some(rel) = tfinder.find(&data[tpos..]) {
            let at = tpos + rel;
            tpos = at + 7;
            let mut lex = Lexer::new(data, at + 7);
            if let Ok(Object::Dict(d)) = parse_object(&mut lex) {
                if d.has(b"Root") {
                    best = Some((at, d));
                }
            }
        }
        if let Some((_, d)) = best {
            // Prefer whichever trailer source appears later in the file.
            trailer = Some(d);
        }
        // Last resort: a file with no usable trailer still has a catalog object.
        if trailer.is_none() {
            if let Some(c) = catalog {
                let mut d = Dict::default();
                d.set(b"Root", Object::Ref(c, 0));
                trailer = Some(d);
            }
        }
        self.trailer = trailer.ok_or_else(|| anyhow!("no trailer with /Root found"))?;
        self.xref_stream = false;
        self.xref_stream_num = None;
        self.setup_crypt(&xref)?;

        // Objects inside object streams: later containers override earlier ones,
        // but never an object that sits later in the file as a top-level object.
        for (stm, stm_offset) in objstms {
            let Ok((_, _, PdfObj::Stream(s), _)) = self.parse_indirect_at(stm_offset, &xref) else { continue };
            let Ok(info) = self.load_objstm(stm, &s, &xref) else { continue };
            for (idx, (num, _, _)) in info.members.iter().enumerate() {
                match xref.get(num) {
                    Some(XrefEntry::Offset(o, _)) if *o > stm_offset => {}
                    _ => {
                        xref.insert(*num, XrefEntry::Compressed(stm, idx));
                    }
                }
            }
        }
        if xref.values().any(|e| matches!(e, XrefEntry::Compressed(..))) {
            self.xref_stream = true;
        }
        Ok(xref)
    }

    // ------------------------------------------------------------- parsing

    /// Parses `n g obj ... endobj` at `offset`. Returns (num, gen, object, end offset).
    fn parse_indirect_at(
        &self,
        offset: usize,
        xref: &BTreeMap<u32, XrefEntry>,
    ) -> Result<(u32, u16, PdfObj, usize)> {
        let data = &self.data;
        let mut lex = Lexer::new(data, offset);
        let (Token::Int(num), _, _) = lex.next() else { bail!("no object at offset {offset}") };
        let (Token::Int(generation), _, _) = lex.next() else { bail!("no object at offset {offset}") };
        let (Token::Keyword(b"obj"), _, _) = lex.next() else { bail!("no object at offset {offset}") };
        if !(0..=u32::MAX as i64).contains(&num) || !(0..=65535).contains(&generation) {
            bail!("invalid object header at offset {offset}");
        }
        let obj = parse_object(&mut lex)?;
        let after_obj = lex.pos;
        let (tok, tok_start, tok_end) = lex.next();
        match tok {
            Token::Keyword(b"endobj") => Ok((num as u32, generation as u16, PdfObj::Plain(obj), tok_end)),
            Token::Keyword(b"stream") => {
                let Object::Dict(dict) = obj else { bail!("stream without dictionary at {offset}") };
                let mut start = tok_end;
                if data.get(start) == Some(&b'\r') {
                    start += 1;
                }
                if data.get(start) == Some(&b'\n') {
                    start += 1;
                }
                let declared = match dict.get(b"Length") {
                    Some(Object::Int(n)) => Some(*n),
                    Some(Object::Ref(n, _)) => match xref.get(n) {
                        Some(XrefEntry::Offset(o, _)) => self.peek_int(*o),
                        _ => None,
                    },
                    _ => None,
                };
                let mut end = None;
                if let Some(len) = declared {
                    let e = start.saturating_add(len.max(0) as usize);
                    if e <= data.len() {
                        let mut l = Lexer::new(data, e);
                        if let (Token::Keyword(b"endstream"), _, _) = l.next() {
                            end = Some(e);
                        }
                    }
                }
                let end = match end {
                    Some(e) => e,
                    None => {
                        let p = memmem::find(&data[start..], b"endstream")
                            .ok_or_else(|| anyhow!("unterminated stream at {offset}"))?;
                        let mut e = start + p;
                        if e > start && data[e - 1] == b'\n' {
                            e -= 1;
                        }
                        if e > start && data[e - 1] == b'\r' {
                            e -= 1;
                        }
                        e
                    }
                };
                let mut l = Lexer::new(data, end);
                let (_, _, es_end) = l.next(); // endstream
                let (t, _, eo_end) = l.next();
                let obj_end = if t == Token::Keyword(b"endobj") { eo_end } else { es_end };
                let stream = Stream { dict, data: StreamData::Raw(start, end) };
                Ok((num as u32, generation as u16, PdfObj::Stream(stream), obj_end))
            }
            // Missing endobj: accept the object, ending where the next token starts.
            _ => {
                let _ = tok_start;
                Ok((num as u32, generation as u16, PdfObj::Plain(obj), after_obj))
            }
        }
    }

    fn peek_int(&self, offset: usize) -> Option<i64> {
        let mut lex = Lexer::new(&self.data, offset);
        let (Token::Int(_), _, _) = lex.next() else { return None };
        let (Token::Int(_), _, _) = lex.next() else { return None };
        let (Token::Keyword(b"obj"), _, _) = lex.next() else { return None };
        match lex.next().0 {
            Token::Int(n) => Some(n),
            _ => None,
        }
    }

    fn setup_crypt(&mut self, xref: &BTreeMap<u32, XrefEntry>) -> Result<()> {
        let Some(enc) = self.trailer.get(b"Encrypt") else { return Ok(()) };
        let dict = match enc {
            Object::Dict(d) => d.clone(),
            Object::Ref(n, _) => match xref.get(n) {
                Some(XrefEntry::Offset(o, _)) => match self.parse_indirect_at(*o, xref)?.2 {
                    PdfObj::Plain(Object::Dict(d)) => d,
                    _ => bail!("malformed /Encrypt dictionary"),
                },
                _ => bail!("missing /Encrypt dictionary"),
            },
            _ => bail!("malformed /Encrypt entry"),
        };
        let id0 = self
            .trailer
            .get(b"ID")
            .and_then(Object::as_array)
            .and_then(|a| a.first())
            .and_then(Object::as_str)
            .unwrap_or(b"")
            .to_vec();
        self.crypt = Some(Crypt::new(&dict, &id0, &self.password)?);
        Ok(())
    }

    fn load_objects(&mut self, xref: &BTreeMap<u32, XrefEntry>, strict: bool) -> Result<()> {
        if strict {
            self.setup_crypt(xref)?;
        }
        let encrypt_num = self.trailer.get(b"Encrypt").and_then(Object::as_ref);
        let mut needed_stms: Vec<u32> = Vec::new();
        for (&num, entry) in xref {
            match *entry {
                XrefEntry::Free => {}
                XrefEntry::Offset(offset, generation) => {
                    if offset == 0 && generation == 65535 {
                        continue;
                    }
                    match self.parse_indirect_at(offset, xref) {
                        Ok((n, g, mut obj, end)) if n == num => {
                            if let (Some(c), true) = (&self.crypt, Some(num) != encrypt_num) {
                                let is_xref =
                                    matches!(&obj, PdfObj::Stream(s) if s.dict.name(b"Type") == Some(b"XRef"));
                                if !is_xref {
                                    match &mut obj {
                                        PdfObj::Plain(o) => c.decrypt_object(num, g, o),
                                        PdfObj::Stream(s) => c.decrypt_dict(num, g, &mut s.dict),
                                    }
                                }
                            }
                            let loc = Loc::File { start: offset, end };
                            self.objects.insert(num, Entry { obj, generation: g, loc, dirty: false });
                        }
                        Ok((n, ..)) if strict => {
                            bail!("xref entry for object {num} points at object {n}")
                        }
                        Err(e) if strict => return Err(e.context(format!("object {num}"))),
                        _ => {}
                    }
                }
                XrefEntry::Compressed(stm, _) => {
                    if !needed_stms.contains(&stm) {
                        needed_stms.push(stm);
                    }
                }
            }
        }
        for stm in needed_stms {
            let info = match self.objects.get(&stm).map(|e| &e.obj) {
                Some(PdfObj::Stream(s)) => self.load_objstm(stm, s, xref),
                _ => Err(anyhow!("object stream {stm} is missing")),
            };
            match info {
                Ok(info) => {
                    self.objstms.insert(stm, info);
                }
                Err(e) if strict => return Err(e),
                Err(_) => {}
            }
        }
        for (&num, entry) in xref {
            let XrefEntry::Compressed(stm, idx) = *entry else { continue };
            let Some(info) = self.objstms.get(&stm) else { continue };
            let member = info.members.get(idx).filter(|m| m.0 == num).copied();
            // Some writers store a wrong index; fall back to searching by number.
            let (idx, member) = match member {
                Some(m) => (idx, m),
                None => match info.members.iter().position(|m| m.0 == num) {
                    Some(i) => (i, info.members[i]),
                    None if strict => bail!("object {num} not found in object stream {stm}"),
                    None => continue,
                },
            };
            let mut lex = Lexer::new(&info.data[..member.2], member.1);
            match parse_object(&mut lex) {
                Ok(obj) => {
                    let loc = Loc::ObjStm { stm, idx };
                    self.objects
                        .insert(num, Entry { obj: PdfObj::Plain(obj), generation: 0, loc, dirty: false });
                }
                Err(e) if strict => return Err(e.context(format!("object {num} in object stream {stm}"))),
                Err(_) => {}
            }
        }
        Ok(())
    }

    fn load_objstm(&self, num: u32, s: &Stream, xref: &BTreeMap<u32, XrefEntry>) -> Result<ObjStm> {
        let _ = xref;
        let generation = self.objects.get(&num).map(|e| e.generation).unwrap_or(0);
        let data = self.decode_stream(num, generation, s)?;
        let n = s.dict.get(b"N").and_then(Object::as_int).unwrap_or(0).max(0) as usize;
        let first = s.dict.get(b"First").and_then(Object::as_int).unwrap_or(0).max(0) as usize;
        let mut lex = Lexer::new(&data, 0);
        let mut heads = Vec::with_capacity(n);
        for _ in 0..n {
            let (Token::Int(onum), _, _) = lex.next() else { bail!("malformed object stream {num}") };
            let (Token::Int(off), _, _) = lex.next() else { bail!("malformed object stream {num}") };
            heads.push((onum as u32, first + off.max(0) as usize));
        }
        let mut starts: Vec<usize> = heads.iter().map(|h| h.1).collect();
        starts.sort_unstable();
        let members = heads
            .iter()
            .map(|&(onum, start)| {
                let end = starts.iter().copied().find(|&s| s > start).unwrap_or(data.len());
                (onum, start.min(data.len()), end.min(data.len()))
            })
            .collect();
        Ok(ObjStm { data, members })
    }

    // -------------------------------------------------------------- access

    pub fn get(&self, num: u32) -> Option<&PdfObj> {
        self.objects.get(&num).map(|e| &e.obj)
    }

    /// Follows indirect references to non-stream objects.
    pub fn resolve<'a>(&'a self, mut obj: &'a Object) -> &'a Object {
        for _ in 0..32 {
            match obj {
                Object::Ref(n, _) => match self.get(*n) {
                    Some(PdfObj::Plain(o)) => obj = o,
                    Some(PdfObj::Stream(_)) => return obj,
                    None => return &Object::Null,
                },
                _ => return obj,
            }
        }
        &Object::Null
    }

    /// The dictionary of object `num`, whether it is a plain dict or a stream.
    pub fn dict_of(&self, num: u32) -> Option<&Dict> {
        match self.get(num)? {
            PdfObj::Plain(Object::Dict(d)) => Some(d),
            PdfObj::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    pub fn dict_mut(&mut self, num: u32) -> Option<&mut Dict> {
        let e = self.objects.get_mut(&num)?;
        let d = match &mut e.obj {
            PdfObj::Plain(Object::Dict(d)) => d,
            PdfObj::Stream(s) => &mut s.dict,
            _ => return None,
        };
        e.dirty = true;
        Some(d)
    }

    pub fn obj_mut(&mut self, num: u32) -> Option<&mut Object> {
        let e = self.objects.get_mut(&num)?;
        match &mut e.obj {
            PdfObj::Plain(o) => {
                e.dirty = true;
                Some(o)
            }
            _ => None,
        }
    }

    /// Looks up `key` in `dict` and resolves it to a dictionary (direct, indirect or stream dict).
    pub fn dict_at<'a>(&'a self, dict: &'a Dict, key: &[u8]) -> Option<&'a Dict> {
        match dict.get(key)? {
            Object::Dict(d) => Some(d),
            Object::Ref(n, _) => self.dict_of(*n),
            _ => None,
        }
    }

    pub fn add(&mut self, obj: Object) -> u32 {
        let num = self.next_num();
        self.objects
            .insert(num, Entry { obj: PdfObj::Plain(obj), generation: 0, loc: Loc::New, dirty: true });
        num
    }

    fn next_num(&self) -> u32 {
        let max = self.objects.keys().next_back().copied().unwrap_or(0);
        max.max(self.xref_stream_num.unwrap_or(0)) + 1
    }

    pub fn is_dirty(&self) -> bool {
        self.objects.values().any(|e| e.dirty)
    }

    fn filters_of(&self, dict: &Dict) -> Vec<Filter> {
        let names: Vec<Vec<u8>> = match self.resolve(dict.get(b"Filter").unwrap_or(&Object::Null)) {
            Object::Name(n) => vec![n.clone()],
            Object::Array(a) => {
                a.iter().filter_map(|o| self.resolve(o).as_name().map(<[u8]>::to_vec)).collect()
            }
            _ => vec![],
        };
        let parms: Vec<Option<Dict>> = match self.resolve(dict.get(b"DecodeParms").unwrap_or(&Object::Null)) {
            Object::Dict(d) => vec![Some(d.clone())],
            Object::Array(a) => a.iter().map(|o| self.resolve(o).as_dict().cloned()).collect(),
            _ => vec![],
        };
        names
            .into_iter()
            .enumerate()
            .map(|(i, name)| Filter { name, parms: parms.get(i).cloned().flatten() })
            .collect()
    }

    fn decode_stream(&self, num: u32, generation: u16, s: &Stream) -> Result<Vec<u8>> {
        match &s.data {
            StreamData::Decoded { data, .. } => Ok(data.clone()),
            StreamData::Encoded(_) => bail!("stream {num} was already finalized"),
            StreamData::Raw(a, b) => {
                let raw = &self.data[*a..*b];
                let filters = self.filters_of(&s.dict);
                match &self.crypt {
                    Some(c) if s.dict.name(b"Type") != Some(b"XRef") => {
                        let plain = c.decrypt_stream(num, generation, &s.dict, raw)?;
                        filters::decode(&filters, &plain)
                    }
                    _ => filters::decode(&filters, raw),
                }
            }
        }
    }

    /// Fully decoded contents of stream `num`.
    pub fn stream_data(&self, num: u32) -> Result<Vec<u8>> {
        let e = self.objects.get(&num).ok_or_else(|| anyhow!("object {num} not found"))?;
        match &e.obj {
            PdfObj::Stream(s) => {
                self.decode_stream(num, e.generation, s).with_context(|| format!("decoding stream {num}"))
            }
            _ => bail!("object {num} is not a stream"),
        }
    }

    /// Replaces the decoded contents of stream `num`.
    pub fn set_stream_data(&mut self, num: u32, data: Vec<u8>) -> Result<()> {
        let e = self.objects.get_mut(&num).ok_or_else(|| anyhow!("object {num} not found"))?;
        let PdfObj::Stream(s) = &mut e.obj else { bail!("object {num} is not a stream") };
        let compress = match &s.data {
            StreamData::Decoded { compress, .. } => *compress,
            _ => s.dict.has(b"Filter"),
        };
        s.data = StreamData::Decoded { data, compress };
        e.dirty = true;
        Ok(())
    }

    /// Losslessly re-deflates untouched Flate streams (largest first) until at
    /// least `need` bytes are saved. The streams decode to exactly the same
    /// bytes and keep the same filter; only the Deflate encoding gets tighter.
    /// Returns (bytes saved, streams recompressed).
    pub fn recompress_to_save(&mut self, need: usize) -> (usize, usize) {
        let mut candidates: Vec<(usize, u32)> = self
            .objects
            .iter()
            .filter_map(|(&num, e)| {
                let PdfObj::Stream(s) = &e.obj else { return None };
                let StreamData::Raw(a, b) = s.data else { return None };
                let flate_only = match s.dict.get(b"Filter") {
                    Some(Object::Name(n)) => n == b"FlateDecode",
                    Some(Object::Array(f)) => f.len() == 1 && f[0].as_name() == Some(b"FlateDecode"),
                    _ => false,
                };
                let plain_length = match s.dict.get(b"Length") {
                    Some(Object::Int(_)) => true,
                    Some(Object::Ref(n, _)) => matches!(self.get(*n), Some(PdfObj::Plain(Object::Int(_)))),
                    _ => false,
                };
                let special = matches!(s.dict.name(b"Type"), Some(b"XRef" | b"ObjStm"));
                let size = b - a;
                (flate_only && plain_length && !special && !e.dirty && (64..8 << 20).contains(&size)).then_some((size, num))
            })
            .collect();
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        let (mut saved, mut count) = (0, 0);
        for (size, num) in candidates.into_iter().take(200) {
            if saved >= need {
                break;
            }
            let e = &self.objects[&num];
            let PdfObj::Stream(s) = &e.obj else { continue };
            let StreamData::Raw(a, b) = s.data else { continue };
            let raw = &self.data[a..b];
            let plain = match &self.crypt {
                Some(c) => match c.decrypt_stream(num, e.generation, &s.dict, raw) {
                    Ok(p) => p,
                    Err(_) => continue,
                },
                None => raw.to_vec(),
            };
            // Only touch streams that inflate cleanly, and prove the round trip.
            let Some(inflated) = filters::inflate_strict(&plain) else { continue };
            let packed = filters::deflate(&inflated, Compression::Max);
            if filters::inflate_strict(&packed).as_deref() != Some(&inflated[..]) {
                continue;
            }
            let bytes = match &self.crypt {
                Some(c) => match c.encrypt_stream(num, e.generation, &s.dict, &packed) {
                    Ok(b) => b,
                    Err(_) => continue,
                },
                None => packed,
            };
            if bytes.len() >= size {
                continue;
            }
            saved += size - bytes.len();
            count += 1;
            let len = Object::Int(bytes.len() as i64);
            let length_ref = s.dict.get(b"Length").and_then(Object::as_ref);
            let e = self.objects.get_mut(&num).unwrap();
            let PdfObj::Stream(s) = &mut e.obj else { unreachable!() };
            s.data = StreamData::Encoded(bytes);
            e.dirty = true;
            match length_ref {
                Some(n) => *self.obj_mut(n).unwrap() = len,
                None => {
                    let PdfObj::Stream(s) = &mut self.objects.get_mut(&num).unwrap().obj else { unreachable!() };
                    s.dict.set(b"Length", len);
                }
            }
        }
        (saved, count)
    }

    // -------------------------------------------------------------- writer

    /// Filter-encodes (and encrypts) every replaced stream and fixes up lengths.
    fn finalize_streams(&mut self) -> Result<()> {
        let nums: Vec<u32> = self
            .objects
            .iter()
            .filter(|(_, e)| matches!(&e.obj, PdfObj::Stream(s) if matches!(s.data, StreamData::Decoded { .. })))
            .map(|(n, _)| *n)
            .collect();
        for num in nums {
            let mode = self.compression;
            let e = self.objects.get_mut(&num).unwrap();
            let generation = e.generation;
            let PdfObj::Stream(s) = &mut e.obj else { unreachable!() };
            let StreamData::Decoded { data, compress } = std::mem::replace(&mut s.data, StreamData::Encoded(vec![]))
            else {
                unreachable!()
            };
            let mut bytes = if compress {
                s.dict.set(b"Filter", Object::Name(b"FlateDecode".to_vec()));
                filters::deflate(&data, mode)
            } else {
                s.dict.remove(b"Filter");
                data
            };
            s.dict.remove(b"DecodeParms");
            s.dict.remove(b"DL");
            if let Some(c) = &self.crypt {
                bytes = c.encrypt_stream(num, generation, &s.dict, &bytes)?;
            }
            let len = bytes.len() as i64;
            let length_ref = match s.dict.get(b"Length") {
                Some(Object::Ref(n, _)) => Some(*n),
                _ => None,
            };
            s.data = StreamData::Encoded(bytes);
            match length_ref {
                // Keep an indirect /Length indirect when it points at a plain integer.
                Some(n) if matches!(self.get(n), Some(PdfObj::Plain(Object::Int(_)))) => {
                    *self.obj_mut(n).unwrap() = Object::Int(len);
                }
                _ => {
                    let PdfObj::Stream(s) = &mut self.objects.get_mut(&num).unwrap().obj else { unreachable!() };
                    s.dict.set(b"Length", Object::Int(len));
                }
            }
        }
        Ok(())
    }

    fn serialize_plain(&self, num: u32, generation: u16, obj: &Object, encrypt: bool) -> Vec<u8> {
        let mut out = Vec::new();
        match (&self.crypt, encrypt) {
            (Some(c), true) => {
                let mut o = obj.clone();
                c.encrypt_object(num, generation, &mut o);
                write_object(&mut out, &o);
            }
            _ => write_object(&mut out, obj),
        }
        out
    }

    fn write_entry(&self, out: &mut Vec<u8>, num: u32, e: &Entry) {
        out.extend_from_slice(format!("{num} {} obj\n", e.generation).as_bytes());
        let encrypt = self.trailer.get(b"Encrypt").and_then(Object::as_ref) != Some(num);
        match &e.obj {
            PdfObj::Plain(o) => out.extend_from_slice(&self.serialize_plain(num, e.generation, o, encrypt)),
            PdfObj::Stream(s) => {
                let mut dict = s.dict.clone();
                if let Some(c) = &self.crypt {
                    c.encrypt_dict(num, e.generation, &mut dict);
                }
                write_dict(out, &dict);
                out.extend_from_slice(b"\nstream\n");
                match &s.data {
                    StreamData::Raw(a, b) => out.extend_from_slice(&self.data[*a..*b]),
                    StreamData::Encoded(d) => out.extend_from_slice(d),
                    StreamData::Decoded { .. } => unreachable!("streams are finalized before writing"),
                }
                out.extend_from_slice(b"\nendstream");
            }
        }
        out.extend_from_slice(b"\nendobj\n");
    }

    pub fn write(&mut self) -> Result<Vec<u8>> {
        self.finalize_streams()?;

        // Objects whose byte offsets are baked into their content cannot survive a rewrite.
        let mut drop: HashSet<u32> = HashSet::new();
        for (&num, e) in &self.objects {
            match &e.obj {
                PdfObj::Stream(s) if s.dict.name(b"Type") == Some(b"XRef") => {
                    drop.insert(num);
                }
                PdfObj::Plain(Object::Dict(d)) if d.has(b"Linearized") => {
                    drop.insert(num);
                    let hint = d.get(b"H").and_then(Object::as_array).and_then(|a| a.first()).and_then(Object::as_int);
                    if let Some(h) = hint {
                        for (&n2, e2) in &self.objects {
                            if matches!(e2.loc, Loc::File { start, .. } if start as i64 == h) {
                                drop.insert(n2);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Decide which object streams must be rebuilt.
        let mut rebuilt: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (&stm, info) in &self.objstms {
            let live: Vec<u32> = info
                .members
                .iter()
                .enumerate()
                .filter(|(idx, m)| {
                    self.objects.get(&m.0).is_some_and(|e| e.loc == Loc::ObjStm { stm, idx: *idx })
                })
                .map(|(_, m)| m.0)
                .collect();
            let dirty = live.iter().any(|n| self.objects[n].dirty);
            if live.is_empty() {
                drop.insert(stm);
            } else if dirty || live.len() != info.members.len() {
                rebuilt.insert(stm, live);
            }
        }

        let header_start = memmem::find(&self.data[..self.data.len().min(1024)], b"%PDF-").unwrap_or(0);
        let mut out: Vec<u8> = Vec::with_capacity(self.data.len());
        let mut p = header_start;
        // Header line plus the customary binary-marker comment line.
        for _ in 0..2 {
            if self.data.get(p) != Some(&b'%') {
                break;
            }
            let eol = self.data[p..].iter().position(|&b| b == b'\n' || b == b'\r').map_or(self.data.len(), |i| p + i);
            out.extend_from_slice(&self.data[p..eol]);
            out.push(b'\n');
            p = eol;
            while matches!(self.data.get(p), Some(b'\n' | b'\r')) {
                p += 1;
            }
        }

        let mut order: Vec<(usize, u32)> = self
            .objects
            .iter()
            .filter_map(|(&n, e)| match e.loc {
                Loc::File { start, .. } => Some((start, n)),
                _ => None,
            })
            .collect();
        order.sort_unstable();
        let new_objs: Vec<u32> =
            self.objects.iter().filter(|(_, e)| e.loc == Loc::New).map(|(&n, _)| n).collect();

        let mut table: BTreeMap<u32, XrefEntry> = BTreeMap::new();
        for num in order.iter().map(|o| o.1).chain(new_objs) {
            if drop.contains(&num) {
                continue;
            }
            let e = &self.objects[&num];
            table.insert(num, XrefEntry::Offset(out.len(), e.generation));
            if let Some(live) = rebuilt.get(&num) {
                self.write_objstm(&mut out, num, e, live, &mut table)?;
            } else if e.dirty {
                self.write_entry(&mut out, num, e);
            } else {
                let Loc::File { start, end } = e.loc else { unreachable!() };
                out.extend_from_slice(&self.data[start..end]);
                out.push(b'\n');
                if let Some(info) = self.objstms.get(&num) {
                    for (idx, m) in info.members.iter().enumerate() {
                        table.insert(m.0, XrefEntry::Compressed(num, idx));
                    }
                }
            }
        }

        let use_stream = self.xref_stream || table.values().any(|e| matches!(e, XrefEntry::Compressed(..)));
        let mut trailer = self.trailer.clone();
        for key in XREF_ONLY_KEYS {
            trailer.remove(key);
        }
        let xref_offset = out.len();
        if use_stream {
            let xnum = self.xref_stream_num.unwrap_or_else(|| self.next_num());
            table.insert(xnum, XrefEntry::Offset(xref_offset, 0));
            let size = table.keys().next_back().unwrap() + 1;
            let max_off = xref_offset as u64;
            let max_f3 = table
                .values()
                .map(|e| match e {
                    XrefEntry::Compressed(_, i) => *i as u64,
                    XrefEntry::Offset(_, g) => *g as u64,
                    XrefEntry::Free => 0,
                })
                .max()
                .unwrap_or(0)
                .max(255);
            let bytes_for = |v: u64| ((64 - v.leading_zeros() as usize).div_ceil(8)).max(1);
            let (w2, w3) = (bytes_for(max_off.max(size as u64)), bytes_for(max_f3).min(2).max(1));
            let w3 = if max_f3 > 0xFFFF { bytes_for(max_f3) } else { w3 };
            let mut rows = Vec::with_capacity(size as usize * (1 + w2 + w3));
            let sections = sections(&table);
            for num in sections.iter().flat_map(|&(start, count)| start..start + count) {
                let (kind, f2, f3) = match table.get(&num) {
                    Some(XrefEntry::Offset(o, g)) => (1u8, *o as u64, *g as u64),
                    Some(XrefEntry::Compressed(s, i)) => (2, *s as u64, *i as u64),
                    _ => (0, 0, if num == 0 { 65535 } else { 0 }),
                };
                rows.push(kind);
                rows.extend_from_slice(&f2.to_be_bytes()[8 - w2..]);
                rows.extend_from_slice(&f3.to_be_bytes()[8 - w3..]);
            }
            let cols = 1 + w2 + w3;
            // The row predictor pays off on big tables but its parameters cost ~40 bytes.
            let predicted = filters::deflate(&filters::png_up_encode(&rows, cols), self.compression);
            let direct = filters::deflate(&rows, self.compression);
            let use_predictor = predicted.len() + 40 < direct.len();
            let body = if use_predictor { predicted } else { direct };
            let mut dict = Dict::default();
            dict.set(b"Type", Object::Name(b"XRef".to_vec()));
            dict.set(b"Size", Object::Int(size as i64));
            dict.set(b"W", Object::Array(vec![Object::Int(1), Object::Int(w2 as i64), Object::Int(w3 as i64)]));
            if sections.len() > 1 {
                let index = sections.iter().flat_map(|&(a, n)| [Object::Int(a as i64), Object::Int(n as i64)]);
                dict.set(b"Index", Object::Array(index.collect()));
            }
            for (k, v) in trailer.0 {
                if k != b"Size" {
                    dict.set(&k, v);
                }
            }
            dict.set(b"Filter", Object::Name(b"FlateDecode".to_vec()));
            if use_predictor {
                let mut parms = Dict::default();
                parms.set(b"Columns", Object::Int(cols as i64));
                parms.set(b"Predictor", Object::Int(12));
                dict.set(b"DecodeParms", Object::Dict(parms));
            }
            dict.set(b"Length", Object::Int(body.len() as i64));
            out.extend_from_slice(format!("{xnum} 0 obj\n").as_bytes());
            write_dict(&mut out, &dict);
            out.extend_from_slice(b"\nstream\n");
            out.extend_from_slice(&body);
            out.extend_from_slice(b"\nendstream\nendobj\n");
        } else {
            let size = table.keys().next_back().map_or(1, |n| n + 1);
            out.extend_from_slice(b"xref\n");
            for (start, count) in sections(&table) {
                out.extend_from_slice(format!("{start} {count}\n").as_bytes());
                for num in start..start + count {
                    let line = match table.get(&num) {
                        Some(XrefEntry::Offset(o, g)) => format!("{o:010} {g:05} n \n"),
                        _ => format!("{:010} {:05} f \n", 0, if num == 0 { 65535 } else { 0 }),
                    };
                    out.extend_from_slice(line.as_bytes());
                }
            }
            trailer.set(b"Size", Object::Int(size as i64));
            out.extend_from_slice(b"trailer\n");
            write_dict(&mut out, &trailer);
            out.push(b'\n');
        }
        out.extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());
        Ok(out)
    }

    fn write_objstm(
        &self,
        out: &mut Vec<u8>,
        stm: u32,
        e: &Entry,
        live: &[u32],
        table: &mut BTreeMap<u32, XrefEntry>,
    ) -> Result<()> {
        let info = &self.objstms[&stm];
        let PdfObj::Stream(s) = &e.obj else { bail!("object stream {stm} is not a stream") };
        let mut head = Vec::new();
        let mut body = Vec::new();
        for (new_idx, num) in live.iter().enumerate() {
            let member = &self.objects[num];
            head.extend_from_slice(format!("{num} {} ", body.len()).as_bytes());
            if member.dirty {
                let PdfObj::Plain(o) = &member.obj else { bail!("object {num} cannot live in an object stream") };
                // Strings inside object streams are protected by the stream's own encryption.
                body.extend_from_slice(&self.serialize_plain(*num, 0, o, false));
            } else {
                let m = info.members.iter().find(|m| m.0 == *num).unwrap();
                let raw = &info.data[m.1..m.2];
                let trimmed = raw.len() - raw.iter().rev().take_while(|b| crate::object::is_whitespace(**b)).count();
                body.extend_from_slice(&raw[..trimmed]);
            }
            body.push(b'\n');
            table.insert(*num, XrefEntry::Compressed(stm, new_idx));
        }
        let first = head.len();
        head.extend_from_slice(&body);
        let mut dict = s.dict.clone();
        dict.set(b"N", Object::Int(live.len() as i64));
        dict.set(b"First", Object::Int(first as i64));
        dict.set(b"Filter", Object::Name(b"FlateDecode".to_vec()));
        dict.remove(b"DecodeParms");
        let mut bytes = filters::deflate(&head, self.compression);
        if let Some(c) = &self.crypt {
            bytes = c.encrypt_stream(stm, e.generation, &dict, &bytes)?;
        }
        dict.set(b"Length", Object::Int(bytes.len() as i64));
        out.extend_from_slice(format!("{stm} {} obj\n", e.generation).as_bytes());
        write_dict(out, &dict);
        out.extend_from_slice(b"\nstream\n");
        out.extend_from_slice(&bytes);
        out.extend_from_slice(b"\nendstream\nendobj\n");
        Ok(())
    }
}

/// Cross-reference subsections as (first number, count): object 0 plus every
/// run of numbers in use. Small holes are bridged with free entries, since a
/// new subsection header costs about as much as one entry.
fn sections(table: &BTreeMap<u32, XrefEntry>) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = vec![(0, 1)];
    for &num in table.keys().filter(|&&n| n != 0) {
        let last = out.last_mut().unwrap();
        let end = last.0 + last.1;
        if num <= end + 1 {
            last.1 = num + 1 - last.0;
        } else {
            out.push((num, 1));
        }
    }
    out
}

fn direct_filters(dict: &Dict) -> Vec<Filter> {
    let names: Vec<Vec<u8>> = match dict.get(b"Filter") {
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(a)) => a.iter().filter_map(|o| o.as_name().map(<[u8]>::to_vec)).collect(),
        _ => vec![],
    };
    let parms: Vec<Option<Dict>> = match dict.get(b"DecodeParms") {
        Some(Object::Dict(d)) => vec![Some(d.clone())],
        Some(Object::Array(a)) => a.iter().map(|o| o.as_dict().cloned()).collect(),
        _ => vec![],
    };
    names.into_iter().enumerate().map(|(i, name)| Filter { name, parms: parms.get(i).cloned().flatten() }).collect()
}

/// If `obj_at` is the position of an `obj` keyword in a valid `n g obj`
/// header, returns the offset where the object number starts.
fn indirect_header_start(data: &[u8], obj_at: usize) -> Option<usize> {
    use crate::object::{is_regular, is_whitespace};
    if data.get(obj_at + 3).is_some_and(|&b| is_regular(b)) {
        return None;
    }
    let mut p = obj_at;
    let skip = |p: &mut usize, pred: fn(u8) -> bool| -> usize {
        let before = *p;
        while *p > 0 && pred(data[*p - 1]) {
            *p -= 1;
        }
        before - *p
    };
    if skip(&mut p, is_whitespace) == 0 {
        return None;
    }
    if skip(&mut p, |b| b.is_ascii_digit()) == 0 {
        return None;
    }
    if skip(&mut p, is_whitespace) == 0 {
        return None;
    }
    if skip(&mut p, |b| b.is_ascii_digit()) == 0 {
        return None;
    }
    if p > 0 && is_regular(data[p - 1]) {
        return None;
    }
    Some(p)
}
