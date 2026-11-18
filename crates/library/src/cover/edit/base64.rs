use super::*;

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i > c.len() {
                '='
            } else {
                ALPHABET[((n >> (18 - i * 6)) & 63) as usize] as char
            });
        }
    }
    out
}

pub(crate) fn decode_base64(text: &str) -> Checked<Vec<u8>> {
    if !text.len().is_multiple_of(4) {
        return Err("invalid_image");
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (i, c) in text.as_bytes().as_chunks::<4>().0.iter().enumerate() {
        let mut n = 0u32;
        let mut pad = 0;
        for (j, b) in c.iter().enumerate() {
            let v = if *b == b'=' && j >= 2 {
                pad += 1;
                0
            } else {
                if pad > 0 {
                    return Err("invalid_image");
                }
                ALPHABET
                    .iter()
                    .position(|x| x == b)
                    .ok_or("invalid_image")? as u32
            };
            n = (n << 6) | v;
        }
        if pad > 0 && i + 1 != text.len() / 4 {
            return Err("invalid_image");
        }
        if (pad == 1 && n & 255 != 0) || (pad == 2 && n & 65535 != 0) {
            return Err("invalid_image");
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}
