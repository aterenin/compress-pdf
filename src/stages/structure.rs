//! Stage 5: structural cleanup. Runs last so it collects everything the
//! earlier stages orphaned.
//!
//! In order: drop unused entries from resource dictionaries
//! (`optimize_resources`), Flate-compress uncompressed streams, merge
//! duplicate objects (`remove_redundant_objects`), drop unreferenced
//! objects, renumber, and raise the header version to what the content
//! needs. Object streams and the xref stream are written by
//! `pipeline::serialize`.

mod dedupe;
mod resources;

use anyhow::Result;
use std::collections::{BTreeSet, HashSet};
use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::pipeline::{Context, Stage};

pub struct CleanStructure;

impl Stage for CleanStructure {
    fn name(&self) -> &'static str {
        "structure"
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        if ctx.config.optimize_resources {
            let removed = resources::prune_unused(doc, &ctx.usage.appearance_fonts);
            if removed > 0 {
                ctx.report.note(format!(
                    "structure: removed {removed} unused resource entries"
                ));
            }
        }
        if ctx.config.rebuild_content_streams {
            let done = rewrite_content_streams(doc);
            if done.streams > 0 {
                ctx.report.note(format!(
                    "structure: re-serialized {} content streams, {} bytes smaller",
                    done.streams, done.saved
                ));
            }
        }
        doc.compress();
        if ctx.config.remove_redundant_objects {
            let merged = dedupe::merge_duplicates(doc);
            if merged > 0 {
                ctx.report
                    .note(format!("structure: merged {merged} duplicate objects"));
            }
        }
        let pruned = doc.prune_objects().len();
        if pruned > 0 {
            tracing::debug!(count = pruned, "pruned unreferenced objects");
        }
        let dangling = drop_dangling_references(doc);
        if dangling > 0 {
            ctx.report.note(format!(
                "structure: {dangling} references to missing objects removed"
            ));
        }
        doc.renumber_objects();
        bump_version(doc);
        Ok(())
    }
}

/// Streams larger than this are left alone by the content rewrite.
const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;

/// What the pass did: streams rewritten and bytes saved in their stored
/// (compressed) form.
#[derive(Debug, Default, PartialEq, Eq)]
struct Rewrite {
    streams: usize,
    saved: usize,
}

/// Rewrite every page content, form, pattern and Type 3 glyph stream
/// whose canonical form is smaller.
fn rewrite_content_streams(doc: &mut Document) -> Rewrite {
    let mut done = Rewrite::default();
    for id in content_streams(doc) {
        let Ok(Object::Stream(stream)) = doc.get_object(id) else {
            continue;
        };
        let Some((candidate, saved)) = smaller_form(stream) else {
            continue;
        };
        if let Ok(Object::Stream(stream)) = doc.get_object_mut(id) {
            stream.set_plain_content(candidate);
            stream.dict.set("Filter", "FlateDecode");
            done.streams += 1;
            done.saved += saved;
        }
    }
    done
}

/// Every object that holds content-stream syntax, in a fixed order.
fn content_streams(doc: &Document) -> BTreeSet<ObjectId> {
    let mut ids = BTreeSet::new();
    for page in doc.page_iter() {
        ids.extend(doc.get_page_contents(page));
    }
    for (&id, obj) in &doc.objects {
        match obj {
            Object::Stream(s) if resources::is_form_or_pattern(&s.dict) => {
                ids.insert(id);
            }
            Object::Dictionary(d) if resources::is_type3(d) => ids.extend(charprocs(d)),
            _ => {}
        }
    }
    ids
}

fn charprocs(font: &Dictionary) -> Vec<ObjectId> {
    font.get(b"CharProcs")
        .and_then(Object::as_dict)
        .map(|procs| {
            procs
                .iter()
                .filter_map(|(_, v)| v.as_reference().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The canonical form of a stream, Flate-compressed, when it is smaller
/// than what is stored (or than the original compressed the same way, for
/// a stream stored raw). Returns the bytes and the saving.
fn smaller_form(stream: &Stream) -> Option<(Vec<u8>, usize)> {
    let content = stream
        .decompressed_content_with_limit(MAX_CONTENT_BYTES)
        .ok()?;
    let canonical = crate::content::canonical(&content)?;
    let candidate = deflate(&canonical);
    let stored = if stream.dict.has(b"Filter") {
        stream.content.len()
    } else {
        deflate(&content).len()
    };
    let saved = stored.checked_sub(candidate.len()).filter(|s| *s > 0)?;
    Some((candidate, saved))
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 2), Compression::best());
    enc.write_all(data).ok();
    enc.finish().unwrap_or_default()
}

/// Raise the header version to the minimum the content requires. Never
/// lowers it. Object streams (1.5) are handled by lopdf's writer.
fn bump_version(doc: &mut Document) {
    if uses_filter(doc, b"JBIG2Decode") && doc.version.as_str() < "1.4" {
        doc.version = "1.4".into();
    }
}

/// A reference to an object that does not exist reads as absent anyway,
/// but left in place it would be rebound to whatever object receives that
/// number when the file is renumbered. Dictionary entries holding one are
/// removed and array elements dropped. Returns how many were removed.
fn drop_dangling_references(doc: &mut Document) -> usize {
    let ids: HashSet<ObjectId> = doc.objects.keys().copied().collect();
    let mut count = 0;
    for obj in doc.objects.values_mut() {
        drop_dangling_in(obj, &ids, &mut count);
    }
    let mut trailer = Object::Dictionary(std::mem::take(&mut doc.trailer));
    drop_dangling_in(&mut trailer, &ids, &mut count);
    if let Object::Dictionary(d) = trailer {
        doc.trailer = d;
    }
    count
}

fn drop_dangling_in(obj: &mut Object, ids: &HashSet<ObjectId>, count: &mut usize) {
    let dangling = |o: &Object| matches!(o, Object::Reference(r) if !ids.contains(r));
    match obj {
        Object::Array(items) => {
            let before = items.len();
            items.retain(|o| !dangling(o));
            *count += before - items.len();
            for item in items.iter_mut() {
                drop_dangling_in(item, ids, count);
            }
        }
        Object::Dictionary(dict) => drop_dangling_in_dict(dict, ids, count),
        Object::Stream(stream) => drop_dangling_in_dict(&mut stream.dict, ids, count),
        _ => {}
    }
}

fn drop_dangling_in_dict(dict: &mut lopdf::Dictionary, ids: &HashSet<ObjectId>, count: &mut usize) {
    let doomed: Vec<Vec<u8>> = dict
        .iter()
        .filter(|(_, v)| matches!(v, Object::Reference(r) if !ids.contains(r)))
        .map(|(k, _)| k.clone())
        .collect();
    *count += doomed.len();
    for key in doomed {
        dict.remove(&key);
    }
    for (_, value) in dict.iter_mut() {
        drop_dangling_in(value, ids, count);
    }
}

fn uses_filter(doc: &Document, filter: &[u8]) -> bool {
    doc.objects
        .values()
        .any(|obj| stream_uses_filter(obj, filter))
}

fn stream_uses_filter(obj: &Object, filter: &[u8]) -> bool {
    match obj {
        Object::Stream(s) => s.filters().is_ok_and(|fs| fs.contains(&filter)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use lopdf::dictionary;

    use super::*;

    fn doc_with_stream(version: &str, filter: &str) -> Document {
        let mut doc = Document::with_version(version);
        let stream = Stream::new(dictionary! { "Filter" => filter }, vec![0u8; 4]);
        let id = doc.add_object(stream);
        doc.trailer.set("Root", id);
        doc
    }

    #[test]
    fn dangling_references_are_dropped_before_renumbering() {
        let mut doc = Document::with_version("1.5");
        let missing = doc.new_object_id();
        let holder = doc.add_object(dictionary! {
            "Next" => missing, "Same" => 1,
            "List" => vec![Object::Reference(missing), 2.into()],
            "Inner" => dictionary! { "Deep" => missing },
        });
        doc.trailer.set("Root", holder);
        assert_eq!(drop_dangling_references(&mut doc), 3);
        let dict = doc.get_object(holder).unwrap().as_dict().unwrap();
        assert!(!dict.has(b"Next"));
        assert_eq!(dict.get(b"List").unwrap().as_array().unwrap(), &[2.into()]);
        assert!(!dict.get(b"Inner").unwrap().as_dict().unwrap().has(b"Deep"));
        assert_eq!(drop_dangling_references(&mut doc), 0);
    }

    fn doc_with_form(xfa: bool, da: &str) -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let f1 = doc.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
        );
        let f2 = doc.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier" },
        );
        let fonts = doc.add_object(dictionary! { "Helv" => f1, "Cour" => f2 });
        let field = doc.add_object(
            dictionary! { "T" => Object::string_literal("x"), "DA" => Object::string_literal(da) },
        );
        let mut acro =
            dictionary! { "Fields" => vec![field.into()], "DR" => dictionary! { "Font" => fonts } };
        if xfa {
            acro.set("XFA", Object::string_literal("<xdp/>"));
        }
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "AcroForm" => acro });
        doc.trailer.set("Root", catalog);
        (doc, fonts)
    }

    #[test]
    fn default_resource_fonts_no_appearance_names_are_dropped() {
        let (mut doc, fonts) = doc_with_form(false, "/Helv 12 Tf 0 g");
        let used = HashSet::from([b"Helv".to_vec()]);
        assert_eq!(resources::prune_unused(&mut doc, &used), 1);
        let dict = doc.get_dictionary(fonts).unwrap();
        assert!(dict.has(b"Helv") && !dict.has(b"Cour"));
        // An XFA form may pick fonts by name: nothing is dropped.
        let (mut doc, fonts) = doc_with_form(true, "/Helv 12 Tf 0 g");
        assert_eq!(resources::prune_unused(&mut doc, &used), 0);
        assert!(doc.get_dictionary(fonts).unwrap().has(b"Cour"));
    }

    #[test]
    fn default_resource_fonts_shared_with_a_page_are_kept() {
        // The font dictionary doubles as a page's resources: content, not
        // only appearance strings, selects from it.
        let (mut doc, fonts) = doc_with_form(false, "/Helv 12 Tf 0 g");
        let contents = doc.add_object(Stream::new(dictionary! {}, b"/Cour 1 Tf".to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => contents,
            "Resources" => dictionary! { "Font" => fonts },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(root).unwrap().set("Pages", pages_id);
        let used = HashSet::from([b"Helv".to_vec()]);
        assert_eq!(resources::prune_unused(&mut doc, &used), 0);
        assert!(doc.get_dictionary(fonts).unwrap().has(b"Cour"));
    }

    #[test]
    fn jbig2_needs_1_4() {
        let mut doc = doc_with_stream("1.3", "JBIG2Decode");
        bump_version(&mut doc);
        assert_eq!(doc.version, "1.4");
    }

    #[test]
    fn version_is_never_lowered() {
        let mut doc = doc_with_stream("1.7", "JBIG2Decode");
        bump_version(&mut doc);
        assert_eq!(doc.version, "1.7");
        let mut doc = doc_with_stream("1.3", "FlateDecode");
        bump_version(&mut doc);
        assert_eq!(doc.version, "1.3");
    }

    #[test]
    fn rewrite_replaces_only_smaller_streams() {
        let mut doc = Document::with_version("1.5");
        let verbose =
            b"q   1.00000 0.00000 0.00000 1.00000 0.00000 0.00000 cm   % identity\n/Im1 Do   Q\n"
                .repeat(40);
        let contents = doc.add_object(Stream::new(dictionary! {}, verbose));
        let tight = doc.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => vec![contents.into(), tight.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let done = rewrite_content_streams(&mut doc);
        assert_eq!(done.streams, 1);
        assert!(done.saved > 0);
        let Ok(Object::Stream(s)) = doc.get_object(contents) else {
            panic!("stream");
        };
        assert_eq!(
            s.dict.get(b"Filter").unwrap().as_name().unwrap(),
            b"FlateDecode"
        );
        let text = s.decompressed_content().unwrap();
        assert!(
            text.starts_with(b"q 1 0 0 1 0 0 cm/Im1 Do Q q 1 0 0 1"),
            "{}",
            String::from_utf8_lossy(&text)
        );
        let Ok(Object::Stream(s)) = doc.get_object(tight) else {
            panic!("stream");
        };
        assert!(!s.dict.has(b"Filter"));
    }
}
