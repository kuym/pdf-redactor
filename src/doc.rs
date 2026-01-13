//! Document traversal: finds every content stream that can draw text (pages,
//! form XObjects, tiling patterns, annotation appearances) together with the
//! resource dictionary its font names resolve against.

use std::collections::{HashMap, HashSet};

use crate::font::{Font, Style};
use crate::object::{Dict, Object};
use crate::pdf::{Pdf, PdfObj};

/// Where a resource dictionary lives, so entries can be added to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResLoc {
    /// The dictionary is indirect object `n`.
    Object(u32),
    /// The dictionary is the direct `/Resources` value inside object `n`.
    Key(u32),
    /// Object `n` has no resources yet.
    Missing(u32),
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub streams: Vec<u32>,
    pub res: ResLoc,
    /// Additional dictionaries searched after `res` (e.g. the AcroForm defaults).
    pub extra: Vec<ResLoc>,
    pub label: String,
    /// 1-based page number this unit was first reached from.
    pub page: usize,
}

fn res_dict(pdf: &Pdf, loc: ResLoc) -> Option<&Dict> {
    match loc {
        ResLoc::Object(n) => pdf.dict_of(n),
        ResLoc::Key(n) => pdf.dict_of(n)?.get(b"Resources")?.as_dict(),
        ResLoc::Missing(_) => None,
    }
}

fn res_loc_of(pdf: &Pdf, owner: u32) -> Option<ResLoc> {
    match pdf.dict_of(owner)?.get(b"Resources")? {
        Object::Ref(n, _) if pdf.dict_of(*n).is_some() => Some(ResLoc::Object(*n)),
        Object::Dict(_) => Some(ResLoc::Key(owner)),
        _ => None,
    }
}

fn is_stream(pdf: &Pdf, n: u32) -> bool {
    matches!(pdf.get(n), Some(PdfObj::Stream(_)))
}

pub fn page_numbers(pdf: &Pdf) -> Vec<u32> {
    let mut pages = Vec::new();
    let mut seen = HashSet::new();
    let root = pdf.trailer.get(b"Root").and_then(Object::as_ref);
    let Some(first) = root.and_then(|r| pdf.dict_of(r)).and_then(|c| c.get(b"Pages")).and_then(Object::as_ref) else {
        return pages;
    };
    let mut stack = vec![first];
    while let Some(n) = stack.pop() {
        if !seen.insert(n) {
            continue;
        }
        let Some(d) = pdf.dict_of(n) else { continue };
        match pdf.resolve(d.get(b"Kids").unwrap_or(&Object::Null)) {
            Object::Array(kids) => stack.extend(kids.iter().rev().filter_map(Object::as_ref)),
            _ => pages.push(n),
        }
    }
    pages
}

pub fn discover(pdf: &Pdf) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let acro_dr = pdf
        .trailer
        .get(b"Root")
        .and_then(Object::as_ref)
        .and_then(|r| pdf.dict_of(r))
        .and_then(|c| match c.get(b"AcroForm")? {
            Object::Ref(n, _) => Some(*n),
            _ => None,
        })
        .and_then(|n| match pdf.dict_of(n)?.get(b"DR")? {
            Object::Ref(r, _) => Some(ResLoc::Object(*r)),
            Object::Dict(_) => None,
            _ => None,
        });

    for (index, &page) in page_numbers(pdf).iter().enumerate() {
        let number = index + 1;
        let Some(dict) = pdf.dict_of(page) else { continue };
        // Resources may be inherited from an ancestor /Pages node.
        let mut res = ResLoc::Missing(page);
        let mut node = Some(page);
        let mut hops = 0;
        while let Some(n) = node {
            if let Some(loc) = res_loc_of(pdf, n) {
                res = loc;
                break;
            }
            hops += 1;
            node = pdf.dict_of(n).and_then(|d| d.get(b"Parent")).and_then(Object::as_ref).filter(|_| hops < 64);
        }
        let streams: Vec<u32> = match dict.get(b"Contents") {
            Some(Object::Ref(n, _)) if is_stream(pdf, *n) => vec![*n],
            Some(other) => match pdf.resolve(other) {
                Object::Array(a) => a.iter().filter_map(Object::as_ref).filter(|n| is_stream(pdf, *n)).collect(),
                _ => vec![],
            },
            None => vec![],
        };
        if !streams.is_empty() {
            units.push(Unit { streams, res, extra: vec![], label: format!("page {number}"), page: number });
        }
        let mut pending: Vec<(ResLoc, usize)> = vec![(res, 0)];
        // Annotation appearance streams.
        if let Object::Array(annots) = pdf.resolve(dict.get(b"Annots").unwrap_or(&Object::Null)) {
            for annot in annots {
                let Some(ad) = pdf.resolve(annot).as_dict().or_else(|| annot.as_ref().and_then(|n| pdf.dict_of(n)))
                else {
                    continue;
                };
                let Some(ap) = pdf.dict_at(ad, b"AP") else { continue };
                for (_, state) in &ap.0 {
                    let mut forms = Vec::new();
                    match state {
                        Object::Ref(n, _) if is_stream(pdf, *n) => forms.push(*n),
                        other => {
                            if let Some(d) = pdf.resolve(other).as_dict() {
                                forms.extend(d.0.iter().filter_map(|(_, v)| v.as_ref()).filter(|n| is_stream(pdf, *n)));
                            }
                        }
                    }
                    for f in forms {
                        if seen.insert(f) {
                            let loc = res_loc_of(pdf, f).unwrap_or(ResLoc::Missing(f));
                            units.push(Unit {
                                streams: vec![f],
                                res: loc,
                                extra: acro_dr.into_iter().collect(),
                                label: format!("page {number} annotation appearance (object {f})"),
                                page: number,
                            });
                            pending.push((loc, 1));
                        }
                    }
                }
            }
        }
        // Form XObjects and tiling patterns reachable through resources.
        while let Some((loc, depth)) = pending.pop() {
            if depth > 32 {
                continue;
            }
            let Some(rd) = res_dict(pdf, loc) else { continue };
            for (key, what) in [(&b"XObject"[..], "form"), (&b"Pattern"[..], "pattern")] {
                let Some(d) = pdf.dict_at(rd, key) else { continue };
                for (_, v) in &d.0 {
                    let Some(n) = v.as_ref() else { continue };
                    let Some(PdfObj::Stream(s)) = pdf.get(n) else { continue };
                    let wanted = if what == "form" {
                        s.dict.name(b"Subtype") == Some(b"Form")
                    } else {
                        s.dict.get(b"PatternType").and_then(Object::as_int) == Some(1)
                    };
                    if !wanted || !seen.insert(n) {
                        continue;
                    }
                    // A form without its own resources uses those of whatever invokes it.
                    let own = res_loc_of(pdf, n);
                    units.push(Unit {
                        streams: vec![n],
                        res: own.unwrap_or(loc),
                        extra: vec![],
                        label: format!("page {number} {what} (object {n})"),
                        page: number,
                    });
                    if let Some(own) = own {
                        pending.push((own, depth + 1));
                    }
                }
            }
        }
    }
    units
}

#[derive(Default)]
pub struct Fonts {
    pub list: Vec<Font>,
    by_obj: HashMap<u32, usize>,
    inline: HashMap<(ResLoc, Vec<u8>), usize>,
    fallback: HashMap<Style, u32>,
}

pub struct UnitFonts {
    pub by_name: HashMap<Vec<u8>, usize>,
    pub by_gs: HashMap<Vec<u8>, (usize, f64)>,
}

impl Fonts {
    fn index_for(&mut self, pdf: &Pdf, loc: ResLoc, name: &[u8], value: &Object) -> Option<usize> {
        match value {
            Object::Ref(n, _) => {
                if let Some(&i) = self.by_obj.get(n) {
                    return Some(i);
                }
                let font = Font::load(pdf, pdf.dict_of(*n)?);
                self.list.push(font);
                self.by_obj.insert(*n, self.list.len() - 1);
                Some(self.list.len() - 1)
            }
            Object::Dict(d) => {
                let key = (loc, name.to_vec());
                if let Some(&i) = self.inline.get(&key) {
                    return Some(i);
                }
                self.list.push(Font::load(pdf, d));
                self.inline.insert(key, self.list.len() - 1);
                Some(self.list.len() - 1)
            }
            _ => None,
        }
    }

    /// Resolves the font names visible to `unit`.
    pub fn for_unit(&mut self, pdf: &Pdf, unit: &Unit) -> UnitFonts {
        let mut out = UnitFonts { by_name: HashMap::new(), by_gs: HashMap::new() };
        for loc in std::iter::once(unit.res).chain(unit.extra.iter().copied()) {
            let Some(rd) = res_dict(pdf, loc) else { continue };
            if let Some(fd) = pdf.dict_at(rd, b"Font") {
                for (name, value) in &fd.0 {
                    if out.by_name.contains_key(name) {
                        continue;
                    }
                    if let Some(i) = self.index_for(pdf, loc, name, value) {
                        out.by_name.insert(name.clone(), i);
                    }
                }
            }
            if let Some(gd) = pdf.dict_at(rd, b"ExtGState") {
                for (name, value) in &gd.0 {
                    let Some(g) = pdf.resolve(value).as_dict().or_else(|| value.as_ref().and_then(|n| pdf.dict_of(n)))
                    else {
                        continue;
                    };
                    let Object::Array(spec) = pdf.resolve(g.get(b"Font").unwrap_or(&Object::Null)) else { continue };
                    let (Some(font), Some(size)) = (spec.first(), spec.get(1).and_then(|s| pdf.resolve(s).as_f64()))
                    else {
                        continue;
                    };
                    if let Some(i) = self.index_for(pdf, loc, name, font) {
                        out.by_gs.entry(name.clone()).or_insert((i, size));
                    }
                }
            }
        }
        out
    }

    /// Makes a standard (non-embedded) font of the given style available to
    /// `unit` and returns its resource name. Costs one tiny object per style.
    pub fn ensure_fallback(&mut self, pdf: &mut Pdf, unit: &mut Unit, style: Style) -> Option<Vec<u8>> {
        let font_num = match self.fallback.get(&style) {
            Some(&n) => n,
            None => {
                let mut d = Dict::default();
                d.set(b"Type", Object::Name(b"Font".to_vec()));
                d.set(b"Subtype", Object::Name(b"Type1".to_vec()));
                d.set(b"BaseFont", Object::Name(style.standard_font().as_bytes().to_vec()));
                d.set(b"Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
                let n = pdf.add(Object::Dict(d));
                self.fallback.insert(style, n);
                n
            }
        };
        // Where is the /Font dictionary of this unit's resources?
        enum Target {
            Indirect(u32),
            InRes,
        }
        let existing = res_dict(pdf, unit.res);
        let (target, taken): (Target, Vec<Vec<u8>>) = match existing.and_then(|r| r.get(b"Font")) {
            Some(Object::Ref(n, _)) if pdf.dict_of(*n).is_some() => {
                (Target::Indirect(*n), pdf.dict_of(*n).unwrap().0.iter().map(|(k, _)| k.clone()).collect())
            }
            Some(Object::Dict(d)) => (Target::InRes, d.0.iter().map(|(k, _)| k.clone()).collect()),
            _ => (Target::InRes, vec![]),
        };
        if let Some(fd) = existing.and_then(|r| pdf.dict_at(r, b"Font")) {
            if let Some((name, _)) = fd.0.iter().find(|(_, v)| v.as_ref() == Some(font_num)) {
                return Some(name.clone());
            }
        }
        let mut name = format!("RdxF{}", self.fallback.len()).into_bytes();
        let mut salt = 0;
        while taken.contains(&name) {
            salt += 1;
            name = format!("RdxF{}x{salt}", self.fallback.len()).into_bytes();
        }
        let entry = Object::Ref(font_num, 0);
        match target {
            Target::Indirect(n) => pdf.dict_mut(n)?.set(&name, entry),
            Target::InRes => {
                let res: &mut Dict = match unit.res {
                    ResLoc::Object(n) => pdf.dict_mut(n)?,
                    ResLoc::Key(n) => match pdf.dict_mut(n)?.get_mut(b"Resources")? {
                        Object::Dict(d) => d,
                        _ => return None,
                    },
                    ResLoc::Missing(n) => {
                        let owner = pdf.dict_mut(n)?;
                        owner.set(b"Resources", Object::Dict(Dict::default()));
                        unit.res = ResLoc::Key(n);
                        match owner.get_mut(b"Resources")? {
                            Object::Dict(d) => d,
                            _ => return None,
                        }
                    }
                };
                if !matches!(res.get(b"Font"), Some(Object::Dict(_))) {
                    res.set(b"Font", Object::Dict(Dict::default()));
                }
                match res.get_mut(b"Font")? {
                    Object::Dict(d) => d.set(&name, entry),
                    _ => return None,
                }
            }
        }
        Some(name)
    }
}
