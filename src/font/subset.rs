//! Subsetting through HarfBuzz, with glyph IDs retained so nothing in the
//! PDF that refers to a glyph (content strings, CMaps, `CIDToGIDMap`, the
//! font's own `cmap`) has to change. Unused glyphs keep their ID and lose
//! their outline.

use std::collections::BTreeSet;

use hb_subset::{Blob, FontFace, SubsetInput};

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
        .remove_hinting()
        .retain_notdef_outline()
        .retain_unrecognized_tables();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::sfnt::tests::tiny_cff;

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
