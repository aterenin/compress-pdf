//! Font programs, independent of the pipeline: recognizing the standard
//! 14, CMaps for Type 0 fonts, reading Type 1 programs and translating
//! their charstrings, writing CFF, wrapping bare CFF for OpenType
//! consumers, and subsetting through HarfBuzz with glyph IDs retained.
//! Nothing here reads `Config` or touches a `Document` beyond resolving
//! references.

pub mod cff;
pub mod charstring;
pub mod cmap;
pub mod convert;
pub mod encodings;
pub mod glyphnames;
pub mod glyphs;
pub mod sfnt;
pub mod std14;
pub mod subset;
pub mod type1;
