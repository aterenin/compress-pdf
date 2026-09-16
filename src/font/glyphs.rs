//! Which glyphs of an embedded program a font's strings select.
//!
//! Simple fonts address glyphs by single-byte code through rules that
//! depend on the program kind, the encoding and the symbolic flag (PDF
//! 32000-1, 9.6.6). Rather than pick the one rule a given viewer applies,
//! every rule is tried and the union kept: a superset of the glyphs any
//! viewer can reach, which is what subsetting with retained glyph IDs
//! needs. Type 0 fonts go through their CMap to CIDs and then the
//! CIDToGIDMap or the CFF charset.

use std::collections::{BTreeSet, HashMap};

use read_fonts::TableProvider;
use read_fonts::ps::cff::CffFontRef;
use read_fonts::tables::cmap::{CmapSubtable, PlatformId};
use read_fonts::types::{GlyphId, GlyphId16, Tag};

use super::cmap::CMap;
use super::{encodings, glyphnames};

/// Base encoding of a simple font, when it is one of the predefined three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    Standard,
    WinAnsi,
    MacRoman,
}

impl Base {
    pub fn from_name(name: &[u8]) -> Option<Base> {
        match name {
            b"StandardEncoding" => Some(Base::Standard),
            b"WinAnsiEncoding" => Some(Base::WinAnsi),
            b"MacRomanEncoding" => Some(Base::MacRoman),
            _ => None,
        }
    }

    fn table(self) -> &'static [Option<&'static str>; 256] {
        match self {
            Base::Standard => &encodings::STANDARD,
            Base::WinAnsi => &encodings::WIN_ANSI,
            Base::MacRoman => &encodings::MAC_ROMAN,
        }
    }
}

/// How a simple font's codes turn into glyph names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimpleEncoding {
    /// `None` means the program's built-in encoding.
    pub base: Option<Base>,
    pub differences: HashMap<u8, Vec<u8>>,
    pub symbolic: bool,
}

impl SimpleEncoding {
    /// The glyph name a code has under this encoding, if any.
    fn name(&self, code: u8) -> Option<Vec<u8>> {
        if let Some(n) = self.differences.get(&code) {
            return Some(n.clone());
        }
        let base = match self.base {
            Some(b) => b,
            // A non-symbolic font without an encoding uses the standard
            // encoding; a symbolic one uses the program's built-in one.
            None if !self.symbolic => Base::Standard,
            None => return None,
        };
        base.table()[usize::from(code)].map(|n| n.as_bytes().to_vec())
    }
}

/// CID to glyph index for a Type 0 font.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CidToGid {
    Identity,
    /// The `CIDToGIDMap` stream: two bytes per CID.
    Map(Vec<u8>),
}

/// How a font selects glyphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addressing {
    Simple(SimpleEncoding),
    Cid { cmap: CMap, cid_to_gid: CidToGid },
}

/// The program's container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    TrueType,
    Cff,
    OpenType,
}

/// The glyph IDs the strings can reach. Codes no rule resolves are left
/// out (they show nothing in any viewer). `None` when the program does
/// not parse, so the caller keeps it untouched.
pub fn used(
    program: &[u8],
    kind: Kind,
    addressing: &Addressing,
    strings: &BTreeSet<Vec<u8>>,
) -> Option<BTreeSet<u32>> {
    let mut out = BTreeSet::new();
    let count = match (kind, addressing) {
        (Kind::Cff, Addressing::Simple(enc)) => {
            let cff = CffFontRef::new_cff(program, 0, None).ok()?;
            let names = CffNames::new(&cff);
            for code in codes(strings) {
                names.simple(&cff, enc, code, &mut out);
            }
            cff.num_glyphs()
        }
        (Kind::Cff, Addressing::Cid { cmap, cid_to_gid }) => {
            let cff = CffFontRef::new_cff(program, 0, None).ok()?;
            for cid in cids(cmap, strings) {
                out.insert(cid_gid(&cff, cid_to_gid, cid)?);
            }
            cff.num_glyphs()
        }
        (Kind::TrueType | Kind::OpenType, Addressing::Simple(enc)) => {
            let face = read_fonts::FontRef::new(program).ok()?;
            let sfnt = SfntMaps::new(&face);
            let cff = cff_table(&face);
            let names = cff.as_ref().map(CffNames::new);
            for code in codes(strings) {
                sfnt.simple(enc, code, &mut out);
                if let (Some(cff), Some(names)) = (&cff, &names) {
                    names.simple(cff, enc, code, &mut out);
                }
            }
            u32::from(face.maxp().ok()?.num_glyphs())
        }
        (Kind::TrueType | Kind::OpenType, Addressing::Cid { cmap, cid_to_gid }) => {
            let face = read_fonts::FontRef::new(program).ok()?;
            let cff = cff_table(&face);
            // A CIDFontType2 addresses glyphs through `CIDToGIDMap` even
            // when the OpenType program holds CFF outlines; a CIDFontType0
            // goes through the CFF charset. The dictionary's subtype is not
            // known here, so both are kept.
            for cid in cids(cmap, strings) {
                out.insert(plain_cid_gid(cid_to_gid, cid));
                if let Some(cff) = &cff {
                    out.insert(cid_gid(cff, cid_to_gid, cid)?);
                }
            }
            u32::from(face.maxp().ok()?.num_glyphs())
        }
    };
    // Encodings and charsets can name glyphs the program does not have.
    out.retain(|g| *g < count);
    Some(out)
}

/// The CFF program inside an OpenType font, if it has one.
fn cff_table<'a>(face: &read_fonts::FontRef<'a>) -> Option<CffFontRef<'a>> {
    let data = face.table_data(Tag::new(b"CFF "))?;
    CffFontRef::new_cff(data.as_bytes(), 0, None).ok()
}

fn codes(strings: &BTreeSet<Vec<u8>>) -> BTreeSet<u8> {
    strings.iter().flatten().copied().collect()
}

fn cids(cmap: &CMap, strings: &BTreeSet<Vec<u8>>) -> BTreeSet<u32> {
    strings.iter().flat_map(|s| cmap.cids(s)).collect()
}

fn plain_cid_gid(map: &CidToGid, cid: u32) -> u32 {
    match map {
        CidToGid::Identity => cid,
        CidToGid::Map(bytes) => {
            let i = cid as usize * 2;
            bytes
                .get(i..i + 2)
                .map_or(0, |b| u32::from(u16::from_be_bytes([b[0], b[1]])))
        }
    }
}

/// In a CID-keyed CFF the charset maps glyph index to CID; a CFF that is
/// not CID-keyed is addressed by glyph index directly.
fn cid_gid(cff: &CffFontRef<'_>, map: &CidToGid, cid: u32) -> Option<u32> {
    let cid = plain_cid_gid(map, cid);
    if !cff.is_cid() {
        return Some(cid);
    }
    let charset = cff.charset()?;
    Some(
        charset
            .glyph_id(read_fonts::ps::string::Sid::new(u16::try_from(cid).ok()?))
            .map_or(0, |g| g.to_u32()),
    )
}

/// Glyph names of a CFF program, for lookups by name.
struct CffNames {
    by_name: HashMap<Vec<u8>, u32>,
}

impl CffNames {
    fn new(cff: &CffFontRef<'_>) -> CffNames {
        let mut by_name = HashMap::new();
        if let Some(charset) = cff.charset()
            && !cff.is_cid()
        {
            for (gid, sid) in charset.iter() {
                if let Some(name) = cff.string(sid) {
                    by_name.entry(name.to_vec()).or_insert(gid.to_u32());
                }
            }
        }
        CffNames { by_name }
    }

    /// Every glyph a code can reach: through its name under the font's
    /// encoding, and through the program's built-in encoding.
    fn simple(
        &self,
        cff: &CffFontRef<'_>,
        enc: &SimpleEncoding,
        code: u8,
        out: &mut BTreeSet<u32>,
    ) {
        if let Some(name) = enc.name(code)
            && let Some(&gid) = self.by_name.get(&name)
        {
            out.insert(gid);
        }
        if let Some(builtin) = cff.encoding()
            && let Some(gid) = builtin.map(code)
        {
            out.insert(gid.to_u32());
        }
    }
}

/// The `cmap` subtables and `post` names of an sfnt, for code lookups.
struct SfntMaps<'a> {
    symbol: Option<CmapSubtable<'a>>,
    mac: Option<CmapSubtable<'a>>,
    unicode: Option<CmapSubtable<'a>>,
    has_cmap: bool,
    post_names: HashMap<Vec<u8>, u32>,
}

impl<'a> SfntMaps<'a> {
    fn new(face: &read_fonts::FontRef<'a>) -> SfntMaps<'a> {
        let mut maps = SfntMaps {
            symbol: None,
            mac: None,
            unicode: None,
            has_cmap: false,
            post_names: HashMap::new(),
        };
        if let Ok(cmap) = face.cmap() {
            maps.has_cmap = true;
            for record in cmap.encoding_records() {
                let Ok(sub) = record.subtable(cmap.offset_data()) else {
                    continue;
                };
                match (record.platform_id(), record.encoding_id()) {
                    (PlatformId::Windows, 0) => maps.symbol = Some(sub),
                    (PlatformId::Windows, 1 | 10) => {
                        maps.unicode = maps.unicode.take().or(Some(sub))
                    }
                    (PlatformId::Unicode, _) => maps.unicode = maps.unicode.take().or(Some(sub)),
                    (PlatformId::Macintosh, 0) => maps.mac = Some(sub),
                    _ => {}
                }
            }
        }
        if let Ok(post) = face.post() {
            for gid in 0..post.num_names() {
                if let Some(name) = post.glyph_name(GlyphId16::new(gid as u16)) {
                    maps.post_names
                        .entry(name.as_bytes().to_vec())
                        .or_insert(gid as u32);
                }
            }
        }
        maps
    }

    /// Every glyph a code can reach under the TrueType rules: the (3,0)
    /// symbol table with the code in each of its usual ranges, the (1,0)
    /// table with the code, the Unicode table with the code's glyph name
    /// (and with the code itself), the `post` names, and the code as a
    /// glyph index when there is no `cmap`.
    fn simple(&self, enc: &SimpleEncoding, code: u8, out: &mut BTreeSet<u32>) {
        let c = u32::from(code);
        if let Some(sym) = &self.symbol {
            for candidate in [c, 0xF000 | c, 0xF100 | c, 0xF200 | c] {
                insert(out, sym.map_codepoint(candidate));
            }
        }
        if let Some(mac) = &self.mac {
            insert(out, mac.map_codepoint(c));
        }
        if let Some(uni) = &self.unicode {
            insert(out, uni.map_codepoint(c));
            if let Some(u) = enc.name(code).and_then(|n| glyphnames::unicode(&n)) {
                insert(out, uni.map_codepoint(u));
            }
        }
        if let Some(name) = enc.name(code)
            && let Some(&gid) = self.post_names.get(&name)
        {
            out.insert(gid);
        }
        if !self.has_cmap {
            out.insert(c);
        }
    }
}

fn insert(out: &mut BTreeSet<u32>, gid: Option<GlyphId>) {
    if let Some(g) = gid
        && g.to_u32() != 0
    {
        out.insert(g.to_u32());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::sfnt::tests::tiny_cff;

    #[test]
    fn cid_fonts_go_through_cmap_and_map() {
        let cff = tiny_cff();
        let strings = BTreeSet::from([vec![0, 1], vec![0, 0]]);
        let addressing = Addressing::Cid {
            cmap: CMap::identity(),
            cid_to_gid: CidToGid::Identity,
        };
        assert_eq!(
            used(&cff, Kind::Cff, &addressing, &strings),
            Some(BTreeSet::from([0, 1]))
        );
        let addressing = Addressing::Cid {
            cmap: CMap::identity(),
            cid_to_gid: CidToGid::Map(vec![0, 0, 0, 1, 0, 1]),
        };
        let strings = BTreeSet::from([vec![0, 2]]);
        assert_eq!(
            used(&cff, Kind::Cff, &addressing, &strings),
            Some(BTreeSet::from([1]))
        );
    }

    #[test]
    fn simple_cff_by_name_and_builtin_encoding() {
        // The tiny CFF has the standard charset (glyph 1 is "space") and
        // the standard encoding, where code 32 is space.
        let cff = tiny_cff();
        let enc = SimpleEncoding {
            base: Some(Base::WinAnsi),
            differences: HashMap::new(),
            symbolic: false,
        };
        let strings = BTreeSet::from([b" ".to_vec()]);
        assert_eq!(
            used(&cff, Kind::Cff, &Addressing::Simple(enc), &strings),
            Some(BTreeSet::from([1]))
        );
        let mut diffs = HashMap::new();
        diffs.insert(65u8, b"space".to_vec());
        let enc = SimpleEncoding {
            base: None,
            differences: diffs,
            symbolic: true,
        };
        let strings = BTreeSet::from([b"A".to_vec()]);
        assert_eq!(
            used(&cff, Kind::Cff, &Addressing::Simple(enc), &strings),
            Some(BTreeSet::from([1]))
        );
        assert!(
            used(
                b"junk",
                Kind::Cff,
                &Addressing::Simple(SimpleEncoding::default()),
                &strings
            )
            .is_none()
        );
    }

    #[test]
    fn encoding_names_follow_differences_then_base() {
        let mut diffs = HashMap::new();
        diffs.insert(65u8, b"bullet".to_vec());
        let enc = SimpleEncoding {
            base: Some(Base::WinAnsi),
            differences: diffs,
            symbolic: false,
        };
        assert_eq!(enc.name(65), Some(b"bullet".to_vec()));
        assert_eq!(enc.name(66), Some(b"B".to_vec()));
        assert_eq!(enc.name(0x80), Some(b"Euro".to_vec()));
        let builtin = SimpleEncoding {
            base: None,
            differences: HashMap::new(),
            symbolic: true,
        };
        assert_eq!(builtin.name(66), None);
        let standard = SimpleEncoding::default();
        assert_eq!(standard.name(0x27), Some(b"quoteright".to_vec()));
    }
}
