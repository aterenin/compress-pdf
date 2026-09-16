//! Subsetting through HarfBuzz, with glyph IDs retained so nothing in the
//! PDF that refers to a glyph (content strings, CMaps, `CIDToGIDMap`, the
//! font's own `cmap`) has to change. Unused glyphs keep their ID and lose
//! their outline.

use std::collections::BTreeSet;

use hb_subset::{Blob, FontFace, SubsetInput};
use read_fonts::ps::cff::CffFontRef;
use read_fonts::ps::cs::CommandSink;
use read_fonts::types::{Fixed, GlyphId};

use super::sfnt;

/// What kind of program the bytes are, which decides the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    /// TrueType or OpenType, already an sfnt.
    Sfnt,
    /// A bare CFF program (`FontFile3` with subtype Type1C or CIDFontType0C).
    Cff,
}

/// Tables copied through unchanged: a viewer's lookup path from character
/// code to glyph, which retained glyph IDs keep correct.
const PASS_THROUGH_TABLES: [&[u8; 4]; 2] = [b"cmap", b"post"];

/// Tables a PDF consumer never reads: layout, legacy kerning, hinting
/// helpers, signatures, and `OS/2`, whose selection and embedding metadata
/// a viewer takes from the font descriptor instead (and which producers
/// write empty or short often enough that HarfBuzz would otherwise refuse
/// the font). Dropping them is a pure saving.
const DROP_TABLES: [&[u8; 4]; 13] = [
    b"GSUB", b"GPOS", b"GDEF", b"BASE", b"JSTF", b"kern", b"DSIG", b"hdmx", b"LTSH", b"VDMX",
    b"gasp", b"PCLT", b"OS/2",
];

/// The program reduced to `glyphs` (glyph 0 is always kept), or `None`
/// when HarfBuzz cannot handle it. `keep_names` retains glyph names
/// (`post`, CFF charset), which simple fonts addressed by name need.
pub fn subset(
    program: &[u8],
    kind: Program,
    glyphs: &BTreeSet<u32>,
    keep_names: bool,
) -> Option<Vec<u8>> {
    let wrapped;
    let bytes = match kind {
        Program::Sfnt => program,
        Program::Cff => {
            wrapped = sfnt::wrap_cff(program)?;
            &wrapped
        }
    };
    let face = FontFace::new(Blob::from_bytes(bytes).ok()?).ok()?;
    if face.glyph_count() == 0 {
        return None;
    }
    let mut input = SubsetInput::new().ok()?;
    {
        let mut set = input.glyph_set();
        set.insert(0);
        for &g in glyphs {
            set.insert(g);
        }
    }
    {
        let mut drop = input.drop_table_tag_set();
        for tag in DROP_TABLES {
            drop.insert(hb_subset::Tag::new(tag));
        }
    }
    {
        // With glyph IDs retained the original code-to-glyph tables stay
        // valid, and HarfBuzz would otherwise drop the subtables it does
        // not rewrite (the Macintosh one simple fonts often rely on) and
        // sometimes the glyph names; keep both untouched.
        let mut keep = input.no_subset_table_tag_set();
        for tag in PASS_THROUGH_TABLES {
            keep.insert(hb_subset::Tag::new(tag));
        }
    }
    let mut flags = input.flags();
    flags
        .retain_glyph_indices()
        .retain_notdef_outline()
        .retain_unrecognized_tables();
    if hints_removable(bytes) {
        flags.remove_hinting();
    }
    if keep_names {
        flags.retain_glyph_names();
    } else {
        flags.remove_glyph_names();
    }
    drop(flags);
    let out = input.subset_font(&face).ok()?;
    let blob = out.underlying_blob();
    let result: Vec<u8> = (*blob).to_vec();
    match kind {
        Program::Sfnt => Some(result),
        Program::Cff => sfnt::unwrap_cff(&result),
    }
}

/// Whether HarfBuzz may strip the hints. Its CFF hint removal assumes
/// stem hints come before the first path operator, as the Type 2 format
/// requires; charstrings Ghostscript converts from Type 1 carry the
/// original hint replacement as stem operators in the middle of the
/// glyph, and there the removal also discards the outline drawn before
/// them (HarfBuzz 8 and 14 alike). Such programs keep their hints, as
/// does anything read-fonts cannot evaluate. TrueType hinting is separate
/// data and always removable.
fn hints_removable(sfnt: &[u8]) -> bool {
    let Some(range) = sfnt::table_range(sfnt, b"CFF ") else {
        return true;
    };
    let Ok(font) = CffFontRef::new_cff(&sfnt[range], 0, None) else {
        return false;
    };
    let mut sink = LateHints::default();
    for gid in 0..font.num_glyphs() {
        let gid = GlyphId::new(gid);
        let index = font.subfont_index(gid).unwrap_or(0);
        let Ok(subfont) = font.subfont(index, &[]) else {
            return false;
        };
        sink.drawing = false;
        if font
            .evaluate_charstring(&subfont, gid, &[], &mut sink)
            .is_err()
            || sink.late
        {
            return false;
        }
    }
    true
}

/// Notes a stem hint that arrives after the glyph has started drawing.
#[derive(Default)]
struct LateHints {
    drawing: bool,
    late: bool,
    stems: usize,
}

impl CommandSink for LateHints {
    fn move_to(&mut self, _: Fixed, _: Fixed) {
        self.drawing = true;
    }
    fn line_to(&mut self, _: Fixed, _: Fixed) {
        self.drawing = true;
    }
    fn curve_to(&mut self, _: Fixed, _: Fixed, _: Fixed, _: Fixed, _: Fixed, _: Fixed) {
        self.drawing = true;
    }
    fn close(&mut self) {}
    fn hstem(&mut self, _: Fixed, _: Fixed) {
        self.stems += 1;
        self.late |= self.drawing;
    }
    fn vstem(&mut self, _: Fixed, _: Fixed) {
        self.stems += 1;
        self.late |= self.drawing;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::cff;
    use crate::font::sfnt::tests::tiny_cff;

    /// A one-glyph CFF whose `A` is `ops` (Type 2 bytes).
    fn font_with(ops: &[u8]) -> Vec<u8> {
        let glyphs = [
            cff::Glyph {
                name: b".notdef".to_vec(),
                charstring: vec![14],
            },
            cff::Glyph {
                name: b"A".to_vec(),
                charstring: ops.to_vec(),
            },
        ];
        cff::write(&cff::Font {
            name: b"T",
            font_matrix: None,
            font_bbox: None,
            glyphs: &glyphs,
            encoding: None,
            private: &[],
        })
        .unwrap()
    }

    fn stems_in(cff: &[u8]) -> usize {
        let font = CffFontRef::new_cff(cff, 0, None).unwrap();
        let subfont = font.subfont(0, &[]).unwrap();
        let mut sink = LateHints::default();
        font.evaluate_charstring(&subfont, GlyphId::new(1), &[], &mut sink)
            .unwrap();
        sink.stems
    }

    #[test]
    fn hints_after_the_first_move_are_kept_and_ordinary_hints_are_removed() {
        // 10 20 hstem 100 100 rmoveto 50 hlineto endchar
        let ordinary = font_with(&[149, 159, 1, 239, 239, 21, 189, 6, 14]);
        // 100 100 rmoveto 50 hlineto 10 20 hstem 50 hlineto endchar
        let late = font_with(&[239, 239, 21, 189, 6, 149, 159, 1, 189, 6, 14]);
        assert!(hints_removable(&sfnt::wrap_cff(&ordinary).unwrap()));
        assert!(!hints_removable(&sfnt::wrap_cff(&late).unwrap()));
        let glyphs = BTreeSet::from([1]);
        let out = subset(&ordinary, Program::Cff, &glyphs, true).unwrap();
        assert_eq!(
            stems_in(&out),
            0,
            "hints removed from a well-formed program"
        );
        let out = subset(&late, Program::Cff, &glyphs, true).unwrap();
        assert_eq!(
            stems_in(&out),
            1,
            "hints kept where removal would cut the outline"
        );
    }

    #[test]
    fn cff_subset_keeps_glyph_count_and_round_trips() {
        let cff = tiny_cff();
        let out = subset(&cff, Program::Cff, &BTreeSet::from([1]), true).unwrap();
        let font = read_fonts::ps::cff::CffFontRef::new_cff(&out, 0, None).unwrap();
        assert_eq!(font.num_glyphs(), 2);
        assert!(subset(b"garbage", Program::Cff, &BTreeSet::new(), true).is_none());
        assert!(subset(b"garbage", Program::Sfnt, &BTreeSet::new(), true).is_none());
    }
}
