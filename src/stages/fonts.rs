//! Stage 3: fonts.
//!
//! In order: unembed the 14 standard fonts when the font's encoding is
//! trustworthy without the program; subset embedded programs to the glyphs
//! the content streams use, with glyph IDs retained. Each step is gated by
//! its `Config` flag, and every embedded program gets one report row.
//!
//! A program is subset only when every font dictionary that shares it was
//! seen by the usage walk and could be analyzed; a font reachable from a
//! place the walk does not cover (form field default appearances, Type 3
//! glyph procedures) makes its program untouchable.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;

use anyhow::Result;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::config::Config;
use crate::font::cmap::CMap;
use crate::font::glyphs::{self, Addressing, Base, CidToGid, Kind, SimpleEncoding};
use crate::font::{std14, subset};
use crate::pipeline::{Context, Stage};
use crate::report::FontRow;
use crate::stages::usage::TextUsage;

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
        let mut rows: HashMap<ObjectId, FontRow> = HashMap::new();
        for font in collect_fonts(doc) {
            let row = rows.entry(font.program.id).or_insert_with(|| font.row());
            if ctx.config.remove_standard_fonts
                && let Some(canonical) = unembed_standard(doc, &font)
            {
                row.action = format!("unembedded as {canonical}");
                row.bytes_out = 0;
            }
        }
        if ctx.config.subset_fonts {
            let seen = Seen {
                untouchable: untouchable_fonts(doc),
                usage: &ctx.usage.fonts,
            };
            for (program, fonts) in by_program(collect_fonts(doc)) {
                let outcome = subset_program(doc, program, &fonts, &seen);
                if let Some(row) = rows.get_mut(&program.id) {
                    row.action = outcome.action;
                    row.bytes_out = outcome.bytes_out.unwrap_or(row.bytes_in);
                }
            }
        }
        let mut rows: Vec<FontRow> = rows.into_values().collect();
        rows.sort_by_key(|r| r.object);
        ctx.report.fonts.extend(rows);
        Ok(())
    }
}

/// A font dictionary with an embedded program.
#[derive(Clone)]
struct Font {
    /// The font dictionary a content stream selects (the Type 0 font for
    /// composite fonts).
    id: ObjectId,
    subtype: Vec<u8>,
    base_font: Vec<u8>,
    /// The dictionary holding the descriptor: the font itself, or the
    /// descendant CIDFont.
    holder: ObjectId,
    descriptor: ObjectId,
    program: Program,
}

#[derive(Clone, Copy)]
struct Program {
    id: ObjectId,
    /// The descriptor key holding it: FontFile, FontFile2 or FontFile3.
    key: &'static str,
    kind: &'static str,
    bytes: usize,
}

impl Font {
    fn row(&self) -> FontRow {
        FontRow {
            object: self.program.id,
            name: String::from_utf8_lossy(&self.base_font).into_owned(),
            program: self.program.kind.to_string(),
            bytes_in: self.program.bytes,
            action: "kept".into(),
            bytes_out: self.program.bytes,
        }
    }
}

/// Every font dictionary in the file that carries an embedded program,
/// in object-number order so the report is stable. CIDFont dictionaries
/// are reached through their Type 0 parent, not on their own.
fn collect_fonts(doc: &Document) -> Vec<Font> {
    let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    ids.sort_unstable();
    ids.into_iter()
        .filter_map(|id| {
            let dict = doc.get_dictionary(id).ok()?;
            if dict.get(b"Type").ok()?.as_name().ok()? != b"Font" {
                return None;
            }
            let subtype = dict.get(b"Subtype").ok()?.as_name().ok()?.to_vec();
            let holder = match subtype.as_slice() {
                b"Type0" => descendant(doc, dict)?,
                b"CIDFontType0" | b"CIDFontType2" | b"Type3" => return None,
                _ => id,
            };
            let holder_dict = doc.get_dictionary(holder).ok()?;
            let descriptor = holder_dict
                .get(b"FontDescriptor")
                .ok()?
                .as_reference()
                .ok()?;
            let program = program_of(doc, doc.get_dictionary(descriptor).ok()?)?;
            Some(Font {
                id,
                subtype,
                base_font: name_of(dict, b"BaseFont"),
                holder,
                descriptor,
                program,
            })
        })
        .collect()
}

fn descendant(doc: &Document, type0: &Dictionary) -> Option<ObjectId> {
    let kids = type0.get(b"DescendantFonts").ok()?;
    let kids = doc.dereference(kids).map(|(_, o)| o).unwrap_or(kids);
    kids.as_array().ok()?.first()?.as_reference().ok()
}

fn name_of(dict: &Dictionary, key: &[u8]) -> Vec<u8> {
    dict.get(key)
        .ok()
        .and_then(|n| n.as_name().ok())
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
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
            id: *id,
            key,
            kind,
            bytes: stream.content.len(),
        });
    }
    None
}

fn by_program(fonts: Vec<Font>) -> Vec<(Program, Vec<Font>)> {
    let mut groups: Vec<(Program, Vec<Font>)> = Vec::new();
    for font in fonts {
        match groups.iter_mut().find(|(p, _)| p.id == font.program.id) {
            Some((_, list)) => list.push(font),
            None => groups.push((font.program, vec![font])),
        }
    }
    groups
}

// ------------------------------------------------------------- unembed

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

// -------------------------------------------------------------- subset

struct Outcome {
    action: String,
    bytes_out: Option<usize>,
}

impl Outcome {
    fn kept(reason: impl Into<String>) -> Outcome {
        Outcome {
            action: format!("kept: {}", reason.into()),
            bytes_out: None,
        }
    }
}

/// Fonts the usage walk cannot see the use of: those a default appearance
/// string (`/DA`, on the AcroForm or any field, widget or annotation)
/// names in the AcroForm's default resources, and those in the resources
/// of Type 3 fonts (used by glyph procedures).
fn untouchable_fonts(doc: &Document) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let da_names = default_appearance_fonts(doc);
    if let Some(dr_fonts) = doc
        .catalog()
        .ok()
        .and_then(|c| deref(doc, c.get(b"AcroForm").ok()?).as_dict().ok())
        .and_then(|acro| deref(doc, acro.get(b"DR").ok()?).as_dict().ok())
        .and_then(|dr| deref(doc, dr.get(b"Font").ok()?).as_dict().ok())
    {
        for (name, value) in dr_fonts.iter() {
            if da_names.contains(name)
                && let Ok(id) = value.as_reference()
            {
                out.insert(id);
            }
        }
    }
    out.extend(type3_resource_fonts(doc));
    out
}

fn type3_resource_fonts(doc: &Document) -> Vec<ObjectId> {
    let mut out = Vec::new();
    for obj in doc.objects.values() {
        let Object::Dictionary(d) = obj else {
            continue;
        };
        if d.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Type3")
            && let Some(res) = d
                .get(b"Resources")
                .ok()
                .and_then(|r| deref(doc, r).as_dict().ok())
            && let Some(fonts) = res
                .get(b"Font")
                .ok()
                .and_then(|f| deref(doc, f).as_dict().ok())
        {
            out.extend(fonts.iter().filter_map(|(_, v)| v.as_reference().ok()));
        }
    }
    out
}

/// Resource names selected with `Tf` in any default appearance string.
fn default_appearance_fonts(doc: &Document) -> HashSet<Vec<u8>> {
    let mut names = HashSet::new();
    let mut strings: Vec<&[u8]> = Vec::new();
    if let Ok(catalog) = doc.catalog()
        && let Some(acro) = catalog
            .get(b"AcroForm")
            .ok()
            .and_then(|a| deref(doc, a).as_dict().ok())
        && let Ok(Object::String(da, _)) = acro.get(b"DA")
    {
        strings.push(da);
    }
    for obj in doc.objects.values() {
        if let Object::Dictionary(d) = obj
            && let Ok(Object::String(da, _)) = d.get(b"DA")
        {
            strings.push(da);
        }
    }
    for da in strings {
        let tokens: Vec<&[u8]> = da.split(|b| b.is_ascii_whitespace()).collect();
        for pair in tokens.windows(3) {
            if pair[2] == b"Tf"
                && let Some(name) = pair[0].strip_prefix(b"/")
            {
                names.insert(name.to_vec());
            }
        }
    }
    names
}

/// What the usage walk learned, and what it could not see.
struct Seen<'a> {
    untouchable: HashSet<ObjectId>,
    usage: &'a HashMap<ObjectId, TextUsage>,
}

/// A program's bytes as HarfBuzz and the glyph analysis need them.
struct Loaded {
    kind: Kind,
    data: Vec<u8>,
    /// Size of the stream as stored, the never-grow reference.
    stored: usize,
}

fn subset_program(
    doc: &mut Document,
    program: Program,
    fonts: &[Font],
    seen: &Seen<'_>,
) -> Outcome {
    let loaded = match load_program(doc, program, fonts, seen) {
        Ok(loaded) => loaded,
        Err(outcome) => return outcome,
    };
    let (glyphs, simple) = match used_glyphs(doc, fonts, &loaded, seen.usage) {
        Ok(found) => found,
        Err(outcome) => return outcome,
    };
    let hb_kind = match loaded.kind {
        Kind::Cff => subset::Program::Cff,
        _ => subset::Program::Sfnt,
    };
    let Some(reduced) = subset::subset(&loaded.data, hb_kind, &glyphs, simple) else {
        return Outcome::kept("subsetter failed");
    };
    let compressed = deflate(&reduced);
    if compressed.len() >= loaded.stored {
        return Outcome::kept("source is smaller");
    }
    let bytes_out = compressed.len();
    write_program(doc, program, &reduced, compressed);
    tag_fonts(doc, fonts, &glyphs);
    Outcome {
        action: format!("subset to {} glyphs", glyphs.len() + 1),
        bytes_out: Some(bytes_out),
    }
}

fn load_program(
    doc: &Document,
    program: Program,
    fonts: &[Font],
    seen: &Seen<'_>,
) -> Result<Loaded, Outcome> {
    let kind = match program.kind {
        "TrueType" => Kind::TrueType,
        "CFF" | "CIDFontType0C" => Kind::Cff,
        "OpenType" => Kind::OpenType,
        other => return Err(Outcome::kept(format!("{other} programs are not subset"))),
    };
    if fonts.iter().any(|f| seen.untouchable.contains(&f.id)) {
        return Err(Outcome::kept("used by form fields or Type 3 glyphs"));
    }
    let Ok(Object::Stream(stream)) = doc.get_object(program.id) else {
        return Err(Outcome::kept("program is not a stream"));
    };
    let Ok(data) = stream.decompressed_content() else {
        return Err(Outcome::kept("program does not decompress"));
    };
    Ok(Loaded {
        kind,
        data,
        stored: stream.content.len(),
    })
}

/// The union of the glyphs every font sharing the program uses, and
/// whether any of them addresses glyphs by name (so names must survive).
fn used_glyphs(
    doc: &Document,
    fonts: &[Font],
    loaded: &Loaded,
    usage: &HashMap<ObjectId, TextUsage>,
) -> Result<(BTreeSet<u32>, bool), Outcome> {
    let mut glyphs = BTreeSet::new();
    let mut simple = false;
    for font in fonts {
        let Some(text) = usage.get(&font.id) else {
            return Err(Outcome::kept("no text seen for it"));
        };
        let Some(addressing) = addressing(doc, font) else {
            return Err(Outcome::kept("encoding or CMap not understood"));
        };
        simple |= matches!(addressing, Addressing::Simple(_));
        match glyphs::used(&loaded.data, loaded.kind, &addressing, &text.strings) {
            Some(used) => glyphs.extend(used),
            None => return Err(Outcome::kept("program does not parse")),
        }
    }
    Ok((glyphs, simple))
}

/// How a font dictionary selects glyphs, from its encoding entries.
fn addressing(doc: &Document, font: &Font) -> Option<Addressing> {
    let dict = doc.get_dictionary(font.id).ok()?;
    if font.subtype != b"Type0" {
        let descriptor = doc.get_dictionary(font.descriptor).ok()?;
        let symbolic = descriptor
            .get(b"Flags")
            .and_then(Object::as_i64)
            .is_ok_and(|f| f & 4 != 0);
        return Some(Addressing::Simple(simple_encoding(doc, dict, symbolic)?));
    }
    let cmap = match dict.get(b"Encoding").ok().map(|e| deref(doc, e))? {
        Object::Name(n) => CMap::predefined(n)?,
        Object::Stream(s) => CMap::parse(&s.decompressed_content().ok()?)?,
        _ => return None,
    };
    let cid_font = doc.get_dictionary(font.holder).ok()?;
    let cid_to_gid = match cid_font.get(b"CIDToGIDMap").ok().map(|m| deref(doc, m)) {
        None | Some(Object::Name(_)) => CidToGid::Identity,
        Some(Object::Stream(s)) => CidToGid::Map(s.decompressed_content().ok()?),
        Some(_) => return None,
    };
    Some(Addressing::Cid { cmap, cid_to_gid })
}

fn simple_encoding(doc: &Document, dict: &Dictionary, symbolic: bool) -> Option<SimpleEncoding> {
    let mut enc = SimpleEncoding {
        base: None,
        differences: HashMap::new(),
        symbolic,
    };
    match dict.get(b"Encoding").ok().map(|e| deref(doc, e)) {
        None => {}
        Some(Object::Name(n)) => enc.base = Some(Base::from_name(n)?),
        Some(Object::Dictionary(d)) => {
            if let Ok(base) = d.get(b"BaseEncoding") {
                enc.base = Some(Base::from_name(base.as_name().ok()?)?);
            }
            if let Ok(diffs) = d.get(b"Differences") {
                enc.differences = differences(deref(doc, diffs).as_array().ok()?)?;
            }
        }
        Some(_) => return None,
    }
    Some(enc)
}

fn differences(items: &[Object]) -> Option<HashMap<u8, Vec<u8>>> {
    let mut out = HashMap::new();
    let mut code = 0u32;
    for item in items {
        match item {
            Object::Integer(i) => code = u32::try_from(*i).ok()?,
            Object::Name(n) => {
                out.insert(u8::try_from(code).ok()?, n.clone());
                code += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(data).ok();
    z.finish().unwrap_or_default()
}

/// Replace the program stream's bytes, keeping its dictionary apart from
/// the filter and the length of the unfiltered program.
fn write_program(doc: &mut Document, program: Program, raw: &[u8], compressed: Vec<u8>) {
    let Ok(Object::Stream(stream)) = doc.get_object_mut(program.id) else {
        return;
    };
    let mut dict = stream.dict.clone();
    dict.remove(b"DecodeParms");
    dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
    if program.key == "FontFile2" {
        dict.set("Length1", raw.len() as i64);
    }
    *stream = Stream::new(dict, compressed);
}

/// A subset font carries a six-letter tag before its name. Fonts that
/// already have one keep it; the others get one derived from the glyph
/// set, so the same input yields the same tag.
fn tag_fonts(doc: &mut Document, fonts: &[Font], glyphs: &BTreeSet<u32>) {
    let tag = subset_tag(glyphs);
    for font in fonts {
        let mut targets = vec![(font.id, "BaseFont"), (font.descriptor, "FontName")];
        if font.holder != font.id {
            targets.push((font.holder, "BaseFont"));
        }
        for (id, key) in targets {
            let Ok(dict) = doc.get_dictionary_mut(id) else {
                continue;
            };
            let Ok(Object::Name(name)) = dict.get(key.as_bytes()) else {
                continue;
            };
            if name.len() > 7 && name[6] == b'+' {
                continue;
            }
            let mut tagged = tag.to_vec();
            tagged.push(b'+');
            tagged.extend_from_slice(name);
            dict.set(key, Object::Name(tagged));
        }
    }
}

fn subset_tag(glyphs: &BTreeSet<u32>) -> [u8; 6] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for g in glyphs {
        h ^= u64::from(*g);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut tag = [0u8; 6];
    for t in &mut tag {
        *t = b'A' + (h % 26) as u8;
        h /= 26;
    }
    tag
}

fn deref<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    doc.dereference(obj).map(|(_, o)| o).unwrap_or(obj)
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;
    use crate::config::Preset;
    use crate::font::sfnt::tests::tiny_cff;
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

    fn run(doc: &mut Document, preset: Preset, usage: ImageUsage) -> Report {
        let config = Config::preset(preset);
        let mut report = Report::new(0);
        let mut ctx = Context {
            config: &config,
            report: &mut report,
            usage,
        };
        OptimizeFonts.run(doc, &mut ctx).unwrap();
        report
    }

    #[test]
    fn standard_font_is_unembedded_under_less() {
        let (mut doc, font, descriptor) = doc_with_font("ABCDEF+Arial-BoldMT", 32);
        let report = run(&mut doc, Preset::Less, ImageUsage::default());
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
    fn unseen_fonts_and_other_fonts_stay() {
        let (mut doc, _, descriptor) = doc_with_font("ArialNarrow", 32);
        let report = run(&mut doc, Preset::Standard, ImageUsage::default());
        assert_eq!(report.fonts[0].action, "kept: no text seen for it");
        assert!(doc.get_dictionary(descriptor).unwrap().has(b"FontFile2"));
    }

    #[test]
    fn cff_program_is_subset_and_tagged() {
        let mut doc = Document::with_version("1.5");
        let cff = tiny_cff();
        // Pad the stream so that the subset can be smaller than the source.
        let mut padded = cff.clone();
        padded.extend(std::iter::repeat_n(0u8, 400));
        let program = doc.add_object(Stream::new(dictionary! { "Subtype" => "Type1C" }, padded));
        let descriptor = doc.add_object(dictionary! {
            "Type" => "FontDescriptor", "FontName" => "Tiny", "Flags" => 32, "FontFile3" => program,
        });
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Tiny",
            "Encoding" => "WinAnsiEncoding", "FontDescriptor" => descriptor,
        });
        doc.trailer.set("Root", font);
        let mut usage = ImageUsage::default();
        usage
            .fonts
            .entry(font)
            .or_default()
            .strings
            .insert(b" ".to_vec());
        let report = run(&mut doc, Preset::Standard, usage);
        assert_eq!(report.fonts[0].action, "subset to 2 glyphs");
        assert!(report.fonts[0].bytes_out < report.fonts[0].bytes_in);
        let name = doc
            .get_dictionary(font)
            .unwrap()
            .get(b"BaseFont")
            .unwrap()
            .as_name()
            .unwrap()
            .to_vec();
        assert_eq!(name.len(), 11);
        assert_eq!(name[6], b'+');
        let stream = doc.get_object(program).unwrap().as_stream().unwrap();
        assert_eq!(
            stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"FlateDecode"
        );
        let out = stream.decompressed_content().unwrap();
        assert!(read_fonts::ps::cff::CffFontRef::new_cff(&out, 0, None).is_ok());
    }

    #[test]
    fn form_field_fonts_are_untouchable() {
        let (mut doc, font, _) = doc_with_font("ArialNarrow", 32);
        let field = doc.add_object(dictionary! { "T" => Object::string_literal("name"),
        "DA" => Object::string_literal("/F1 12 Tf 0 g") });
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! {
                "Fields" => vec![field.into()],
                "DR" => dictionary! { "Font" => dictionary! { "F1" => font, "F2" => font } },
            },
        });
        doc.trailer.set("Root", catalog);
        let mut usage = ImageUsage::default();
        usage
            .fonts
            .entry(font)
            .or_default()
            .strings
            .insert(b"A".to_vec());
        let report = run(&mut doc, Preset::Standard, usage);
        assert_eq!(
            report.fonts[0].action,
            "kept: used by form fields or Type 3 glyphs"
        );
    }

    #[test]
    fn default_resource_fonts_no_appearance_names_are_fair_game() {
        let (mut doc, font, _) = doc_with_font("ArialNarrow", 32);
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "AcroForm" => dictionary! { "DA" => Object::string_literal("/Helv 0 Tf 0 g"),
                "DR" => dictionary! { "Font" => dictionary! { "F1" => font } } },
        });
        doc.trailer.set("Root", catalog);
        let mut usage = ImageUsage::default();
        usage
            .fonts
            .entry(font)
            .or_default()
            .strings
            .insert(b"A".to_vec());
        let report = run(&mut doc, Preset::Standard, usage);
        // The program is junk, so the subsetter cannot parse it; what
        // matters is that the AcroForm rule no longer stops it.
        assert_eq!(report.fonts[0].action, "kept: program does not parse");
    }
}
