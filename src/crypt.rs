//! Standard security handler (RC4, AES-128, AES-256), so that encrypted
//! documents can be edited without changing how they are protected.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use anyhow::{Result, bail};
use md5::{Digest, Md5};
use sha2::{Sha256, Sha384, Sha512};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{Dict, Object};

const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08, 0x2E, 0x2E, 0x00,
    0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    None,
    Rc4,
    AesV2,
    AesV3,
}

pub struct Crypt {
    key: Vec<u8>,
    strings: Method,
    streams: Method,
    encrypt_metadata: bool,
}

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|&b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            b ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize]
        })
        .collect()
}

fn md5(parts: &[&[u8]]) -> Vec<u8> {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().to_vec()
}

enum Aes {
    K128(aes::Aes128),
    K256(aes::Aes256),
}

impl Aes {
    fn new(key: &[u8]) -> Result<Aes> {
        Ok(match key.len() {
            16 => Aes::K128(aes::Aes128::new(GenericArray::from_slice(key))),
            32 => Aes::K256(aes::Aes256::new(GenericArray::from_slice(key))),
            n => bail!("unsupported AES key length {n}"),
        })
    }
    fn enc(&self, block: &mut [u8]) {
        let b = GenericArray::from_mut_slice(block);
        match self {
            Aes::K128(c) => c.encrypt_block(b),
            Aes::K256(c) => c.encrypt_block(b),
        }
    }
    fn dec(&self, block: &mut [u8]) {
        let b = GenericArray::from_mut_slice(block);
        match self {
            Aes::K128(c) => c.decrypt_block(b),
            Aes::K256(c) => c.decrypt_block(b),
        }
    }
}

fn cbc_encrypt_raw(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let aes = Aes::new(key)?;
    let mut prev = iv.to_vec();
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block: Vec<u8> = chunk.iter().zip(&prev).map(|(a, b)| a ^ b).collect();
        aes.enc(&mut block);
        out.extend_from_slice(&block);
        prev = block;
    }
    Ok(out)
}

fn cbc_decrypt_raw(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let aes = Aes::new(key)?;
    let mut prev = iv.to_vec();
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = chunk.to_vec();
        aes.dec(&mut block);
        out.extend(block.iter().zip(&prev).map(|(a, b)| a ^ b));
        prev = chunk.to_vec();
    }
    Ok(out)
}

/// AES-CBC with the IV prepended and PKCS#7 padding, as PDF stores it.
fn aes_decrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 16 {
        return Ok(Vec::new());
    }
    let body = &data[16..];
    let mut out = cbc_decrypt_raw(key, &data[..16], &body[..body.len() - body.len() % 16])?;
    if let Some(&pad) = out.last() {
        if (1..=16).contains(&pad) && out.len() >= pad as usize {
            out.truncate(out.len() - pad as usize);
        }
    }
    Ok(out)
}

fn aes_encrypt(key: &[u8], data: &[u8], salt: u64) -> Result<Vec<u8>> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let iv = md5(&[&nanos.to_le_bytes(), &n.to_le_bytes(), &salt.to_le_bytes(), &std::process::id().to_le_bytes()]);
    let pad = 16 - data.len() % 16;
    let mut padded = data.to_vec();
    padded.extend(std::iter::repeat_n(pad as u8, pad));
    let mut out = iv.clone();
    out.extend(cbc_encrypt_raw(key, &iv, &padded)?);
    Ok(out)
}

/// ISO 32000-2 algorithm 2.B (revision 6 hash); revision 5 is plain SHA-256.
fn hash_r56(revision: i64, password: &[u8], salt: &[u8], udata: &[u8]) -> Result<Vec<u8>> {
    let mut k = Sha256::new().chain_update(password).chain_update(salt).chain_update(udata).finalize().to_vec();
    if revision < 6 {
        return Ok(k);
    }
    let mut round = 0u32;
    loop {
        let mut k1 = Vec::with_capacity((password.len() + k.len() + udata.len()) * 64);
        for _ in 0..64 {
            k1.extend_from_slice(password);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        let e = cbc_encrypt_raw(&k[..16], &k[16..32], &k1)?;
        let m: u32 = e[..16].iter().map(|&b| b as u32).sum::<u32>() % 3;
        k = match m {
            0 => Sha256::digest(&e).to_vec(),
            1 => Sha384::digest(&e).to_vec(),
            _ => Sha512::digest(&e).to_vec(),
        };
        round += 1;
        if round >= 64 && *e.last().unwrap() as u32 <= round - 32 {
            break;
        }
    }
    k.truncate(32);
    Ok(k)
}

fn bytes<'a>(dict: &'a Dict, key: &[u8]) -> &'a [u8] {
    dict.get(key).and_then(Object::as_str).unwrap_or(b"")
}

impl Crypt {
    pub fn new(dict: &Dict, id0: &[u8], password: &[u8]) -> Result<Crypt> {
        if dict.name(b"Filter").is_some_and(|f| f != b"Standard") {
            bail!("unsupported encryption handler (only the Standard security handler is supported)");
        }
        let v = dict.get(b"V").and_then(Object::as_int).unwrap_or(0);
        let r = dict.get(b"R").and_then(Object::as_int).unwrap_or(2);
        let p = dict.get(b"P").and_then(Object::as_int).unwrap_or(0) as u32;
        let (o, u) = (bytes(dict, b"O"), bytes(dict, b"U"));
        let encrypt_metadata = !matches!(dict.get(b"EncryptMetadata"), Some(Object::Bool(false)));

        let method_of = |key: &[u8]| -> Result<Method> {
            let name = dict.name(key).unwrap_or(b"Identity");
            if name == b"Identity" {
                return Ok(Method::None);
            }
            let cf = dict.get(b"CF").and_then(Object::as_dict).and_then(|d| d.get(name)).and_then(Object::as_dict);
            Ok(match cf.and_then(|d| d.name(b"CFM")) {
                Some(b"V2") => Method::Rc4,
                Some(b"AESV2") => Method::AesV2,
                Some(b"AESV3") => Method::AesV3,
                Some(b"None") | None => Method::None,
                Some(other) => bail!("unsupported crypt filter method /{}", String::from_utf8_lossy(other)),
            })
        };
        let (strings, streams) = match v {
            1 | 2 => (Method::Rc4, Method::Rc4),
            4 | 5 => (method_of(b"StrF")?, method_of(b"StmF")?),
            _ => bail!("unsupported encryption version V={v}"),
        };

        if v == 5 {
            if o.len() < 48 || u.len() < 48 {
                bail!("malformed encryption dictionary");
            }
            let pw = &password[..password.len().min(127)];
            let (oe, ue) = (bytes(dict, b"OE"), bytes(dict, b"UE"));
            let zero = [0u8; 16];
            let key = if hash_r56(r, pw, &u[32..40], b"")? == u[..32] && ue.len() >= 32 {
                cbc_decrypt_raw(&hash_r56(r, pw, &u[40..48], b"")?, &zero, &ue[..32])?
            } else if hash_r56(r, pw, &o[32..40], &u[..48])? == o[..32] && oe.len() >= 32 {
                cbc_decrypt_raw(&hash_r56(r, pw, &o[40..48], &u[..48])?, &zero, &oe[..32])?
            } else {
                bail!("{}", wrong_password(password));
            };
            return Ok(Crypt { key, strings, streams, encrypt_metadata });
        }

        if o.len() < 32 || u.len() < 32 {
            bail!("malformed encryption dictionary");
        }
        let n = if r == 2 { 5 } else { (dict.get(b"Length").and_then(Object::as_int).unwrap_or(40) / 8).clamp(5, 16) as usize };
        let pad = |pw: &[u8]| -> Vec<u8> {
            let mut v = pw[..pw.len().min(32)].to_vec();
            v.extend_from_slice(&PAD[..32 - v.len()]);
            v
        };
        let file_key = |user_pw: &[u8]| -> Vec<u8> {
            let mut h = md5(&[
                &pad(user_pw),
                &o[..32],
                &p.to_le_bytes(),
                id0,
                if r >= 4 && !encrypt_metadata { &[0xFF; 4] } else { &[] },
            ]);
            if r >= 3 {
                for _ in 0..50 {
                    h = md5(&[&h[..n]]);
                }
            }
            h[..n].to_vec()
        };
        let check_user = |key: &[u8]| -> bool {
            if r == 2 {
                rc4(key, &PAD) == u[..32]
            } else {
                let mut x = rc4(key, &md5(&[&PAD, id0]));
                for i in 1..=19u8 {
                    let k: Vec<u8> = key.iter().map(|b| b ^ i).collect();
                    x = rc4(&k, &x);
                }
                x[..16] == u[..16]
            }
        };
        let key = file_key(password);
        if check_user(&key) {
            return Ok(Crypt { key, strings, streams, encrypt_metadata });
        }
        // Try the password as the owner password: recover the user password from /O.
        let mut h = md5(&[&pad(password)]);
        if r >= 3 {
            for _ in 0..50 {
                h = md5(&[&h]);
            }
        }
        let okey = &h[..n];
        let mut user = o[..32].to_vec();
        if r == 2 {
            user = rc4(okey, &user);
        } else {
            for i in (0..=19u8).rev() {
                let k: Vec<u8> = okey.iter().map(|b| b ^ i).collect();
                user = rc4(&k, &user);
            }
        }
        let key = file_key(&user);
        if check_user(&key) {
            return Ok(Crypt { key, strings, streams, encrypt_metadata });
        }
        bail!("{}", wrong_password(password))
    }

    fn object_key(&self, num: u32, generation: u16, method: Method) -> Vec<u8> {
        if method == Method::AesV3 {
            return self.key.clone();
        }
        let n = num.to_le_bytes();
        let g = generation.to_le_bytes();
        let salt: &[u8] = if method == Method::AesV2 { b"sAlT" } else { b"" };
        let mut h = md5(&[&self.key, &n[..3], &g, salt]);
        h.truncate((self.key.len() + 5).min(16));
        h
    }

    fn apply(&self, method: Method, num: u32, generation: u16, data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
        let key = self.object_key(num, generation, method);
        match method {
            Method::None => Ok(data.to_vec()),
            Method::Rc4 => Ok(rc4(&key, data)),
            Method::AesV2 | Method::AesV3 => {
                if encrypt {
                    aes_encrypt(&key, data, (num as u64) << 16 | generation as u64)
                } else {
                    aes_decrypt(&key, data)
                }
            }
        }
    }

    fn stream_method(&self, dict: &Dict) -> Method {
        if dict.name(b"Type") == Some(b"XRef") {
            return Method::None;
        }
        if dict.name(b"Type") == Some(b"Metadata") && !self.encrypt_metadata {
            return Method::None;
        }
        // An explicit /Crypt filter (in practice always Identity) opts the stream out.
        let has_crypt = match dict.get(b"Filter") {
            Some(Object::Name(n)) => n == b"Crypt",
            Some(Object::Array(a)) => a.iter().any(|o| o.as_name() == Some(b"Crypt")),
            _ => false,
        };
        if has_crypt { Method::None } else { self.streams }
    }

    pub fn decrypt_stream(&self, num: u32, generation: u16, dict: &Dict, data: &[u8]) -> Result<Vec<u8>> {
        self.apply(self.stream_method(dict), num, generation, data, false)
    }

    pub fn encrypt_stream(&self, num: u32, generation: u16, dict: &Dict, data: &[u8]) -> Result<Vec<u8>> {
        self.apply(self.stream_method(dict), num, generation, data, true)
    }

    fn walk(&self, num: u32, generation: u16, obj: &mut Object, encrypt: bool) {
        match obj {
            Object::Str(s, _) => {
                if let Ok(v) = self.apply(self.strings, num, generation, s, encrypt) {
                    *s = v;
                }
            }
            Object::Array(a) => a.iter_mut().for_each(|o| self.walk(num, generation, o, encrypt)),
            Object::Dict(d) => d.0.iter_mut().for_each(|(_, o)| self.walk(num, generation, o, encrypt)),
            _ => {}
        }
    }

    pub fn decrypt_object(&self, num: u32, generation: u16, obj: &mut Object) {
        self.walk(num, generation, obj, false)
    }
    pub fn encrypt_object(&self, num: u32, generation: u16, obj: &mut Object) {
        self.walk(num, generation, obj, true)
    }
    pub fn decrypt_dict(&self, num: u32, generation: u16, dict: &mut Dict) {
        dict.0.iter_mut().for_each(|(_, o)| self.walk(num, generation, o, false))
    }
    pub fn encrypt_dict(&self, num: u32, generation: u16, dict: &mut Dict) {
        dict.0.iter_mut().for_each(|(_, o)| self.walk(num, generation, o, true))
    }
}

fn wrong_password(given: &[u8]) -> &'static str {
    if given.is_empty() {
        "this PDF is encrypted and needs a password (use --password)"
    } else {
        "incorrect password for this encrypted PDF"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_vector() {
        assert_eq!(rc4(b"Key", b"Plaintext"), [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]);
    }

    #[test]
    fn aes_roundtrip() {
        for key in [vec![7u8; 16], vec![9u8; 32]] {
            for len in [0usize, 1, 15, 16, 17, 100] {
                let data: Vec<u8> = (0..len as u8).collect();
                let enc = aes_encrypt(&key, &data, 1).unwrap();
                assert_eq!(enc.len() % 16, 0);
                assert_eq!(aes_decrypt(&key, &enc).unwrap(), data);
            }
        }
    }
}
