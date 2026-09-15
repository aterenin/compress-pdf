//! Stage 3: fonts.
//!
//! In order: unembed the 14 standard fonts when the font's encoding is
//! trustworthy without the program; convert Type 1 programs to CFF; merge
//! duplicate embeddings of the same font; subset embedded programs to the
//! glyphs the content streams use. Each step is gated by its `Config`
//! flag, and every embedded program gets one report row.

use anyhow::Result;
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::config::Config;
use crate::font::std14;
use crate::pipeline::{Context, Stage};
use crate::report::FontRow;

pub struct OptimizeFonts;

impl Stage for OptimizeFonts {
    fn name(&self) -> &'static str {
        "fonts"
    }

    fn enabled(&self, config: &Config) -> bool {
        config.subset_fonts
            || config.merge_fonts
            || config.remove_standard_fonts
            || config.convert_to_cff
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        for font in collect_fonts(doc) {
            let mut row = FontRow {
                object: font.id,
                name: String::from_utf8_lossy(&font.base_font).into_owned(),
                program: font.program.kind.to_string(),
                bytes_in: font.program.bytes,
                action: "kept".into(),
                bytes_out: font.program.bytes,
            };
            if ctx.config.remove_standard_fonts
                && let Some(canonical) = unembed_standard(doc, &font)
            {
                row.action = format!("unembedded as {canonical}");
                row.bytes_out = 0;
            }
            ctx.report.fonts.push(row);
        }
        Ok(())
    }
}

/// A font dictionary with an embedded program.
struct Font {
    /// The font dictionary; for Type 0 fonts the descendant CIDFont, which
    /// is where the descriptor and program hang.
    id: ObjectId,
    subtype: Vec<u8>,
    base_font: Vec<u8>,
    descriptor: ObjectId,
    program: Program,
}

#[derive(Clone, Copy)]
struct Program {
    /// The descriptor key holding it: FontFile, FontFile2 or FontFile3.
    key: &'static str,
    kind: &'static str,
    bytes: usize,
}

/// Every font dictionary in the file that carries an embedded program,
/// in object-number order so the report is stable.
fn collect_fonts(doc: &Document) -> Vec<Font> {
    let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    ids.sort_unstable();
    ids.into_iter()
        .filter_map(|id| {
            let dict = doc.get_dictionary(id).ok()?;
            if dict.get(b"Type").ok()?.as_name().ok()? != b"Font" {
                return None;
            }
            let descriptor = dict.get(b"FontDescriptor").ok()?.as_reference().ok()?;
            let program = program_of(doc, doc.get_dictionary(descriptor).ok()?)?;
            Some(Font {
                id,
                subtype: dict.get(b"Subtype").ok()?.as_name().ok()?.to_vec(),
                base_font: dict
                    .get(b"BaseFont")
                    .ok()
                    .and_then(|n| n.as_name().ok())
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default(),
                descriptor,
                program,
            })
        })
        .collect()
}

fn program_of(doc: &Document, descriptor: &Dictionary) -> Option<Program> {
    for key in ["FontFile", "FontFile2", "FontFile3"] {
        let Ok(Object::Reference(id)) = descriptor.get(key.as_bytes()) else {
            continue;
        };
        let Ok(Object::Stream(stream)) = doc.get_object(*id) else {
            continue;
        };
        let kind = match key {
            "FontFile" => "Type1",
            "FontFile2" => "TrueType",
            _ => match stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|s| s.as_name().ok())
            {
                Some(b"Type1C") => "CFF",
                Some(b"CIDFontType0C") => "CIDFontType0C",
                Some(b"OpenType") => "OpenType",
                _ => "FontFile3",
            },
        };
        return Some(Program {
            key,
            kind,
            bytes: stream.content.len(),
        });
    }
    None
}

/// Drop the program of a simple font that is one of the standard 14 and
/// whose encoding stands on its own; rename the font to the canonical
/// name so viewers pick the right substitute. The program stream becomes
/// unreferenced and is collected by the structure stage.
fn unembed_standard(doc: &mut Document, font: &Font) -> Option<&'static str> {
    if !matches!(font.subtype.as_slice(), b"Type1" | b"TrueType" | b"MMType1") {
        return None;
    }
    let dict = doc.get_dictionary(font.id).ok()?;
    let descriptor = doc.get_dictionary(font.descriptor).ok()?;
    let canonical = std14::can_unembed(doc, dict, descriptor)?;
    let name = Object::Name(canonical.as_bytes().to_vec());
    let descriptor = doc.get_dictionary_mut(font.descriptor).ok()?;
    descriptor.remove(font.program.key.as_bytes());
    descriptor.set("FontName", name.clone());
    doc.get_dictionary_mut(font.id).ok()?.set("BaseFont", name);
    Some(canonical)
}

#[cfg(test)]
mod tests {
    use lopdf::{Stream, dictionary};

    use super::*;
    use crate::config::Preset;
    use crate::report::Report;
    use crate::stages::usage::ImageUsage;

    fn doc_with_font(base_font: &str, flags: i64) -> (Document, ObjectId, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let program = doc.add_object(Stream::new(dictionary! {}, vec![0u8; 100]));
        let descriptor = doc.add_object(dictionary! {
            "Type" => "FontDescriptor", "FontName" => base_font, "Flags" => flags,
            "FontFile2" => program,
        });
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => base_font,
            "Encoding" => "WinAnsiEncoding", "FontDescriptor" => descriptor,
        });
        doc.trailer.set("Root", font);
        (doc, font, descriptor)
    }

    fn run(doc: &mut Document, preset: Preset) -> Report {
        let config = Config::preset(preset);
        let mut report = Report::new(0);
        let mut ctx = Context {
            config: &config,
            report: &mut report,
            usage: ImageUsage::default(),
        };
        OptimizeFonts.run(doc, &mut ctx).unwrap();
        report
    }

    #[test]
    fn standard_font_is_unembedded_under_less() {
        let (mut doc, font, descriptor) = doc_with_font("ABCDEF+Arial-BoldMT", 32);
        let report = run(&mut doc, Preset::Less);
        assert_eq!(report.fonts.len(), 1);
        assert_eq!(report.fonts[0].action, "unembedded as Helvetica-Bold");
        assert_eq!(report.fonts[0].bytes_out, 0);
        let d = doc.get_dictionary(descriptor).unwrap();
        assert!(!d.has(b"FontFile2"));
        assert_eq!(
            d.get(b"FontName").unwrap().as_name().unwrap(),
            b"Helvetica-Bold"
        );
        let f = doc.get_dictionary(font).unwrap();
        assert_eq!(
            f.get(b"BaseFont").unwrap().as_name().unwrap(),
            b"Helvetica-Bold"
        );
    }

    #[test]
    fn standard_preset_keeps_programs_and_other_fonts_stay() {
        let (mut doc, _, descriptor) = doc_with_font("Arial", 32);
        let report = run(&mut doc, Preset::Standard);
        assert_eq!(report.fonts[0].action, "kept");
        assert!(doc.get_dictionary(descriptor).unwrap().has(b"FontFile2"));
        let (mut doc, _, descriptor) = doc_with_font("ArialNarrow", 32);
        let report = run(&mut doc, Preset::Less);
        assert_eq!(report.fonts[0].action, "kept");
        assert!(doc.get_dictionary(descriptor).unwrap().has(b"FontFile2"));
    }
}
