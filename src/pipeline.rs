//! Stage orchestration.
//!
//! A stage is a black box with one contract: it receives the whole document
//! plus shared context, mutates the document in place, and records what it did
//! in the report. Stages do not call each other. Anything one stage needs from
//! another travels through [`Context`] (today: the image usage analysis).

use std::time::Instant;

use anyhow::{Context as _, Result};
use lopdf::{Document, Object, StringFormat};

use crate::config::Config;
use crate::report::{Report, StageSummary};
use crate::stages::{self, usage::ImageUsage};

pub struct Context<'a> {
    pub config: &'a Config,
    pub report: &'a mut Report,
    /// Filled by the usage stage, read by the image stage.
    #[allow(dead_code)] // until stages::images reads it
    pub usage: ImageUsage,
}

pub trait Stage {
    fn name(&self) -> &'static str;

    /// Whether the stage should run at all under this configuration. Skipped
    /// stages still appear in the report so a reader can see they were off.
    fn enabled(&self, _config: &Config) -> bool {
        true
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()>;
}

/// Fixed order. Analysis first, then the lossy image work, then fonts, then
/// the cheap structural passes that clean up whatever the earlier stages left
/// behind.
pub fn default_stages() -> Vec<Box<dyn Stage>> {
    vec![
        Box::new(stages::usage::AnalyzeUsage),
        Box::new(stages::images::RecompressImages),
        Box::new(stages::fonts::OptimizeFonts),
        Box::new(stages::strip::StripDocument),
        Box::new(stages::structure::CleanStructure),
    ]
}

/// Error message for encrypted input; stable because callers match on it.
pub const ENCRYPTED_INPUT: &str = "encrypted input is not supported";
pub const DAMAGED_PAGE_TREE: &str = "page tree refers to objects the parser could not load";
pub const DAMAGED_RESOURCES: &str =
    "page content or resources refer to objects the parser could not load";

pub fn run(doc: &mut Document, config: &Config, report: &mut Report) -> Result<()> {
    // Out of scope for v1 (CLAUDE.md). Refusing is safer than writing a
    // file that claims to be encrypted but is not, or vice versa. lopdf
    // decrypts on load and drops the trailer entry, hence the second test.
    if doc.trailer.has(b"Encrypt") || doc.was_encrypted() {
        anyhow::bail!(ENCRYPTED_INPUT);
    }
    if !page_tree_is_intact(doc) {
        // A kid that did not load is a page the parser cannot see; writing
        // the file would drop it silently while the page count still adds
        // up. Repair is out of scope for v1.
        anyhow::bail!(DAMAGED_PAGE_TREE);
    }
    if !resources_are_intact(doc) {
        // Likewise a content stream, font or image the parser could not
        // load: the page would lose it while still "verifying". Viewers
        // that rebuild the cross-reference table recover such files.
        anyhow::bail!(DAMAGED_RESOURCES);
    }
    normalize_filters(doc);
    hex_binary_strings(doc);
    let mut ctx = Context {
        config,
        report,
        usage: ImageUsage::default(),
    };

    let mut before = serialized_len(doc)?;
    for stage in default_stages() {
        let name = stage.name();
        if !stage.enabled(config) {
            tracing::info!(stage = name, "disabled by configuration");
            continue;
        }
        let started = Instant::now();
        stage
            .run(doc, &mut ctx)
            .with_context(|| format!("stage `{name}` failed"))?;
        let after = serialized_len(doc)?;
        ctx.report.stages.push(StageSummary {
            name,
            bytes_before: before,
            bytes_after: after,
            elapsed: started.elapsed(),
        });
        tracing::info!(stage = name, before, after, "done");
        before = after;
    }
    Ok(())
}

/// Size the document would have on disk right now. Serializing after every
/// stage is wasteful but it is what makes the per-stage report truthful; if it
/// ever matters, gate it behind a verbosity flag.
fn serialized_len(doc: &mut Document) -> Result<usize> {
    let mut buf = Vec::new();
    doc.save_modern(&mut buf)
        .context("measuring document size")?;
    Ok(buf.len())
}

/// Final bytes for the output file, honoring the never-grow rule at file
/// level: the smaller of lopdf's two writers is used, and if neither is
/// strictly smaller than the input, the input bytes are returned unchanged
/// and the report says so.
/// Every reference from a `Contents` entry, and from the entries of every
/// `Resources` category dictionary, resolves to a loaded object, and no
/// content stream came out of the parser empty without a `Length` (a
/// stream without one is read as empty rather than up to `endstream`).
fn resources_are_intact(doc: &Document) -> bool {
    doc.objects.values().all(|obj| {
        let dict = match obj {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &s.dict,
            _ => return true,
        };
        dict.get(b"Contents")
            .ok()
            .is_none_or(|c| references_resolve(doc, c) && content_streams_loaded(doc, c))
            && dict
                .get(b"Resources")
                .ok()
                .is_none_or(|r| resource_entries_resolve(doc, r))
    })
}

/// A `Contents` reference (or each one in an array) is a stream that was
/// read to the end: it has a `Length`, or it has content.
fn content_streams_loaded(doc: &Document, contents: &Object) -> bool {
    match contents {
        Object::Reference(r) => match doc.get_object(*r) {
            Ok(Object::Stream(s)) => s.dict.has(b"Length") || !s.content.is_empty(),
            _ => true,
        },
        Object::Array(items) => items.iter().all(|o| content_streams_loaded(doc, o)),
        _ => true,
    }
}

/// Every entry of every category dictionary under a `Resources` value
/// names a loaded object. A category that is not a dictionary is not judged.
fn resource_entries_resolve(doc: &Document, resources: &Object) -> bool {
    let Ok((_, Object::Dictionary(resources))) = doc.dereference(resources) else {
        return false;
    };
    resources.iter().all(|(_, cat)| match doc.dereference(cat) {
        Ok((_, Object::Dictionary(d))) => d.iter().all(|(_, v)| references_resolve(doc, v)),
        Ok(_) => true,
        Err(_) => false,
    })
}

/// A reference, or every reference in an array, names a loaded object.
fn references_resolve(doc: &Document, obj: &Object) -> bool {
    match obj {
        Object::Reference(r) => doc.get_object(*r).is_ok(),
        Object::Array(items) => items.iter().all(|o| references_resolve(doc, o)),
        _ => true,
    }
}

/// An empty `Filter` array means no filter, but lopdf decodes such a
/// stream to nothing (its filter loop never runs); drop the entry so every
/// reader in the pipeline sees the raw content.
fn normalize_filters(doc: &mut Document) {
    for obj in doc.objects.values_mut() {
        if let Object::Stream(s) = obj
            && s.dict
                .get(b"Filter")
                .ok()
                .and_then(|f| f.as_array().ok())
                .is_some_and(Vec::is_empty)
        {
            s.dict.remove(b"Filter");
            s.dict.remove(b"DecodeParms");
        }
    }
}

/// Strings holding bytes outside printable ASCII are written in
/// hexadecimal. lopdf writes a string in the form it was read, escaping
/// what a literal needs escaped, and hayro's literal-string lexer, which
/// both verification levels rely on, reads some of those escaped binary
/// strings differently (an Indexed palette came back as gray indices);
/// hex has no escapes to disagree about. Text strings stay literal.
fn hex_binary_strings(doc: &mut Document) {
    for obj in doc.objects.values_mut() {
        hexify(obj);
    }
    for (_, value) in doc.trailer.iter_mut() {
        hexify(value);
    }
}

fn hexify(object: &mut Object) {
    match object {
        Object::String(bytes, format) if bytes.iter().any(|b| !(0x20..0x7f).contains(b)) => {
            *format = StringFormat::Hexadecimal;
        }
        Object::Array(items) => items.iter_mut().for_each(hexify),
        Object::Dictionary(dict) => dict.iter_mut().for_each(|(_, v)| hexify(v)),
        Object::Stream(stream) => stream.dict.iter_mut().for_each(|(_, v)| hexify(v)),
        _ => {}
    }
}

/// Every `Kids` entry reachable from the catalog resolves to a dictionary.
fn page_tree_is_intact(doc: &Document) -> bool {
    let Some(root) = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Pages").ok())
        .and_then(|p| p.as_reference().ok())
    else {
        return true;
    };
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Ok(Object::Dictionary(node)) = doc.get_object(id) else {
            return false;
        };
        let Ok(Object::Array(kids)) = node.get(b"Kids") else {
            continue;
        };
        for kid in kids {
            match kid {
                Object::Reference(r) => stack.push(*r),
                _ => return false,
            }
        }
    }
    true
}

pub fn serialize(doc: &mut Document, input: &[u8], report: &mut Report) -> Result<Vec<u8>> {
    // Object streams and an xref stream, packed as tightly as lopdf allows:
    // its defaults (100 objects per stream, level 6) leave a few percent on
    // the table against what good producers emit.
    let options = lopdf::SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .max_objects_per_stream(5000)
        .compression_level(9)
        .build();
    let mut modern = Vec::new();
    doc.save_with_options(&mut modern, options)
        .context("serializing (xref stream)")?;
    let mut classic = Vec::new();
    doc.save_to(&mut classic)
        .context("serializing (classic xref)")?;
    let best = if classic.len() < modern.len() {
        report.note("classic xref table was smaller than an xref stream");
        classic
    } else {
        modern
    };
    if best.len() >= input.len() {
        report.note(format!(
            "no stage produced a smaller file ({} vs {} bytes); output is the input unchanged",
            best.len(),
            input.len()
        ));
        report.output_bytes = input.len();
        return Ok(input.to_vec());
    }
    report.output_bytes = best.len();
    Ok(best)
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;

    fn doc_with_kids(missing_kid: bool) -> Document {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id });
        let mut kids = vec![Object::Reference(page)];
        if missing_kid {
            kids.push(Object::Reference(doc.new_object_id()));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 2 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        doc
    }

    #[test]
    fn missing_kids_are_refused() {
        assert!(page_tree_is_intact(&doc_with_kids(false)));
        let mut doc = doc_with_kids(true);
        assert!(!page_tree_is_intact(&doc));
        let mut report = Report::new(0);
        let err = run(
            &mut doc,
            &Config::preset(crate::config::Preset::Standard),
            &mut report,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), DAMAGED_PAGE_TREE);
    }

    #[test]
    fn unloaded_content_or_resources_are_refused() {
        let mut doc = doc_with_kids(false);
        let page = doc.get_pages()[&1];
        assert!(resources_are_intact(&doc));
        let missing = doc.new_object_id();
        doc.get_dictionary_mut(page).unwrap().set(
            "Resources",
            dictionary! { "Font" => dictionary! { "F1" => missing } },
        );
        assert!(!resources_are_intact(&doc));
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Resources", dictionary! { "Font" => dictionary! {} });
        assert!(resources_are_intact(&doc));
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Contents", vec![Object::Reference(missing)]);
        assert!(!resources_are_intact(&doc));
        // A stream the parser read as empty for lack of a Length.
        let empty = doc.add_object(lopdf::Stream::new(dictionary! {}, Vec::new()));
        let Ok(Object::Stream(s)) = doc.get_object_mut(empty) else {
            panic!("stream");
        };
        s.dict.remove(b"Length");
        doc.get_dictionary_mut(page).unwrap().set("Contents", empty);
        assert!(!resources_are_intact(&doc));
        let mut report = Report::new(0);
        let err = run(
            &mut doc,
            &Config::preset(crate::config::Preset::Standard),
            &mut report,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), DAMAGED_RESOURCES);
        let Ok(Object::Stream(s)) = doc.get_object_mut(empty) else {
            panic!("stream");
        };
        s.dict.set("Length", 0);
        assert!(resources_are_intact(&doc));
    }

    #[test]
    fn binary_strings_are_written_in_hexadecimal() {
        let mut doc = Document::with_version("1.5");
        let palette = Object::String(vec![0xff, 0x0d, b'(', b')', 0x28], StringFormat::Literal);
        let text = Object::String(b"Hello (world)".to_vec(), StringFormat::Literal);
        let id = doc.add_object(dictionary! { "Lookup" => vec![palette, text.clone()] });
        doc.trailer.set(
            "ID",
            vec![Object::String(vec![0x00, 0x01], StringFormat::Literal)],
        );
        hex_binary_strings(&mut doc);
        let dict = doc.get_dictionary(id).unwrap();
        let items = dict.get(b"Lookup").unwrap().as_array().unwrap();
        assert!(matches!(
            items[0],
            Object::String(_, StringFormat::Hexadecimal)
        ));
        assert!(matches!(items[1], Object::String(_, StringFormat::Literal)));
        let ids = doc.trailer.get(b"ID").unwrap().as_array().unwrap();
        assert!(matches!(
            ids[0],
            Object::String(_, StringFormat::Hexadecimal)
        ));
    }

    #[test]
    fn empty_filter_arrays_are_dropped() {
        let mut doc = doc_with_kids(false);
        let empty: Vec<Object> = Vec::new();
        let id = doc.add_object(lopdf::Stream::new(
            dictionary! { "Filter" => empty, "DecodeParms" => dictionary! {} },
            b"q Q".to_vec(),
        ));
        normalize_filters(&mut doc);
        let Ok(Object::Stream(s)) = doc.get_object(id) else {
            panic!("stream");
        };
        assert!(!s.dict.has(b"Filter") && !s.dict.has(b"DecodeParms"));
        assert_eq!(s.decompressed_content().unwrap(), b"q Q");
    }
}
