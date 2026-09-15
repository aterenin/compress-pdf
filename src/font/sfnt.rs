//! Minimal OpenType container handling: wrap a bare CFF program (what PDF
//! embeds as `FontFile3`) in an `OTTO` file so HarfBuzz can open it, and
//! take the `CFF ` table back out afterwards.

use read_fonts::ps::cff::CffFontRef;

const OTTO: u32 = 0x4F54_544F;

/// A bare CFF program as an OpenType font with the `head` and `maxp`
/// tables HarfBuzz consults for glyph count and units per em. `None` when
/// the program does not parse as CFF.
pub fn wrap_cff(cff: &[u8]) -> Option<Vec<u8>> {
    let font = CffFontRef::new_cff(cff, 0, None).ok()?;
    let num_glyphs = u16::try_from(font.num_glyphs()).ok()?;
    let upem = u16::try_from(font.upem()).unwrap_or(1000);
    let head = head_table(upem);
    let maxp = [
        0x00,
        0x00,
        0x50,
        0x00,
        (num_glyphs >> 8) as u8,
        num_glyphs as u8,
    ];
    // Table records must be sorted by tag.
    let tables: [(&[u8; 4], &[u8]); 3] = [(b"CFF ", cff), (b"head", &head), (b"maxp", &maxp)];
    Some(build(OTTO, &tables))
}

/// The `CFF ` table of an OpenType font, or `None` when there is none.
pub fn unwrap_cff(sfnt: &[u8]) -> Option<Vec<u8>> {
    let num_tables = usize::from(u16::from_be_bytes(sfnt.get(4..6)?.try_into().ok()?));
    for i in 0..num_tables {
        let rec = sfnt.get(12 + 16 * i..12 + 16 * i + 16)?;
        if &rec[..4] == b"CFF " {
            let offset = u32::from_be_bytes(rec[8..12].try_into().ok()?) as usize;
            let length = u32::from_be_bytes(rec[12..16].try_into().ok()?) as usize;
            return sfnt
                .get(offset..offset.checked_add(length)?)
                .map(<[u8]>::to_vec);
        }
    }
    None
}

/// Whether the bytes start like an OpenType font (TrueType, CFF-based
/// OpenType, Mac `true`, or a collection).
pub fn is_sfnt(data: &[u8]) -> bool {
    matches!(
        data.get(..4),
        Some(b"\x00\x01\x00\x00" | b"OTTO" | b"true" | b"ttcf")
    )
}

fn head_table(upem: u16) -> [u8; 54] {
    let mut head = [0u8; 54];
    head[..4].copy_from_slice(&[0, 1, 0, 0]); // version 1.0
    head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes()); // magic
    head[18..20].copy_from_slice(&upem.to_be_bytes());
    head[46..48].copy_from_slice(&8u16.to_be_bytes()); // lowestRecPPEM
    head[48..50].copy_from_slice(&2u16.to_be_bytes()); // fontDirectionHint
    head
}

fn build(version: u32, tables: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let n = tables.len() as u16;
    let entry_selector = 15 - n.leading_zeros() as u16;
    let search_range: u16 = 16 << entry_selector;
    let mut out = Vec::new();
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&(n * 16 - search_range).to_be_bytes());
    let mut offset = 12 + 16 * tables.len();
    for (tag, data) in tables {
        out.extend_from_slice(*tag);
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().div_ceil(4) * 4;
    }
    for (_, data) in tables {
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(4) * 4, 0);
    }
    out
}

fn checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A two-glyph CFF (`.notdef` and `A`, both empty charstrings) with a
    /// standard charset and encoding; enough for the wrapper and the
    /// subsetter to work on.
    pub(crate) fn tiny_cff() -> Vec<u8> {
        let mut cff = vec![1, 0, 4, 1]; // header: 1.0, hdrSize 4, offSize 1
        // Name INDEX: one name "T".
        cff.extend([0, 1, 1, 1, 2, b'T']);
        // Top DICT INDEX with one dict; offsets filled below.
        let charstrings_op = [29, 0, 0, 0, 0, 17]; // longint placeholder, CharStrings
        let private_op = [28, 0, 0, 29, 0, 0, 0, 0, 18]; // size 0, offset placeholder, Private
        let dict_len = charstrings_op.len() + private_op.len();
        cff.extend([0, 1, 1, 1, (1 + dict_len) as u8]);
        let dict_start = cff.len();
        cff.extend(charstrings_op);
        cff.extend(private_op);
        // String INDEX: empty. Global Subr INDEX: empty.
        cff.extend([0, 0, 0, 0]);
        let charstrings_at = cff.len() as u32;
        // CharStrings INDEX: two glyphs, each just `endchar` (14).
        cff.extend([0, 2, 1, 1, 2, 3, 14, 14]);
        let private_at = cff.len() as u32;
        // Patch the placeholders (longint operands are big-endian).
        cff[dict_start + 1..dict_start + 5].copy_from_slice(&charstrings_at.to_be_bytes());
        cff[dict_start + 10..dict_start + 14].copy_from_slice(&private_at.to_be_bytes());
        cff
    }

    #[test]
    fn wrapped_cff_round_trips_and_parses() {
        let cff = tiny_cff();
        let font = CffFontRef::new_cff(&cff, 0, None).unwrap();
        assert_eq!(font.num_glyphs(), 2);
        let sfnt = wrap_cff(&cff).unwrap();
        assert!(is_sfnt(&sfnt));
        assert_eq!(unwrap_cff(&sfnt).unwrap(), cff);
        let face = read_fonts::FontRef::new(&sfnt).unwrap();
        use read_fonts::TableProvider;
        assert_eq!(face.maxp().unwrap().num_glyphs(), 2);
        assert_eq!(face.head().unwrap().units_per_em(), 1000);
    }

    #[test]
    fn non_fonts_are_rejected() {
        assert!(wrap_cff(b"not a font").is_none());
        assert!(unwrap_cff(b"\x00\x01\x00\x00\x00\x00").is_none());
        assert!(!is_sfnt(b"%!PS-AdobeFont"));
    }
}
