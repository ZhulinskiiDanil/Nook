//! Text sniffing for `peek`: UTF-8 / UTF-16 (with or without BOM) / CP1251.

use encoding_rs::{UTF_16BE, UTF_16LE, WINDOWS_1251};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Cp1251,
}

/// Looks like binary: NULs that are not explained by UTF-16.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(4096)];
    if sample.is_empty() {
        return false;
    }
    let ctrl = sample
        .iter()
        .filter(|&&b| b < 0x09 || (b > 0x0d && b < 0x20 && b != 0x1b))
        .count();
    ctrl * 10 > sample.len()
}

fn utf16_without_bom(bytes: &[u8]) -> Option<TextEncoding> {
    if bytes.len() < 4 {
        return None;
    }
    let pairs = bytes.len() / 2;
    let odd_zero = bytes.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    let even_zero = bytes.iter().step_by(2).filter(|&&b| b == 0).count();
    // Cyrillic UTF-16LE has 0x04 high bytes rather than 0x00; count those too.
    let odd_cyr = bytes
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&b| b == 0x04)
        .count();
    if (odd_zero + odd_cyr) * 10 >= pairs * 7 && even_zero * 10 < pairs {
        Some(TextEncoding::Utf16Le)
    } else if even_zero * 10 >= pairs * 7 && odd_zero * 10 < pairs {
        Some(TextEncoding::Utf16Be)
    } else {
        None
    }
}

/// Decodes up to `limit` input bytes. `None` if the data looks binary.
pub fn decode_prefix(bytes: &[u8], limit: usize) -> Option<(String, TextEncoding)> {
    let bytes = &bytes[..bytes.len().min(limit)];
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some((
            String::from_utf8_lossy(trim_utf8_tail(rest)).into_owned(),
            TextEncoding::Utf8,
        ));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some((
            UTF_16LE
                .decode_without_bom_handling(even(rest))
                .0
                .into_owned(),
            TextEncoding::Utf16Le,
        ));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some((
            UTF_16BE
                .decode_without_bom_handling(even(rest))
                .0
                .into_owned(),
            TextEncoding::Utf16Be,
        ));
    }
    if let Some(enc) = utf16_without_bom(bytes) {
        let codec = if enc == TextEncoding::Utf16Le {
            UTF_16LE
        } else {
            UTF_16BE
        };
        return Some((
            codec
                .decode_without_bom_handling(even(bytes))
                .0
                .into_owned(),
            enc,
        ));
    }
    // Not UTF-16, so any NUL means binary (PNG with an XMP text chunk, PSD, PE...).
    if bytes.contains(&0) || looks_binary(bytes) {
        return None;
    }
    let trimmed = trim_utf8_tail(bytes);
    if let Ok(s) = std::str::from_utf8(trimmed) {
        return Some((s.to_string(), TextEncoding::Utf8));
    }
    Some((
        WINDOWS_1251
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
        TextEncoding::Cp1251,
    ))
}

fn even(b: &[u8]) -> &[u8] {
    &b[..b.len() & !1]
}

/// Drops an incomplete UTF-8 sequence cut off by the byte limit.
fn trim_utf8_tail(b: &[u8]) -> &[u8] {
    match std::str::from_utf8(b) {
        Err(e) if e.error_len().is_none() => &b[..e.valid_up_to()],
        _ => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RU: &str = "Семен КС конфиги: sensitivity 2.1";

    #[test]
    fn utf8_plain_and_bom() {
        assert_eq!(
            decode_prefix(RU.as_bytes(), 4096).unwrap(),
            (RU.to_string(), TextEncoding::Utf8)
        );
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(RU.as_bytes());
        assert_eq!(decode_prefix(&b, 4096).unwrap().0, RU);
    }

    #[test]
    fn utf8_cut_mid_char() {
        let b = RU.as_bytes();
        let (s, e) = decode_prefix(b, 3).unwrap(); // "С" is 2 bytes, "е" cut
        assert_eq!((s.as_str(), e), ("С", TextEncoding::Utf8));
    }

    #[test]
    fn utf16_with_and_without_bom() {
        let le: Vec<u8> = RU.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.extend_from_slice(&le);
        assert_eq!(
            decode_prefix(&with_bom, 4096).unwrap(),
            (RU.to_string(), TextEncoding::Utf16Le)
        );
        assert_eq!(
            decode_prefix(&le, 4096).unwrap(),
            (RU.to_string(), TextEncoding::Utf16Le)
        );
        let ascii_le: Vec<u8> = "hello world"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(
            decode_prefix(&ascii_le, 4096).unwrap().1,
            TextEncoding::Utf16Le
        );
    }

    #[test]
    fn cp1251() {
        let (bytes, _, _) = WINDOWS_1251.encode("Привет, мир! конфиг");
        assert_eq!(
            decode_prefix(&bytes, 4096).unwrap(),
            ("Привет, мир! конфиг".to_string(), TextEncoding::Cp1251)
        );
    }

    #[test]
    fn binary_rejected() {
        let png = [
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
            0, 0, 1, 0, 0, 0, 1, 0, 8, 6, 0, 0, 0,
        ];
        assert!(decode_prefix(&png, 4096).is_none());
        // PNG whose header is followed by a long XMP text chunk.
        let mut xmp = png.to_vec();
        xmp.extend(std::iter::repeat_n(b'x', 3000));
        assert!(decode_prefix(&xmp, 4096).is_none());
    }
}
