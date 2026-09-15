//! Writing a CFF (Type 1C) program: the container a converted Type 1 font
//! is embedded as. One font, not CID-keyed, no subroutines (charstrings
//! arrive with their subroutines expanded), hints already dropped.
//!
//! Layout, in the order the format fixes: header, Name INDEX, Top DICT
//! INDEX, String INDEX, Global Subr INDEX; then the charset, the custom
//! encoding when the font has one, the CharStrings INDEX and the Private
//! DICT, whose offsets the Top DICT points at. The Top DICT is written
//! with five-byte offset operands so its size is known before the
//! offsets are.

use std::collections::HashMap;

use read_fonts::ps::string::STANDARD_STRINGS;

/// A glyph to write: its name and Type 2 charstring.
pub struct Glyph {
    pub name: Vec<u8>,
    pub charstring: Vec<u8>,
}

pub struct Font<'a> {
    pub name: &'a [u8],
    pub font_matrix: Option<[f64; 6]>,
    pub font_bbox: Option<[f64; 4]>,
    /// Glyph 0 must be `.notdef`.
    pub glyphs: &'a [Glyph],
    /// Code to glyph name for a custom encoding; `None` for the standard
    /// encoding.
    pub encoding: Option<&'a HashMap<u8, Vec<u8>>>,
    /// Private DICT values by Type 1 key.
    pub private: &'a [(String, Vec<f64>)],
}

const DEFAULT_MATRIX: [f64; 6] = [0.001, 0.0, 0.0, 0.001, 0.0, 0.0];

pub fn write(font: &Font<'_>) -> Option<Vec<u8>> {
    if font.glyphs.is_empty() || font.glyphs[0].name != b".notdef" || font.glyphs.len() > 65535 {
        return None;
    }
    let mut strings = Strings::default();
    let parts = Parts {
        charset: charset(font.glyphs, &mut strings),
        encoding: font
            .encoding
            .map(|e| encoding(e, font.glyphs, &mut strings)),
        charstrings: index(font.glyphs.iter().map(|g| g.charstring.as_slice())),
        private: private_dict(font.private),
        name_index: index([sanitized_name(font.name).as_slice()]),
        string_index: index(strings.custom.iter().map(Vec::as_slice)),
    };
    Some(assemble(font, &parts))
}

/// The variable-size pieces, built before any offset is known.
struct Parts {
    charset: Vec<u8>,
    encoding: Option<Vec<u8>>,
    charstrings: Vec<u8>,
    private: Vec<u8>,
    name_index: Vec<u8>,
    string_index: Vec<u8>,
}

fn assemble(font: &Font<'_>, parts: &Parts) -> Vec<u8> {
    const HEADER: [u8; 4] = [1, 0, 4, 4];
    const GLOBAL_SUBRS: [u8; 2] = [0, 0];
    // The top dict's size does not depend on the offsets it holds.
    let placeholder = Offsets {
        encoding: parts.encoding.as_ref().map(|_| 0),
        ..Offsets::default()
    };
    let top_len = top_dict(font, &placeholder, parts.private.len()).len();
    let top_index_len = index([vec![0u8; top_len].as_slice()]).len();
    let fixed = HEADER.len()
        + parts.name_index.len()
        + top_index_len
        + parts.string_index.len()
        + GLOBAL_SUBRS.len();
    let mut offsets = Offsets {
        charset: fixed,
        ..Offsets::default()
    };
    let mut cursor = fixed + parts.charset.len();
    if let Some(enc) = &parts.encoding {
        offsets.encoding = Some(cursor);
        cursor += enc.len();
    }
    offsets.charstrings = cursor;
    offsets.private = cursor + parts.charstrings.len();

    let top = top_dict(font, &offsets, parts.private.len());
    debug_assert_eq!(top.len(), top_len);
    let mut out = Vec::with_capacity(offsets.private + parts.private.len() + 1);
    out.extend_from_slice(&HEADER);
    out.extend_from_slice(&parts.name_index);
    out.extend_from_slice(&index([top.as_slice()]));
    out.extend_from_slice(&parts.string_index);
    out.extend_from_slice(&GLOBAL_SUBRS);
    out.extend_from_slice(&parts.charset);
    if let Some(enc) = &parts.encoding {
        out.extend_from_slice(enc);
    }
    out.extend_from_slice(&parts.charstrings);
    out.extend_from_slice(&parts.private);
    // Some readers refuse a font whose last INDEX has no data; a trailing
    // byte after the Private DICT keeps every offset in range.
    out.push(0);
    out
}

#[derive(Default)]
struct Offsets {
    charset: usize,
    encoding: Option<usize>,
    charstrings: usize,
    private: usize,
}

/// Custom strings get SIDs after the 391 standard ones.
#[derive(Default)]
struct Strings {
    custom: Vec<Vec<u8>>,
    by_name: HashMap<Vec<u8>, u16>,
}

impl Strings {
    fn sid(&mut self, name: &[u8]) -> u16 {
        if let Some(i) = STANDARD_STRINGS.iter().position(|s| s.as_bytes() == name) {
            return i as u16;
        }
        if let Some(&sid) = self.by_name.get(name) {
            return sid;
        }
        let sid = (STANDARD_STRINGS.len() + self.custom.len()) as u16;
        self.custom.push(name.to_vec());
        self.by_name.insert(name.to_vec(), sid);
        sid
    }
}

/// Format 0: one SID per glyph after `.notdef`.
fn charset(glyphs: &[Glyph], strings: &mut Strings) -> Vec<u8> {
    let mut out = vec![0u8];
    for g in &glyphs[1..] {
        out.extend(strings.sid(&g.name).to_be_bytes());
    }
    out
}

/// Format 0 with supplements: glyphs 1..n get the code that first selects
/// them; further codes for the same glyph are supplements. Glyphs with no
/// code must follow the encoded ones, which `sort_glyphs` guarantees.
fn encoding(map: &HashMap<u8, Vec<u8>>, glyphs: &[Glyph], strings: &mut Strings) -> Vec<u8> {
    let mut codes_of: HashMap<&[u8], Vec<u8>> = HashMap::new();
    let mut sorted: Vec<(&u8, &Vec<u8>)> = map.iter().collect();
    sorted.sort();
    for (code, name) in sorted {
        codes_of.entry(name.as_slice()).or_default().push(*code);
    }
    let mut primary = Vec::new();
    let mut supplements: Vec<(u8, u16)> = Vec::new();
    for g in &glyphs[1..] {
        let Some(codes) = codes_of.get(g.name.as_slice()) else {
            break;
        };
        primary.push(codes[0]);
        for &extra in &codes[1..] {
            supplements.push((extra, strings.sid(&g.name)));
        }
    }
    let mut out = vec![
        if supplements.is_empty() { 0 } else { 0x80 },
        primary.len() as u8,
    ];
    out.extend(primary);
    if !supplements.is_empty() {
        out.push(supplements.len() as u8);
        for (code, sid) in supplements {
            out.push(code);
            out.extend(sid.to_be_bytes());
        }
    }
    out
}

/// Glyph order for a custom encoding: `.notdef`, then the glyphs the
/// encoding selects (in code order), then the rest in their own order.
pub fn sort_glyphs(glyphs: Vec<Glyph>, encoding: Option<&HashMap<u8, Vec<u8>>>) -> Vec<Glyph> {
    let Some(map) = encoding else {
        return glyphs;
    };
    let mut first_code: HashMap<&[u8], u8> = HashMap::new();
    let mut codes: Vec<(&u8, &Vec<u8>)> = map.iter().collect();
    codes.sort();
    for (code, name) in codes {
        first_code.entry(name.as_slice()).or_insert(*code);
    }
    let mut rest = glyphs;
    let notdef = rest.remove(0);
    let (mut encoded, unencoded): (Vec<Glyph>, Vec<Glyph>) = rest
        .into_iter()
        .partition(|g| first_code.contains_key(g.name.as_slice()));
    encoded.sort_by_key(|g| first_code[g.name.as_slice()]);
    let mut out = vec![notdef];
    out.extend(encoded);
    out.extend(unencoded);
    out
}

fn top_dict(font: &Font<'_>, offsets: &Offsets, private_len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(bbox) = font.font_bbox {
        for v in bbox {
            number(v, &mut out);
        }
        out.push(5);
    }
    if let Some(m) = font.font_matrix
        && m != DEFAULT_MATRIX
    {
        for v in m {
            number(v, &mut out);
        }
        out.extend([12, 7]);
    }
    offset(offsets.charset, &mut out);
    out.push(15);
    if let Some(enc) = offsets.encoding {
        offset(enc, &mut out);
        out.push(16);
    }
    offset(offsets.charstrings, &mut out);
    out.push(17);
    offset(private_len, &mut out);
    offset(offsets.private, &mut out);
    out.push(18);
    out
}

/// Type 1 private keys with their CFF operators; arrays marked delta are
/// stored as differences from the previous element.
const PRIVATE_OPS: [(&str, &[u8], bool); 14] = [
    ("BlueValues", &[6], true),
    ("OtherBlues", &[7], true),
    ("FamilyBlues", &[8], true),
    ("FamilyOtherBlues", &[9], true),
    ("BlueScale", &[12, 9], false),
    ("BlueShift", &[12, 10], false),
    ("BlueFuzz", &[12, 11], false),
    ("StdHW", &[10], false),
    ("StdVW", &[11], false),
    ("StemSnapH", &[12, 12], true),
    ("StemSnapV", &[12, 13], true),
    ("ForceBold", &[12, 14], false),
    ("LanguageGroup", &[12, 17], false),
    ("ExpansionFactor", &[12, 18], false),
];

fn private_dict(values: &[(String, Vec<f64>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, vals) in values {
        let Some((_, op, delta)) = PRIVATE_OPS.iter().find(|(k, _, _)| k == key) else {
            continue;
        };
        if vals.is_empty() || (*delta && vals.len() % 2 != 0) {
            continue;
        }
        let mut prev = 0.0;
        for &v in vals {
            number(if *delta { v - prev } else { v }, &mut out);
            prev = v;
        }
        out.extend_from_slice(op);
    }
    out
}

/// An INDEX: count, offset size, 1-based offsets, data.
fn index<'a>(items: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let items: Vec<&[u8]> = items.into_iter().collect();
    if items.is_empty() {
        return vec![0, 0];
    }
    let total: usize = items.iter().map(|i| i.len()).sum::<usize>() + 1;
    let off_size = match total {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    let mut out = Vec::new();
    out.extend((items.len() as u16).to_be_bytes());
    out.push(off_size);
    let mut pos = 1usize;
    for len in items.iter().map(|i| i.len()).chain([0]) {
        out.extend_from_slice(&pos.to_be_bytes()[8 - off_size as usize..]);
        pos += len;
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn sanitized_name(name: &[u8]) -> Vec<u8> {
    let cleaned: Vec<u8> = name
        .iter()
        .take(127)
        .map(|&b| {
            if (b'!'..=b'~').contains(&b) && !b"[](){}<>/%".contains(&b) {
                b
            } else {
                b'_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        b"Untitled".to_vec()
    } else {
        cleaned
    }
}

/// A five-byte integer, so the operand's size never depends on its value.
fn offset(v: usize, out: &mut Vec<u8>) {
    out.push(29);
    out.extend((v as i32).to_be_bytes());
}

/// A DICT operand: the shortest integer form for whole numbers, else the
/// nibble-coded real.
fn number(v: f64, out: &mut Vec<u8>) {
    if v.fract() == 0.0 && v.abs() < 2_147_483_648.0 {
        integer(v as i32, out);
    } else {
        real(v, out);
    }
}

fn integer(i: i32, out: &mut Vec<u8>) {
    match i {
        -107..=107 => out.push((i + 139) as u8),
        108..=1131 => {
            let d = i - 108;
            out.extend([(d >> 8) as u8 + 247, d as u8]);
        }
        -1131..=-108 => {
            let d = -i - 108;
            out.extend([(d >> 8) as u8 + 251, d as u8]);
        }
        -32768..=32767 => {
            out.push(28);
            out.extend((i as i16).to_be_bytes());
        }
        _ => {
            out.push(29);
            out.extend(i.to_be_bytes());
        }
    }
}

/// Nibbles: digits, `a` for the point, `b`/`c` for a positive or negative
/// exponent, `e` for minus, `f` to end.
fn real(v: f64, out: &mut Vec<u8>) {
    let text = format!("{v}");
    let mut nibbles: Vec<u8> = Vec::new();
    let mut chars = text.bytes().peekable();
    while let Some(c) = chars.next() {
        nibbles.push(match c {
            b'0'..=b'9' => c - b'0',
            b'.' => 0xA,
            b'e' | b'E' => match chars.next_if_eq(&b'-') {
                Some(_) => 0xC,
                None => 0xB,
            },
            b'-' => 0xE,
            _ => continue,
        });
    }
    nibbles.push(0xF);
    if nibbles.len() % 2 == 1 {
        nibbles.push(0xF);
    }
    out.push(30);
    for pair in nibbles.as_chunks::<2>().0 {
        out.push((pair[0] << 4) | pair[1]);
    }
}

#[cfg(test)]
mod tests {
    use read_fonts::ps::cff::CffFontRef;
    use read_fonts::types::GlyphId;

    use super::*;

    fn glyph(name: &str, cs: &[u8]) -> Glyph {
        Glyph {
            name: name.as_bytes().to_vec(),
            charstring: cs.to_vec(),
        }
    }

    #[test]
    fn written_font_parses_with_names_encoding_and_dicts() {
        let glyphs = vec![
            glyph(".notdef", &[139, 14]),
            glyph("A", &[139, 14]),
            glyph("uni2202", &[139, 14]),
        ];
        let mut enc = HashMap::new();
        enc.insert(65u8, b"A".to_vec());
        enc.insert(97u8, b"A".to_vec());
        enc.insert(100u8, b"uni2202".to_vec());
        let private = vec![
            ("BlueValues".to_string(), vec![-10.0, 0.0, 700.0, 710.0]),
            ("StdHW".to_string(), vec![30.0]),
            ("BlueScale".to_string(), vec![0.039625]),
        ];
        let font = Font {
            name: b"Tiny Font/1",
            font_matrix: Some([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]),
            font_bbox: Some([-5.0, 0.0, 400.0, 700.0]),
            glyphs: &glyphs,
            encoding: Some(&enc),
            private: &private,
        };
        let bytes = write(&font).unwrap();
        let cff = CffFontRef::new_cff(&bytes, 0, None).unwrap();
        assert_eq!(cff.num_glyphs(), 3);
        assert!(!cff.is_cid());
        let charset = cff.charset().unwrap();
        let name = |gid: u32| {
            cff.string(charset.string_id(GlyphId::new(gid)).unwrap())
                .unwrap()
                .to_vec()
        };
        assert_eq!(name(1), b"A");
        assert_eq!(name(2), b"uni2202");
        let encoding = cff.encoding().unwrap();
        assert_eq!(encoding.map(65).unwrap().to_u32(), 1);
        assert_eq!(encoding.map(97).unwrap().to_u32(), 1);
        assert_eq!(encoding.map(100).unwrap().to_u32(), 2);
        assert!(encoding.map(66).is_none());
        let meta = cff.metadata().unwrap();
        assert_eq!(meta.bbox().x_max, read_fonts::types::Fixed::from_i32(400));
        assert_eq!(cff.upem(), 1000);
    }

    #[test]
    fn glyphs_are_ordered_for_the_encoding() {
        let glyphs = vec![
            glyph(".notdef", &[14]),
            glyph("z", &[14]),
            glyph("b", &[14]),
            glyph("a", &[14]),
        ];
        let mut enc = HashMap::new();
        enc.insert(98u8, b"b".to_vec());
        enc.insert(97u8, b"a".to_vec());
        let sorted = sort_glyphs(glyphs, Some(&enc));
        let names: Vec<&[u8]> = sorted.iter().map(|g| g.name.as_slice()).collect();
        assert_eq!(names, [b".notdef" as &[u8], b"a", b"b", b"z"]);
    }

    #[test]
    fn numbers_encode_in_every_form() {
        let mut out = Vec::new();
        for v in [
            0.0, 107.0, 108.0, 1131.0, -1131.0, -1132.0, 32767.0, 40000.0, 0.5, -0.001, 1e-5,
        ] {
            number(v, &mut out);
        }
        assert_eq!(out[0], 139);
        assert_eq!(&out[1..2], &[246]);
        assert_eq!(&out[2..4], &[247, 0]);
        assert_eq!(&out[4..6], &[250, 255]);
        assert_eq!(&out[6..8], &[254, 255]);
        assert_eq!(&out[8..11], &[28, 0xFB, 0x94]);
        assert_eq!(&out[11..14], &[28, 0x7F, 0xFF]);
        assert_eq!(&out[14..19], &[29, 0, 0, 0x9C, 0x40]);
        // 0.5 -> nibbles 0 . 5 f  -> 0x0a 0x5f
        assert_eq!(&out[19..22], &[30, 0x0A, 0x5F]);
        // -0.001 -> e 0 . 0 0 1 f, padded to e0 a0 01 ff
        assert_eq!(&out[22..27], &[30, 0xE0, 0xA0, 0x01, 0xFF]);
        assert_eq!(out[27], 30);
        assert!(
            write(&Font {
                name: b"x",
                font_matrix: None,
                font_bbox: None,
                glyphs: &[glyph("A", &[14])],
                encoding: None,
                private: &[],
            })
            .is_none()
        );
    }
}
