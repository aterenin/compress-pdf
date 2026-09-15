//! Type 1 to CFF: parse the program, translate every charstring, and
//! write the result with the same names, encoding and private values.
//! One glyph that fails to translate fails the conversion, so the caller
//! keeps the original.

use super::{cff, charstring, type1};

pub fn type1_to_cff(program: &[u8]) -> Option<Vec<u8>> {
    let font = type1::parse(program)?;
    let mut glyphs = Vec::with_capacity(font.charstrings.len() + 1);
    let mut notdef = None;
    for (name, code) in &font.charstrings {
        let glyph = cff::Glyph {
            name: name.clone(),
            charstring: charstring::to_type2(code, &font.subrs)?.charstring,
        };
        if name == b".notdef" && notdef.is_none() {
            notdef = Some(glyph);
        } else {
            glyphs.push(glyph);
        }
    }
    glyphs.insert(
        0,
        notdef.unwrap_or(cff::Glyph {
            name: b".notdef".to_vec(),
            charstring: vec![14],
        }),
    );
    let glyphs = cff::sort_glyphs(glyphs, font.encoding.as_ref());
    cff::write(&cff::Font {
        name: &font.font_name,
        font_matrix: font.font_matrix,
        font_bbox: font.font_bbox,
        glyphs: &glyphs,
        encoding: font.encoding.as_ref(),
        private: &font.private,
    })
}

#[cfg(test)]
mod tests {
    use read_fonts::ps::cff::CffFontRef;
    use read_fonts::types::GlyphId;

    use super::*;
    use crate::font::type1::tests::tiny_type1;

    /// Collects an outline as a list of commands with rounded coordinates,
    /// the same way from both parsers so the two can be compared.
    #[derive(Default, Debug, PartialEq)]
    struct Path(Vec<String>);

    impl hayro_font::OutlineBuilder for Path {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("M {x:.0} {y:.0}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("L {x:.0} {y:.0}"));
        }
        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            self.0.push(format!("Q {x1:.0} {y1:.0} {x:.0} {y:.0}"));
        }
        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            self.0
                .push(format!("C {x1:.0} {y1:.0} {x2:.0} {y2:.0} {x:.0} {y:.0}"));
        }
        fn close(&mut self) {
            self.0.push("Z".into());
        }
    }

    impl read_fonts::model::pen::OutlinePen for Path {
        fn move_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("M {x:.0} {y:.0}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0.push(format!("L {x:.0} {y:.0}"));
        }
        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            self.0.push(format!("Q {x1:.0} {y1:.0} {x:.0} {y:.0}"));
        }
        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            self.0
                .push(format!("C {x1:.0} {y1:.0} {x2:.0} {y2:.0} {x:.0} {y:.0}"));
        }
        fn close(&mut self) {
            self.0.push("Z".into());
        }
    }

    #[test]
    fn converted_font_matches_the_type1_outline() {
        let program = tiny_type1(false);
        let cff_bytes = type1_to_cff(&program).unwrap();
        let cff = CffFontRef::new_cff(&cff_bytes, 0, None).unwrap();
        assert_eq!(cff.num_glyphs(), 2);
        let subfont = cff.subfont(0, &[]).unwrap();
        let mut from_cff = Path::default();
        cff.draw(&subfont, GlyphId::new(1), &[], None, &mut from_cff)
            .unwrap();
        let t1 = hayro_font::type1::Table::parse(&program).unwrap();
        let mut from_t1 = Path::default();
        t1.outline("A", &mut from_t1).unwrap();
        assert_eq!(from_cff, from_t1);
        assert!(
            from_cff.0.contains(&"L 450 400".to_string()),
            "{from_cff:?}"
        );
        // The built-in encoding survives: code 65 selects A.
        assert_eq!(cff.encoding().unwrap().map(65).unwrap().to_u32(), 1);
        assert!(type1_to_cff(b"not a type 1 font").is_none());
    }
}
