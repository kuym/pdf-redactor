//! Stream filter decoding and (Flate) encoding.

use anyhow::{Result, anyhow, bail};
use flate2::{Decompress, FlushDecompress, Status};
use std::io::Write;

use crate::object::Dict;

#[derive(Clone, Debug)]
pub struct Filter {
    pub name: Vec<u8>,
    pub parms: Option<Dict>,
}

fn parm(parms: &Option<Dict>, key: &[u8], default: i64) -> i64 {
    parms.as_ref().and_then(|d| d.get(key)).and_then(|o| o.as_int()).unwrap_or(default)
}

pub fn decode(filters: &[Filter], data: &[u8]) -> Result<Vec<u8>> {
    let mut cur = data.to_vec();
    for f in filters {
        cur = match f.name.as_slice() {
            b"FlateDecode" | b"Fl" => predict(inflate(&cur)?, &f.parms)?,
            b"LZWDecode" | b"LZW" => {
                predict(lzw_decode(&cur, parm(&f.parms, b"EarlyChange", 1) != 0), &f.parms)?
            }
            b"ASCII85Decode" | b"A85" => ascii85_decode(&cur),
            b"ASCIIHexDecode" | b"AHx" => asciihex_decode(&cur),
            b"RunLengthDecode" | b"RL" => runlength_decode(&cur),
            b"Crypt" => cur,
            other => bail!("unsupported stream filter /{}", String::from_utf8_lossy(other)),
        };
    }
    Ok(cur)
}

/// Inflates zlib data, tolerating truncated or slightly corrupt streams the
/// way PDF readers do (whatever decoded before the error is returned).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    for zlib_header in [true, false] {
        let mut d = Decompress::new(zlib_header);
        let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_mul(4).max(1024));
        let mut failed = false;
        loop {
            if out.len() == out.capacity() {
                out.reserve(out.capacity().max(4096));
            }
            let before_in = d.total_in();
            let before_out = d.total_out();
            let input = &data[(d.total_in() as usize).min(data.len())..];
            match d.decompress_vec(input, &mut out, FlushDecompress::None) {
                Ok(Status::StreamEnd) => break,
                Ok(_) => {
                    if d.total_in() == before_in && d.total_out() == before_out {
                        break; // no progress: truncated input
                    }
                }
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if !failed || !out.is_empty() {
            return Ok(out);
        }
    }
    if data.is_empty() {
        return Ok(Vec::new());
    }
    Err(anyhow!("corrupt FlateDecode stream"))
}

/// Inflates only if the data is a complete, well-formed zlib stream.
pub fn inflate_strict(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    decoder.read_to_end(&mut out).ok()?;
    // Trailing bytes after the stream would be dropped by a rewrite; leave those alone.
    (decoder.total_in() as usize + 1 >= data.len()).then_some(out)
}

fn predict(data: Vec<u8>, parms: &Option<Dict>) -> Result<Vec<u8>> {
    let predictor = parm(parms, b"Predictor", 1);
    if predictor <= 1 {
        return Ok(data);
    }
    let colors = parm(parms, b"Colors", 1).max(1) as usize;
    let bpc = parm(parms, b"BitsPerComponent", 8).max(1) as usize;
    let columns = parm(parms, b"Columns", 1).max(1) as usize;
    let bpp = (colors * bpc).div_ceil(8).max(1);
    let row = (colors * bpc * columns).div_ceil(8);
    if predictor == 2 {
        if bpc != 8 {
            bail!("unsupported TIFF predictor with {bpc} bits per component");
        }
        let mut out = data;
        for line in out.chunks_mut(row) {
            for i in bpp..line.len() {
                line[i] = line[i].wrapping_add(line[i - bpp]);
            }
        }
        return Ok(out);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        let tag = chunk[0];
        let mut line = chunk[1..].to_vec();
        line.resize(row, 0);
        for i in 0..row {
            let a = if i >= bpp { line[i - bpp] } else { 0 };
            let b = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            let add = match tag {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((a as u16 + b as u16) / 2) as u8,
                4 => {
                    let p = a as i32 + b as i32 - c as i32;
                    let (pa, pb, pc) = ((p - a as i32).abs(), (p - b as i32).abs(), (p - c as i32).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => 0,
            };
            line[i] = line[i].wrapping_add(add);
        }
        out.extend_from_slice(&line);
        prev = line;
    }
    Ok(out)
}

fn lzw_decode(data: &[u8], early: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut table: Vec<Vec<u8>> = Vec::new();
    let reset = |t: &mut Vec<Vec<u8>>| {
        t.clear();
        for i in 0..256u32 {
            t.push(vec![i as u8]);
        }
        t.push(Vec::new()); // 256 clear
        t.push(Vec::new()); // 257 eod
    };
    reset(&mut table);
    let (mut bits, mut nbits, mut width) = (0u32, 0u32, 9u32);
    let mut prev: Option<Vec<u8>> = None;
    let early = early as usize;
    for &b in data {
        bits = (bits << 8) | b as u32;
        nbits += 8;
        while nbits >= width {
            let code = ((bits >> (nbits - width)) & ((1 << width) - 1)) as usize;
            nbits -= width;
            if code == 256 {
                reset(&mut table);
                width = 9;
                prev = None;
                continue;
            }
            if code == 257 {
                return out;
            }
            let entry = if code < table.len() {
                table[code].clone()
            } else if let Some(p) = &prev {
                let mut e = p.clone();
                e.push(p[0]);
                e
            } else {
                return out;
            };
            out.extend_from_slice(&entry);
            if let Some(p) = prev.take() {
                let mut e = p;
                e.push(entry[0]);
                table.push(e);
            }
            prev = Some(entry);
            let n = table.len() + early;
            width = if n >= 2048 {
                12
            } else if n >= 1024 {
                11
            } else if n >= 512 {
                10
            } else {
                9
            };
        }
    }
    out
}

fn ascii85_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut n = 0;
    let mut i = 0;
    if data.starts_with(b"<~") {
        i = 2;
    }
    while i < data.len() {
        let b = data[i];
        i += 1;
        match b {
            b'~' => break,
            b'z' if n == 0 => out.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                group[n] = b - b'!';
                n += 1;
                if n == 5 {
                    let v = group.iter().fold(0u32, |a, &d| a.wrapping_mul(85).wrapping_add(d as u32));
                    out.extend_from_slice(&v.to_be_bytes());
                    n = 0;
                }
            }
            _ => {}
        }
    }
    if n > 1 {
        for g in group.iter_mut().skip(n) {
            *g = 84;
        }
        let v = group.iter().fold(0u32, |a, &d| a.wrapping_mul(85).wrapping_add(d as u32));
        out.extend_from_slice(&v.to_be_bytes()[..n - 1]);
    }
    out
}

fn asciihex_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut hi: Option<u8> = None;
    for &b in data {
        let v = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            b'>' => break,
            _ => continue,
        };
        match hi.take() {
            Some(h) => out.push(h << 4 | v),
            None => hi = Some(v),
        }
    }
    if let Some(h) = hi {
        out.push(h << 4);
    }
    out
}

fn runlength_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let n = data[i] as usize;
        i += 1;
        if n < 128 {
            let end = (i + n + 1).min(data.len());
            out.extend_from_slice(&data[i..end]);
            i = end;
        } else if n > 128 {
            if let Some(&b) = data.get(i) {
                out.extend(std::iter::repeat_n(b, 257 - n));
            }
            i += 1;
        } else {
            break;
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// zlib level 6
    Fast,
    /// zlib level 9
    Best,
    /// Zopfli: slowest, smallest, output is still ordinary zlib/Flate data
    Max,
}

pub fn deflate(data: &[u8], mode: Compression) -> Vec<u8> {
    let zlib = |level: u32| {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
        e.write_all(data).expect("in-memory write");
        e.finish().expect("in-memory write")
    };
    match mode {
        Compression::Fast => zlib(6),
        Compression::Best => zlib(9),
        Compression::Max => {
            let best = zlib(9);
            // Zopfli is quadratic-ish; keep it for streams where it finishes quickly.
            if data.len() > 8 << 20 {
                return best;
            }
            let mut out = Vec::new();
            match zopfli::compress(zopfli::Options::default(), zopfli::Format::Zlib, data, &mut out) {
                Ok(()) if out.len() < best.len() => out,
                _ => best,
            }
        }
    }
}

/// PNG "Up" predictor (as used by cross-reference streams).
pub fn png_up_encode(data: &[u8], columns: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / columns + 1);
    let mut prev = vec![0u8; columns];
    for row in data.chunks(columns) {
        out.push(2);
        for (i, &b) in row.iter().enumerate() {
            out.push(b.wrapping_sub(prev[i]));
        }
        prev[..row.len()].copy_from_slice(row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flate_roundtrip() {
        let data = b"BT /F1 12 Tf (Hello World) Tj ET ".repeat(50);
        for mode in [Compression::Fast, Compression::Best, Compression::Max] {
            assert_eq!(inflate(&deflate(&data, mode)).unwrap(), data);
        }
    }

    #[test]
    fn png_up_roundtrip() {
        let data: Vec<u8> = (0..40u8).collect();
        let enc = png_up_encode(&data, 5);
        let mut parms = Dict::default();
        parms.set(b"Predictor", crate::object::Object::Int(12));
        parms.set(b"Columns", crate::object::Object::Int(5));
        assert_eq!(predict(enc, &Some(parms)).unwrap(), data);
    }

    #[test]
    fn ascii_filters() {
        assert_eq!(ascii85_decode(b"<~87cURD]i,\"Ebo80~>"), b"Hello World!");
        assert_eq!(asciihex_decode(b"48 65 6C6c 6F>"), b"Hello");
        assert_eq!(runlength_decode(&[2, b'a', b'b', b'c', 254, b'x', 128]), b"abcxxx");
    }
}
