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
    table_range(sfnt, b"CFF ").map(|r| sfnt[r].to_vec())
}

/// Puts a TrueType or OpenType program into the shape the readers assume
/// without changing any table's content: the directory sorted by tag with
/// records that point outside the file dropped, `head` at version 1.0, and
/// `maxp`'s glyph count no larger than `loca` provides for. Each of these
/// was seen in the corpus (a producer that appends the `loca` and `glyf` it
/// rewrote after the other tables, garbage records, a `head` version of
/// 241, a glyph count six times the offsets), and each makes HarfBuzz and
/// read-fonts, which look tables up by binary search and sanitize what
/// they find, reject the whole font. Collections and truncated
/// directories are left alone.
pub fn normalize(data: &mut [u8]) {
    if !is_sfnt(data) || data.starts_with(b"ttcf") {
        return;
    }
    let len = data.len();
    let Some(count) = data.get(4..6) else {
        return;
    };
    let num_tables = usize::from(u16::from_be_bytes([count[0], count[1]]));
    let Some(directory) = data.get_mut(12..12 + 16 * num_tables) else {
        return;
    };
    let (records, _) = directory.as_chunks_mut::<16>();
    let mut kept: Vec<[u8; 16]> = records
        .iter()
        .copied()
        .filter(|rec| record_range(rec).is_some_and(|r| r.end <= len))
        .collect();
    kept.sort_by(|a, b| a[..4].cmp(&b[..4]));
    for (slot, rec) in records.iter_mut().zip(&kept) {
        *slot = *rec;
    }
    data[4..6].copy_from_slice(&(kept.len() as u16).to_be_bytes());
    repair_head_version(data);
    cap_glyph_count(data);
}

/// The `head` table has only ever had version 1.0, and readers refuse
/// any other major version.
fn repair_head_version(data: &mut [u8]) {
    if let Some(head) = table_range(data, b"head")
        && head.len() >= 4
        && data[head.start..head.start + 2] != [0, 1]
    {
        data[head.start..head.start + 4].copy_from_slice(&[0, 1, 0, 0]);
    }
}

/// `maxp` may claim more glyphs than `loca` has offsets for; the extra
/// glyph IDs have no outline either way.
fn cap_glyph_count(data: &mut [u8]) {
    let (Some(maxp), Some(head), Some(loca)) = (
        table_range(data, b"maxp"),
        table_range(data, b"head"),
        table_range(data, b"loca"),
    ) else {
        return;
    };
    if maxp.len() < 6 || head.len() < 52 {
        return;
    }
    let long_offsets = data[head.start + 50..head.start + 52] != [0, 0];
    let entries = loca.len() / if long_offsets { 4 } else { 2 };
    let claimed = u16::from_be_bytes([data[maxp.start + 4], data[maxp.start + 5]]);
    if let Some(available) = entries.checked_sub(1).and_then(|n| u16::try_from(n).ok())
        && claimed > available
    {
        data[maxp.start + 4..maxp.start + 6].copy_from_slice(&available.to_be_bytes());
    }
}

/// The byte range a directory record points at.
fn record_range(rec: &[u8]) -> Option<std::ops::Range<usize>> {
    let offset = u32::from_be_bytes(rec.get(8..12)?.try_into().ok()?) as usize;
    let length = u32::from_be_bytes(rec.get(12..16)?.try_into().ok()?) as usize;
    Some(offset..offset.checked_add(length)?)
}

/// Where the table `tag` lives, from a scan of the directory.
pub fn table_range(data: &[u8], tag: &[u8; 4]) -> Option<std::ops::Range<usize>> {
    let num_tables = usize::from(u16::from_be_bytes(data.get(4..6)?.try_into().ok()?));
    (0..num_tables)
        .filter_map(|i| data.get(12 + 16 * i..28 + 16 * i))
        .find(|rec| &rec[..4] == tag)
        .and_then(record_range)
        .filter(|r| r.end <= data.len())
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
    fn normalize_sorts_the_directory_and_repairs_the_counts() {
        let mut head = [0u8; 54];
        head[..2].copy_from_slice(&[0, 241]); // a head version no reader accepts
        let maxp = [0, 1, 0, 0, 0, 9]; // claims 9 glyphs
        let loca = [0u8; 8]; // short offsets: 4 entries, so 3 glyphs
        let mut font = build(
            0x0001_0000,
            &[
                (b"maxp", &maxp),
                (b"loca", &loca),
                (b"head", &head),
                (b"junk", &[1; 4]),
            ],
        );
        let junk = 12 + 16 * 3;
        font[junk + 8..junk + 12].copy_from_slice(&u32::MAX.to_be_bytes()); // points outside
        let tags = |f: &[u8]| {
            let n = usize::from(f[5]);
            (0..n)
                .map(|i| f[12 + 16 * i..16 + 16 * i].to_vec())
                .collect::<Vec<_>>()
        };
        assert_eq!(tags(&font), [b"maxp", b"loca", b"head", b"junk"]);
        let body = font[12 + 64..].to_vec();
        normalize(&mut font);
        assert_eq!(tags(&font), [b"head", b"loca", b"maxp"]);
        let head = table_range(&font, b"head").unwrap();
        assert_eq!(font[head.start..head.start + 4], [0, 1, 0, 0]);
        let maxp = table_range(&font, b"maxp").unwrap();
        assert_eq!(font[maxp.start + 4..maxp.start + 6], [0, 3]);
        let mut expected = body.clone();
        expected[4..6].copy_from_slice(&[0, 3]); // maxp glyph count
        expected[16..20].copy_from_slice(&[0, 1, 0, 0]); // head version
        assert_eq!(font[12 + 64..], expected[..], "nothing else changed");
        let before = font.clone();
        normalize(&mut font);
        assert_eq!(font, before, "a normalized font is a fixed point");
        for short in [
            &b"\x00\x01\x00\x00\x00"[..],
            b"\x00\x01\x00\x00\x00\x09",
            b"ttcf\x00\x02",
        ] {
            let mut data = short.to_vec();
            normalize(&mut data);
            assert_eq!(data, short);
        }
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
