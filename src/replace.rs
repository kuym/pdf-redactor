//! The search-and-replace engine.
//!
//! Page text is reconstructed from glyph positions (so words split across
//! operators, kerned arrays or lines still match), matches are mapped back to
//! the glyphs that drew them, and only the affected text operators are
//! rewritten.

use anyhow::Result;
use std::collections::{HashMap, HashSet};

use crate::content::{self, FontTable, Item, ShowKind, TextOp};
use crate::doc::{Fonts, Unit};
use crate::font::{Family, Font, encode_win_ansi, standard_width};
use crate::object::{Object, StrFmt, fmt_num, is_regular, is_whitespace, write_name, write_string};
use crate::pdf::{Pdf, PdfObj};
use crate::script::Rule;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Let replacement text take its natural width.
    None,
    /// Condense an operator's text only when it would run into text that follows on the line.
    Auto,
    /// Never let an operator's text grow wider than it originally was.
    Strict,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Use a standard font when the original font lacks a needed glyph.
    pub fallback: bool,
    /// Force a particular family for the fallback font instead of matching the original.
    pub fallback_family: Option<Family>,
    pub fit: Fit,
    /// Keep the glyphs of text that the replacement leaves unchanged.
    pub minimal: bool,
}

#[derive(Default, Debug)]
pub struct RuleStats {
    pub text: usize,
    pub links: usize,
    pub fallback: usize,
    pub condensed: usize,
    pub actual_text: usize,
    pub skipped: Vec<String>,
}

const NO_GLYPH: u32 = u32::MAX;

/// Page text in reading (stream) order plus the mapping back to glyphs.
pub struct Logical {
    pub text: String,
    /// Glyph index of each char, or `NO_GLYPH` for inferred spaces and line breaks.
    char_glyph: Vec<u32>,
    /// Byte offset of each char in `text`.
    char_byte: Vec<usize>,
    /// (op, item, glyph-in-item) of each glyph.
    glyphs: Vec<(u32, u32, u32)>,
    /// Char range of each glyph.
    glyph_chars: Vec<(u32, u32)>,
    /// First global glyph index of each op.
    op_start: Vec<usize>,
}

fn dot(a: [f64; 2], b: [f64; 2]) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

/// Position of `to` relative to `from` in ems along (`.0`) and across (`.1`) the line of `op`.
fn offset_em(op: &TextOp, other_dir: [f64; 2], from: [f64; 2], to: [f64; 2]) -> Option<(f64, f64)> {
    let (dl, nl) = (dot(op.dir, op.dir).sqrt(), dot(op.norm, op.norm).sqrt());
    if dl < 1e-9 || nl < 1e-9 {
        return None;
    }
    // Measure in the larger of the two font sizes so superscripts stay on their line.
    let em_along = dl.max(dot(other_dir, other_dir).sqrt());
    let em_across = nl * em_along / dl;
    let d = [to[0] - from[0], to[1] - from[1]];
    Some((dot(d, op.dir) / dl / em_along, dot(d, op.norm) / nl / em_across))
}

pub fn build_text(ops: &[TextOp], fonts: &[Font]) -> Logical {
    let mut l = Logical {
        text: String::new(),
        char_glyph: Vec::new(),
        char_byte: Vec::new(),
        glyphs: Vec::new(),
        glyph_chars: Vec::new(),
        op_start: Vec::with_capacity(ops.len()),
    };
    // (op index, end position) of the previous glyph.
    let mut prev: Option<(usize, [f64; 2])> = None;
    for (oi, op) in ops.iter().enumerate() {
        l.op_start.push(l.glyphs.len());
        let font = op.font.map(|f| &fonts[f]);
        for (ii, item) in op.items.iter().enumerate() {
            let Item::Str { glyphs, .. } = item else { continue };
            for (gi, g) in glyphs.iter().enumerate() {
                let text = font.and_then(|f| f.text(g.code)).unwrap_or("\u{FFFD}");
                if let Some((poi, pend)) = prev {
                    let sep = match offset_em(&ops[poi], op.dir, pend, g.p0) {
                        Some((along, across)) => {
                            if across.abs() > 0.5 || along < -0.5 {
                                Some('\n')
                            } else if along > 0.15 {
                                Some(' ')
                            } else {
                                None
                            }
                        }
                        None => None,
                    };
                    let prev_ws = l.text.chars().next_back().is_none_or(char::is_whitespace);
                    let next_ws = text.chars().next().is_some_and(char::is_whitespace);
                    match sep {
                        Some('\n') => l.push_char('\n', NO_GLYPH),
                        Some(c) if !prev_ws && !next_ws => l.push_char(c, NO_GLYPH),
                        _ => {}
                    }
                }
                let index = l.glyphs.len() as u32;
                let first_char = l.char_glyph.len() as u32;
                for c in text.chars() {
                    l.push_char(c, index);
                }
                l.glyphs.push((oi as u32, ii as u32, gi as u32));
                l.glyph_chars.push((first_char, l.char_glyph.len() as u32));
                prev = Some((oi, g.p1));
            }
        }
    }
    l
}

impl Logical {
    fn push_char(&mut self, c: char, glyph: u32) {
        self.char_byte.push(self.text.len());
        self.char_glyph.push(glyph);
        self.text.push(c);
    }

    fn char_at_byte(&self, byte: usize) -> usize {
        self.char_byte.partition_point(|&b| b < byte)
    }

    fn char_at(&self, index: usize) -> char {
        self.text[self.char_byte[index]..].chars().next().unwrap_or(' ')
    }

    /// Shrinks the match `[cs, ce)` by the whole glyphs at either end that
    /// `repl` would reproduce unchanged. Returns the remaining range and
    /// replacement; at least one glyph is kept in range whenever any
    /// replacement text remains, so there is somewhere to put it.
    fn trim_common(&self, cs: usize, ce: usize, repl: &str) -> (usize, usize, String) {
        let r: Vec<char> = repl.chars().collect();
        // Each step is (chars consumed in text, chars consumed in repl, was a real glyph).
        let step_fwd = |pos: usize, rp: usize, end: usize, rend: usize| -> Option<(usize, usize, bool)> {
            if pos >= end {
                return None;
            }
            let g = self.char_glyph[pos];
            if g == NO_GLYPH {
                return (rp < rend && r[rp].is_whitespace()).then_some((1, 1, false));
            }
            let (gs, ge) = (self.glyph_chars[g as usize].0 as usize, self.glyph_chars[g as usize].1 as usize);
            let n = ge - gs;
            (gs == pos && ge <= end && rp + n <= rend && (0..n).all(|k| self.char_at(gs + k) == r[rp + k]))
                .then_some((n, n, true))
        };
        let step_back = |end: usize, rq: usize, start: usize, rstart: usize| -> Option<(usize, usize, bool)> {
            if end <= start {
                return None;
            }
            let g = self.char_glyph[end - 1];
            if g == NO_GLYPH {
                return (rq > rstart && r[rq - 1].is_whitespace()).then_some((1, 1, false));
            }
            let (gs, ge) = (self.glyph_chars[g as usize].0 as usize, self.glyph_chars[g as usize].1 as usize);
            let n = ge - gs;
            (ge == end && gs >= start && rq >= rstart + n && (0..n).all(|k| self.char_at(gs + k) == r[rq - n + k]))
                .then_some((n, n, true))
        };
        let (mut pos, mut rp) = (cs, 0);
        let mut fwd: Vec<(usize, usize, bool)> = Vec::new();
        while let Some(step) = step_fwd(pos, rp, ce, r.len()) {
            pos += step.0;
            rp += step.1;
            fwd.push(step);
        }
        let (mut end, mut rq) = (ce, r.len());
        let mut back: Vec<(usize, usize, bool)> = Vec::new();
        while let Some(step) = step_back(end, rq, pos, rp) {
            end -= step.0;
            rq -= step.1;
            back.push(step);
        }
        let has_glyph = |a: usize, b: usize| (a..b).any(|c| self.char_glyph[c] != NO_GLYPH);
        if rp < rq && !has_glyph(pos, end) {
            // Text must be inserted: give back one glyph to anchor it.
            let mut anchored = false;
            while let Some(step) = fwd.pop() {
                pos -= step.0;
                rp -= step.1;
                if step.2 {
                    anchored = true;
                    break;
                }
            }
            while !anchored {
                let Some(step) = back.pop() else { break };
                end += step.0;
                rq += step.1;
                anchored = step.2;
            }
        }
        (pos, end, r[rp..rq].iter().collect())
    }

    fn chars(&self, from: u32, to: u32) -> &str {
        let a = self.char_byte.get(from as usize).copied().unwrap_or(self.text.len());
        let b = self.char_byte.get(to as usize).copied().unwrap_or(self.text.len());
        &self.text[a..b.max(a)]
    }
}

enum Piece {
    Code(u32),
    /// A word space for a font that has no space glyph: emitted as a gap.
    Space,
    /// Text set in a standard fallback font.
    Alt { res: Vec<u8>, bytes: Vec<u8>, family: Family, text: String },
}

enum Elem {
    Str { bytes: Vec<u8>, hex: bool, width: f64 },
    Raw { start: usize, end: usize, width: f64 },
    Adj { text: Vec<u8>, width: f64 },
    Alt { res: Vec<u8>, bytes: Vec<u8>, width: f64 },
}

impl Elem {
    fn width(&self) -> f64 {
        match self {
            Elem::Str { width, .. } | Elem::Raw { width, .. } | Elem::Adj { width, .. } | Elem::Alt { width, .. } => {
                *width
            }
        }
    }
}

fn push_num(out: &mut Vec<u8>, text: &[u8]) {
    if out.last().is_some_and(|&b| is_regular(b)) {
        out.push(b' ');
    }
    out.extend_from_slice(text);
}

/// Serializes the replacement for one text operator.
fn emit(op: &TextOp, elems: &[Elem], src: &[u8], scale: Option<f64>) -> Vec<u8> {
    let mut out = Vec::new();
    // Whatever separated the last operand from the operator originally (often nothing).
    let kw_start = op.end - if op.kind == ShowKind::Quote || op.kind == ShowKind::DQuote { 1 } else { 2 };
    let gap_len = src[op.start..kw_start].iter().rev().take_while(|b| is_whitespace(**b)).count();
    let gap = &src[kw_start - gap_len..kw_start];
    let write_elem = |out: &mut Vec<u8>, e: &Elem| match e {
        Elem::Str { bytes, hex, .. } => write_string(out, bytes, if *hex { StrFmt::Hex } else { StrFmt::Literal }),
        Elem::Raw { start, end, .. } => out.extend_from_slice(&src[*start..*end]),
        Elem::Adj { text, .. } => push_num(out, text),
        Elem::Alt { .. } => unreachable!(),
    };
    let is_string = |e: &Elem| matches!(e, Elem::Str { .. } | Elem::Raw { .. });
    let plain = !elems.iter().any(|e| matches!(e, Elem::Alt { .. })) && scale.is_none();
    let single = elems.len() <= 1 && elems.iter().all(is_string);

    if plain && op.kind == ShowKind::TJ {
        out.push(b'[');
        elems.iter().for_each(|e| write_elem(&mut out, e));
        out.push(b']');
        out.extend_from_slice(gap);
        out.extend_from_slice(b"TJ");
        return out;
    }
    if plain && single {
        if let Some((aw, ac)) = &op.dquote_args {
            out.extend_from_slice(aw);
            out.push(b' ');
            out.extend_from_slice(ac);
            out.push(b' ');
        }
        match elems.first() {
            Some(e) => write_elem(&mut out, e),
            None => out.extend_from_slice(b"()"),
        }
        out.extend_from_slice(gap);
        out.extend_from_slice(match op.kind {
            ShowKind::Quote => b"'",
            ShowKind::DQuote => b"\"",
            _ => b"Tj",
        });
        return out;
    }

    // General form: explicit line move, then one show operator per font run.
    match (&op.kind, &op.dquote_args) {
        (ShowKind::DQuote, Some((aw, ac))) => {
            out.extend_from_slice(aw);
            out.extend_from_slice(b" Tw ");
            out.extend_from_slice(ac);
            out.extend_from_slice(b" Tc T* ");
        }
        (ShowKind::Quote | ShowKind::DQuote, _) => out.extend_from_slice(b"T* "),
        _ => {}
    }
    if let Some(s) = scale {
        out.extend_from_slice(format!("{} Tz ", fmt_num(op.th * 100.0 * s)).as_bytes());
    }
    let mut wrote_any = false;
    let flush = |out: &mut Vec<u8>, run: &[&Elem], wrote_any: &mut bool| {
        if run.is_empty() {
            return;
        }
        if *wrote_any {
            out.push(b' ');
        }
        if run.len() == 1 && is_string(run[0]) {
            write_elem(out, run[0]);
            out.extend_from_slice(b"Tj");
        } else {
            out.push(b'[');
            run.iter().for_each(|e| write_elem(out, e));
            out.extend_from_slice(b"]TJ");
        }
        *wrote_any = true;
    };
    let mut run: Vec<&Elem> = Vec::new();
    for e in elems {
        match e {
            Elem::Alt { res, bytes, .. } => {
                flush(&mut out, &run, &mut wrote_any);
                run.clear();
                if wrote_any {
                    out.push(b' ');
                }
                write_name(&mut out, res);
                out.push(b' ');
                out.extend_from_slice(&op.size_raw);
                out.extend_from_slice(b" Tf ");
                write_string(&mut out, bytes, StrFmt::Literal);
                out.extend_from_slice(b"Tj ");
                write_name(&mut out, &op.font_res);
                out.push(b' ');
                out.extend_from_slice(&op.size_raw);
                out.extend_from_slice(b" Tf");
                wrote_any = true;
            }
            e => run.push(e),
        }
    }
    flush(&mut out, &run, &mut wrote_any);
    if !wrote_any {
        out.extend_from_slice(b"()Tj");
    }
    if scale.is_some() {
        out.extend_from_slice(format!(" {} Tz", fmt_num(op.th * 100.0)).as_bytes());
    }
    out
}

struct Planned {
    first: usize,
    last: usize,
    pieces: Vec<Piece>,
    used_fallback: bool,
}

/// Decides how to write `full` (replacement plus any leftover characters of
/// partially matched glyphs) at the position of a match.
#[allow(clippy::too_many_arguments)]
fn plan_pieces(
    pdf: &mut Pdf,
    fonts: &mut Fonts,
    unit: &mut Unit,
    op: &TextOp,
    prefix: &str,
    repl: &str,
    suffix: &str,
    opts: &Options,
) -> Result<(Vec<Piece>, bool), String> {
    let fi = op.font.ok_or("text uses an unresolvable font")?;
    let full = format!("{prefix}{repl}{suffix}");
    let to_pieces =
        |codes: Vec<Option<u32>>| codes.into_iter().map(|c| c.map_or(Piece::Space, Piece::Code)).collect::<Vec<_>>();
    if let Some(codes) = fonts.list[fi].encode(&full) {
        return Ok((to_pieces(codes), false));
    }
    let missing = fonts.list[fi].first_unencodable(&full).unwrap_or('?');
    let font_name = fonts.list[fi].base_name.clone();
    let why = format!(
        "font {} has no glyph for {missing:?}{}",
        if font_name.is_empty() { "(unnamed)" } else { &font_name },
        if fonts.list[fi].subset { " (embedded subset)" } else { "" }
    );
    if !opts.fallback {
        return Err(format!("{why}; fallback font disabled"));
    }
    if op.font_res.is_empty() || op.vertical {
        return Err(format!("{why}; a fallback font cannot be used for this text"));
    }
    // Keep leftover characters in the original font when possible.
    let (pre, suf, alt_text) = match (fonts.list[fi].encode(prefix), fonts.list[fi].encode(suffix)) {
        (Some(p), Some(s)) => (to_pieces(p), to_pieces(s), repl.to_string()),
        _ => (Vec::new(), Vec::new(), full.clone()),
    };
    let Some(bytes) = encode_win_ansi(&alt_text) else {
        let bad = alt_text.chars().find(|c| encode_win_ansi(&c.to_string()).is_none()).unwrap_or('?');
        return Err(format!("{why}; and the standard fallback fonts cannot show {bad:?} either"));
    };
    let mut style = fonts.list[fi].style;
    if let Some(family) = opts.fallback_family {
        style.family = family;
    }
    let res = fonts.ensure_fallback(pdf, unit, style).ok_or("could not register a fallback font")?;
    let mut pieces = pre;
    if !bytes.is_empty() {
        pieces.push(Piece::Alt { res, bytes, family: style.family, text: alt_text });
    }
    pieces.extend(suf);
    Ok((pieces, true))
}

fn decode_streams(pdf: &Pdf, unit: &Unit) -> Result<Vec<Vec<u8>>> {
    unit.streams.iter().map(|&n| pdf.stream_data(n)).collect()
}

/// Parses a unit once so every code it shows is recorded as "in use" for its font.
pub fn scan_usage(pdf: &Pdf, fonts: &mut Fonts, unit: &Unit) -> Result<()> {
    let data = decode_streams(pdf, unit)?;
    let uf = fonts.for_unit(pdf, unit);
    let mut table = FontTable { fonts: &mut fonts.list, by_name: &uf.by_name, by_gs: &uf.by_gs, note_used: true };
    content::parse(&data, &mut table);
    Ok(())
}

/// The text of a unit as the matcher sees it.
pub fn extract_text(pdf: &Pdf, fonts: &mut Fonts, unit: &Unit) -> Result<String> {
    let data = decode_streams(pdf, unit)?;
    let uf = fonts.for_unit(pdf, unit);
    let mut table = FontTable { fonts: &mut fonts.list, by_name: &uf.by_name, by_gs: &uf.by_gs, note_used: false };
    let ops = content::parse(&data, &mut table).ops;
    Ok(build_text(&ops, &fonts.list).text)
}

/// Applies every text rule to one content unit. `done` holds streams already
/// rewritten through another unit; they are left alone here.
pub fn process_unit(
    pdf: &mut Pdf,
    fonts: &mut Fonts,
    unit: &mut Unit,
    rules: &[Rule],
    stats: &mut [RuleStats],
    opts: &Options,
    done: &mut HashSet<u32>,
) -> Result<()> {
    let mut data = decode_streams(pdf, unit)?;
    let mut local_seen = HashSet::new();
    let frozen: Vec<bool> = unit.streams.iter().map(|n| done.contains(n) || !local_seen.insert(*n)).collect();
    let mut changed = vec![false; data.len()];

    for (ri, rule) in rules.iter().enumerate() {
        if !rule.text || !rule.applies_to_page(unit.page) {
            continue;
        }
        let uf = fonts.for_unit(pdf, unit);
        let content::Parsed { ops, actual } = {
            let mut table =
                FontTable { fonts: &mut fonts.list, by_name: &uf.by_name, by_gs: &uf.by_gs, note_used: false };
            content::parse(&data, &mut table)
        };
        let logical = build_text(&ops, &fonts.list);

        // (stream, start, end, replacement bytes)
        let mut edits: Vec<(usize, usize, usize, Vec<u8>)> = Vec::new();
        // Marked-content /ActualText overrides what the glyphs say; keep it in step.
        for at in actual.iter().filter(|a| !frozen[a.stream]) {
            let (text, utf16) = decode_text_string(&at.bytes);
            if !rule.text_re.is_match(&text) {
                continue;
            }
            let new = rule.text_re.replace_all(&text, |c: &regex::Captures| rule.replacement(c));
            let mut out = Vec::new();
            write_string(&mut out, &encode_text_string(&new, utf16), StrFmt::Literal);
            edits.push((at.stream, at.start, at.end, out));
            stats[ri].actual_text += 1;
        }

        let mut planned: Vec<Planned> = Vec::new();
        // Matches that needed no glyph changes, and the last glyph claimed by any match.
        let mut matched = 0;
        let mut reserved: Option<usize> = None;
        for caps in rule.text_re.captures_iter(&logical.text) {
            let m = caps.get(0).unwrap();
            if m.is_empty() {
                continue;
            }
            let (cs, ce) = (logical.char_at_byte(m.start()), logical.char_at_byte(m.end()));
            let mut real = (cs..ce).filter(|&c| logical.char_glyph[c] != NO_GLYPH);
            let Some(first_char) = real.next() else { continue };
            let last_char = real.next_back().unwrap_or(first_char);
            let (first, last) = (logical.char_glyph[first_char] as usize, logical.char_glyph[last_char] as usize);
            if reserved.is_some_and(|r| r >= first) {
                continue;
            }
            let op_of = |g: usize| &ops[logical.glyphs[g].0 as usize];
            if (first..=last).any(|g| frozen[op_of(g).stream]) {
                continue;
            }
            let span_last = last;
            let repl = rule.replacement(&caps);
            // Leave glyphs alone where the replacement repeats the original text:
            // they keep their exact codes, kerning and position.
            let (cs, ce, repl) = if opts.minimal { logical.trim_common(cs, ce, &repl) } else { (cs, ce, repl) };
            let mut real = (cs..ce).filter(|&c| logical.char_glyph[c] != NO_GLYPH);
            let Some(first_char) = real.next() else {
                matched += 1; // nothing left to change
                reserved = Some(span_last);
                continue;
            };
            let last_char = real.next_back().unwrap_or(first_char);
            let (first, last) = (logical.char_glyph[first_char] as usize, logical.char_glyph[last_char] as usize);
            // Characters of partially matched glyphs (ligatures) must survive.
            let prefix = logical.chars(logical.glyph_chars[first].0, cs as u32).to_string();
            let suffix = logical.chars(ce as u32, logical.glyph_chars[last].1).to_string();
            match plan_pieces(pdf, fonts, unit, op_of(first), &prefix, &repl, &suffix, opts) {
                Ok((pieces, used_fallback)) => {
                    reserved = Some(span_last);
                    planned.push(Planned { first, last, pieces, used_fallback })
                }
                Err(why) => {
                    let shown: String = m.as_str().chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
                    stats[ri].skipped.push(format!("{}: {shown:?} not replaced: {why}", unit.label));
                }
            }
        }
        stats[ri].text += planned.len() + matched;
        if planned.is_empty() && edits.is_empty() {
            continue;
        }
        stats[ri].fallback += planned.iter().filter(|p| p.used_fallback).count();

        // Which match (1-based) removes each glyph; 0 = the glyph stays.
        let mut owner = vec![0u32; logical.glyphs.len()];
        let mut inserts: HashMap<usize, Vec<Piece>> = HashMap::new();
        let mut touched: Vec<usize> = Vec::new();
        for (id, p) in planned.into_iter().enumerate() {
            for g in p.first..=p.last {
                owner[g] = id as u32 + 1;
                touched.push(logical.glyphs[g].0 as usize);
            }
            inserts.insert(p.first, p.pieces);
        }
        touched.sort_unstable();
        touched.dedup();

        for &oi in &touched {
            let op = &ops[oi];
            let font = &fonts.list[op.font.expect("matched text has a font")];
            let base = logical.op_start[oi];
            let count: usize = op.items.iter().map(|i| if let Item::Str { glyphs, .. } = i { glyphs.len() } else { 0 }).sum();
            let unit_adv = |code: u32, len: usize| {
                if op.vertical {
                    -op.size + op.tc
                } else {
                    font.width(code) / 1000.0 * op.size + op.tc + if len == 1 && code == 32 { op.tw } else { 0.0 }
                }
            };
            let mut elems: Vec<Elem> = Vec::new();
            let mut old_width = 0.0;
            let mut g = base;
            for item in &op.items {
                match item {
                    Item::Adj { value, start, end } => {
                        let width = -value / 1000.0 * op.size;
                        old_width += width;
                        // Kerning between two removed glyphs goes with them.
                        let inside = g > base && g < base + count && owner[g] != 0 && owner[g - 1] == owner[g];
                        if !inside {
                            elems.push(Elem::Adj { text: data[op.stream][*start..*end].to_vec(), width });
                        }
                    }
                    Item::Str { glyphs, hex, start, end } => {
                        let item_width: f64 = glyphs.iter().map(|x| x.adv).sum();
                        old_width += item_width;
                        let untouched = (g..g + glyphs.len()).all(|x| owner[x] == 0);
                        if untouched {
                            elems.push(Elem::Raw { start: *start, end: *end, width: item_width });
                            g += glyphs.len();
                            continue;
                        }
                        let mut buf: Vec<u8> = Vec::new();
                        let mut width = 0.0;
                        let flush = |buf: &mut Vec<u8>, width: &mut f64, elems: &mut Vec<Elem>| {
                            if !buf.is_empty() {
                                elems.push(Elem::Str { bytes: std::mem::take(buf), hex: *hex, width: *width });
                            }
                            *width = 0.0;
                        };
                        for glyph in glyphs {
                            if let Some(pieces) = inserts.get(&g) {
                                for piece in pieces {
                                    match piece {
                                        Piece::Code(code) => {
                                            let bytes = font.code_bytes(*code);
                                            width += unit_adv(*code, bytes.len());
                                            buf.extend_from_slice(&bytes);
                                        }
                                        Piece::Space => {
                                            flush(&mut buf, &mut width, &mut elems);
                                            let w = font.space_width();
                                            let (value, adv) = if op.vertical {
                                                (w, -w / 1000.0 * op.size)
                                            } else {
                                                (-w, w / 1000.0 * op.size)
                                            };
                                            elems.push(Elem::Adj { text: fmt_num(value).into_bytes(), width: adv });
                                        }
                                        Piece::Alt { res, bytes, family, text } => {
                                            flush(&mut buf, &mut width, &mut elems);
                                            let w: f64 = text
                                                .chars()
                                                .map(|c| {
                                                    standard_width(*family, c) / 1000.0 * op.size
                                                        + op.tc
                                                        + if c == ' ' { op.tw } else { 0.0 }
                                                })
                                                .sum();
                                            elems.push(Elem::Alt { res: res.clone(), bytes: bytes.clone(), width: w });
                                        }
                                    }
                                }
                            }
                            if owner[g] == 0 {
                                let bytes = glyph.code.to_be_bytes();
                                buf.extend_from_slice(&bytes[4 - glyph.len as usize..]);
                                width += glyph.adv;
                            }
                            g += 1;
                        }
                        flush(&mut buf, &mut width, &mut elems);
                    }
                }
            }

            // Fitting: condense the operator if its new text would collide with what follows.
            let mut scale = None;
            let new_width: f64 = elems.iter().map(Elem::width).sum();
            if opts.fit != Fit::None && !op.vertical && op.size > 0.0 && new_width > old_width + 1e-6 && count > 0 {
                let last_glyph = op.items.iter().rev().find_map(|i| match i {
                    Item::Str { glyphs, .. } => glyphs.last(),
                    _ => None,
                });
                let next = (base + count..logical.glyphs.len()).find(|&x| owner[x] == 0).map(|x| {
                    let (o, i, k) = logical.glyphs[x];
                    let nop = &ops[o as usize];
                    let Item::Str { glyphs, .. } = &nop.items[i as usize] else { unreachable!() };
                    (nop.dir, glyphs[k as usize].p0)
                });
                let room = match (last_glyph, next) {
                    (Some(lg), Some((ndir, np0))) => match offset_em(op, ndir, lg.p1, np0) {
                        // `along` is in ems of the larger font; convert to this op's text units.
                        Some((along, across)) if across.abs() <= 0.5 && along >= -0.5 => {
                            let em = dot(op.dir, op.dir).sqrt().max(dot(ndir, ndir).sqrt());
                            let gap = along * em / dot(op.dir, op.dir).sqrt() * op.size;
                            Some((gap - 0.2 * op.size).max(0.0))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                let allowed = match (opts.fit, room) {
                    (_, Some(r)) => Some(r),
                    (Fit::Strict, None) => Some(0.0),
                    _ => None,
                };
                if let Some(a) = allowed {
                    if new_width > old_width + a + 1e-6 && old_width > 0.0 {
                        scale = Some(((old_width + a) / new_width).max(0.5));
                        stats[ri].condensed += 1;
                    }
                }
            }
            edits.push((op.stream, op.start, op.end, emit(op, &elems, &data[op.stream], scale)));
        }

        edits.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
        for (stream, start, end, mut bytes) in edits {
            // The original operand may have abutted its neighbours (`Td(x)Tj`);
            // keep tokens apart if the new text starts or ends differently.
            let buf = &data[stream];
            if start > 0 && is_regular(buf[start - 1]) && bytes.first().is_some_and(|&b| is_regular(b)) {
                bytes.insert(0, b' ');
            }
            if buf.get(end).is_some_and(|&b| is_regular(b)) && bytes.last().is_some_and(|&b| is_regular(b)) {
                bytes.push(b' ');
            }
            data[stream].splice(start..end, bytes);
            changed[stream] = true;
        }
    }

    for (i, &num) in unit.streams.iter().enumerate() {
        if changed[i] {
            pdf.set_stream_data(num, std::mem::take(&mut data[i]))?;
        }
        done.insert(num);
    }
    // Glyphs written by replacements now count as used.
    scan_usage(pdf, fonts, unit).ok();
    Ok(())
}

/// Decodes a PDF text string (UTF-16BE with BOM, UTF-8 with BOM, else PDFDocEncoding
/// treated as Latin-1). Returns the text and whether it was UTF-16.
fn decode_text_string(bytes: &[u8]) -> (String, bool) {
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| (c[0] as u16) << 8 | c[1] as u16).collect();
        (String::from_utf16_lossy(&units), true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (String::from_utf8_lossy(rest).into_owned(), true)
    } else {
        (bytes.iter().map(|&b| b as char).collect(), false)
    }
}

fn encode_text_string(text: &str, utf16: bool) -> Vec<u8> {
    if !utf16 && text.chars().all(|c| (c as u32) < 256) {
        return text.chars().map(|c| c as u32 as u8).collect();
    }
    let mut out = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

// ------------------------------------------------------------------ links

fn uri_to_string(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), true),
        Err(_) => (bytes.iter().map(|&b| b as char).collect(), false),
    }
}

/// Applies the link rules to one URI string. Returns true if any rule matches;
/// the string is only modified (and hits counted) when `apply` is set.
fn rewrite_uri_string(bytes: &mut Vec<u8>, rules: &[Rule], stats: &mut [RuleStats], apply: bool) -> bool {
    let (mut uri, was_utf8) = uri_to_string(bytes);
    let mut changed = false;
    for (ri, rule) in rules.iter().enumerate().filter(|(_, r)| r.links) {
        let n = rule.link_re.find_iter(&uri).count();
        if n == 0 {
            continue;
        }
        changed = true;
        if apply {
            stats[ri].links += n;
            uri = rule.link_re.replace_all(&uri, |c: &regex::Captures| rule.replacement(c)).into_owned();
        }
    }
    if changed && apply {
        *bytes = if !was_utf8 && uri.chars().all(|c| (c as u32) < 256) {
            uri.chars().map(|c| c as u32 as u8).collect()
        } else {
            uri.into_bytes()
        };
    }
    changed
}

/// Rewrites URI strings stored directly in URI actions under `obj`. URIs held
/// as separate string objects are reported through `indirect` instead.
fn rewrite_uris(
    obj: &mut Object,
    rules: &[Rule],
    stats: &mut [RuleStats],
    apply: bool,
    indirect: &mut Vec<u32>,
) -> bool {
    let mut hit = false;
    match obj {
        Object::Dict(d) => {
            let is_uri_action = d.name(b"S") == Some(b"URI");
            for (key, value) in d.0.iter_mut() {
                if is_uri_action && key == b"URI" {
                    match value {
                        Object::Str(bytes, _) => {
                            hit |= rewrite_uri_string(bytes, rules, stats, apply);
                            continue;
                        }
                        Object::Ref(n, _) => {
                            indirect.push(*n);
                            continue;
                        }
                        _ => {}
                    }
                }
                hit |= rewrite_uris(value, rules, stats, apply, indirect);
            }
        }
        Object::Array(a) => {
            for o in a {
                hit |= rewrite_uris(o, rules, stats, apply, indirect);
            }
        }
        _ => {}
    }
    hit
}

/// Collects the URIs of URI actions under `obj`, following only the
/// references an action chain can hang from (never `/P` or `/Parent`, which
/// would lead back into the whole document).
fn collect_uris(pdf: &Pdf, obj: &Object, out: &mut Vec<String>, visited: &mut HashSet<u32>, depth: usize) {
    if depth > 16 {
        return;
    }
    match obj {
        Object::Dict(d) => {
            let is_uri_action = d.name(b"S") == Some(b"URI");
            for (key, value) in &d.0 {
                if is_uri_action && key == b"URI" {
                    if let Object::Ref(n, _) = value {
                        visited.insert(*n);
                    }
                    if let Object::Str(bytes, _) = pdf.resolve(value) {
                        out.push(uri_to_string(bytes).0);
                    }
                    continue;
                }
                match value {
                    Object::Ref(n, _) => {
                        let follow = matches!(key.as_slice(), b"A" | b"AA" | b"PA" | b"Next");
                        if follow && visited.insert(*n) {
                            if let Some(PdfObj::Plain(o)) = pdf.get(*n) {
                                collect_uris(pdf, o, out, visited, depth + 1);
                            }
                        }
                    }
                    other => collect_uris(pdf, other, out, visited, depth + 1),
                }
            }
        }
        Object::Array(a) => {
            for o in a {
                match o {
                    Object::Ref(n, _) => {
                        if depth > 0 && visited.insert(*n) {
                            if let Some(PdfObj::Plain(inner)) = pdf.get(*n) {
                                collect_uris(pdf, inner, out, visited, depth + 1);
                            }
                        }
                    }
                    other => collect_uris(pdf, other, out, visited, depth + 1),
                }
            }
        }
        _ => {}
    }
}

/// Every hyperlink URI in the document, as the link rules see them: one list
/// per page (from its annotations, in order) and a final list of URIs found
/// anywhere else (bookmarks, document-level actions).
pub fn extract_links(pdf: &Pdf) -> (Vec<Vec<String>>, Vec<String>) {
    let mut visited: HashSet<u32> = HashSet::new();
    let mut pages = Vec::new();
    for page in crate::doc::page_numbers(pdf) {
        let mut uris = Vec::new();
        let annots = pdf.dict_of(page).and_then(|d| d.get(b"Annots")).map(|a| pdf.resolve(a));
        if let Some(Object::Array(annots)) = annots {
            for annot in annots {
                match annot {
                    Object::Ref(n, _) => {
                        if visited.insert(*n) {
                            if let Some(PdfObj::Plain(o)) = pdf.get(*n) {
                                collect_uris(pdf, o, &mut uris, &mut visited, 0);
                            }
                        }
                    }
                    direct => collect_uris(pdf, direct, &mut uris, &mut visited, 0),
                }
            }
        }
        pages.push(uris);
    }
    let page_set: HashSet<u32> = crate::doc::page_numbers(pdf).into_iter().collect();
    let mut other = Vec::new();
    for (&num, entry) in &pdf.objects {
        // Pages were handled above (their direct annotations included).
        if visited.contains(&num) || page_set.contains(&num) {
            continue;
        }
        if let PdfObj::Plain(o) = &entry.obj {
            // Depth 0 keeps this to the object itself plus its own action chain.
            let mut found = Vec::new();
            let mut local = visited.clone();
            collect_uris(pdf, o, &mut found, &mut local, 0);
            if !found.is_empty() {
                visited = local;
                visited.insert(num);
                other.extend(found);
            }
        }
    }
    (pages, other)
}

/// Applies link rules to every URI action in the document, wherever it lives
/// (link annotations, outline items, chained actions).
pub fn process_links(pdf: &mut Pdf, rules: &[Rule], stats: &mut [RuleStats]) {
    if !rules.iter().any(|r| r.links) {
        return;
    }
    let nums: Vec<u32> = pdf.objects.keys().copied().collect();
    let mut indirect: Vec<u32> = Vec::new();
    for num in nums {
        // Probe on a copy first so untouched objects are never marked modified.
        let mut probe = match pdf.get(num) {
            Some(PdfObj::Plain(o)) => o.clone(),
            Some(PdfObj::Stream(s)) => Object::Dict(s.dict.clone()),
            None => continue,
        };
        if !rewrite_uris(&mut probe, rules, stats, false, &mut indirect) {
            continue;
        }
        rewrite_uris(&mut probe, rules, stats, true, &mut Vec::new());
        match probe {
            Object::Dict(d) if matches!(pdf.get(num), Some(PdfObj::Stream(_))) => {
                if let Some(dict) = pdf.dict_mut(num) {
                    *dict = d;
                }
            }
            other => {
                if let Some(o) = pdf.obj_mut(num) {
                    *o = other;
                }
            }
        }
    }
    // Some producers (macOS Quartz) keep each URI in its own string object.
    indirect.sort_unstable();
    indirect.dedup();
    for num in indirect {
        let Some(PdfObj::Plain(Object::Str(bytes, _))) = pdf.get(num) else { continue };
        let mut bytes = bytes.clone();
        if !rewrite_uri_string(&mut bytes, rules, stats, false) {
            continue;
        }
        rewrite_uri_string(&mut bytes, rules, stats, true);
        if let Some(Object::Str(target, _)) = pdf.obj_mut(num) {
            *target = bytes;
        }
    }
}
