//! Stage 4: remove non-visual document parts selected by [`Strip`] flags.
//!
//! Each flag maps to concrete dictionary keys:
//!   THREADS          catalog /Threads, page /B
//!   METADATA         /Metadata streams on the catalog and on any object
//!   PIECE_INFO       /PieceInfo on the catalog and on any object
//!   STRUCT_TREE      catalog /StructTreeRoot and /MarkInfo; /StructParents
//!                    and /StructParent on any object; marked-content
//!                    operators are left in place
//!   THUMBNAILS       page /Thumb
//!   SPIDER           catalog /SpiderInfo
//!   ALTERNATES       image /Alternates
//!   OUTPUT_INTENTS   catalog /OutputIntents
//!
//! Annotations and form fields are never touched (no preset asks for it).
//!
//! Removed objects become unreferenced and are collected by the structure
//! stage, so this stage only edits dictionaries.

use anyhow::Result;
use lopdf::{Dictionary, Document, Object};

use crate::config::{Config, Strip};
use crate::pipeline::{Context, Stage};

pub struct StripDocument;

impl Stage for StripDocument {
    fn name(&self) -> &'static str {
        "strip"
    }

    fn enabled(&self, config: &Config) -> bool {
        config.strip != Strip::NONE
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        let removed = strip(doc, ctx.config.strip);
        if removed > 0 {
            ctx.report.note(format!(
                "strip: removed {removed} entries ({:?})",
                ctx.config.strip
            ));
        }
        Ok(())
    }
}

/// Keys removed from the catalog only, per flag.
const CATALOG_KEYS: [(Strip, &[u8]); 6] = [
    (Strip::THREADS, b"Threads"),
    (Strip::STRUCT_TREE, b"StructTreeRoot"),
    (Strip::STRUCT_TREE, b"MarkInfo"),
    (Strip::SPIDER, b"SpiderInfo"),
    (Strip::OUTPUT_INTENTS, b"OutputIntents"),
    (Strip::METADATA, b"Metadata"),
];

/// Keys removed from every dictionary and stream dictionary, per flag.
/// Page-only keys (`/B`, `/Thumb`) and image-only keys (`/Alternates`)
/// only ever occur on those objects, so no type check is needed.
const OBJECT_KEYS: [(Strip, &[u8]); 7] = [
    (Strip::THREADS, b"B"),
    (Strip::METADATA, b"Metadata"),
    (Strip::PIECE_INFO, b"PieceInfo"),
    (Strip::STRUCT_TREE, b"StructParents"),
    (Strip::STRUCT_TREE, b"StructParent"),
    (Strip::THUMBNAILS, b"Thumb"),
    (Strip::ALTERNATES, b"Alternates"),
];

/// Returns the number of dictionary entries removed.
pub fn strip(doc: &mut Document, flags: Strip) -> usize {
    let mut removed = 0;
    if let Ok(catalog) = doc.catalog_mut() {
        removed += remove_keys(catalog, flags, &CATALOG_KEYS);
    }
    for obj in doc.objects.values_mut() {
        let dict = match obj {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &mut s.dict,
            _ => continue,
        };
        removed += remove_keys(dict, flags, &OBJECT_KEYS);
    }
    removed
}

fn remove_keys(dict: &mut Dictionary, flags: Strip, keys: &[(Strip, &[u8])]) -> usize {
    keys.iter()
        .filter(|(flag, _)| flags.contains(*flag))
        .filter(|(_, key)| dict.remove(key).is_some())
        .count()
}

#[cfg(test)]
mod tests {
    use lopdf::{Stream, dictionary};

    use super::*;

    /// A document exercising every strippable key once.
    fn loaded_doc() -> Document {
        let mut doc = Document::with_version("1.5");
        let meta = doc.add_object(Stream::new(
            dictionary! { "Type" => "Metadata" },
            b"<x/>".to_vec(),
        ));
        let thumb = doc.add_object(Stream::new(dictionary! {}, vec![0]));
        let image = doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Alternates" => vec![], "StructParent" => 3 },
            vec![0],
        ));
        let pages_id = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "B" => vec![], "Thumb" => thumb,
            "StructParents" => 0, "PieceInfo" => dictionary! {}, "Metadata" => meta,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im" => image } },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id, "Threads" => vec![], "SpiderInfo" => dictionary! {},
            "OutputIntents" => vec![], "StructTreeRoot" => dictionary! {}, "MarkInfo" => dictionary! {},
            "Metadata" => meta, "PieceInfo" => dictionary! {},
        });
        doc.trailer.set("Root", catalog);
        doc
    }

    fn keys(doc: &Document, type_name: &[u8]) -> Vec<String> {
        let mut out: Vec<String> = doc
            .objects
            .values()
            .filter_map(|o| match o {
                Object::Dictionary(d) => Some(d),
                Object::Stream(s) => Some(&s.dict),
                _ => None,
            })
            .filter(|d| {
                d.get(b"Type")
                    .and_then(Object::as_name)
                    .is_ok_and(|t| t == type_name)
            })
            .flat_map(|d| {
                d.iter()
                    .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn nothing_flagged_removes_nothing() {
        let mut doc = loaded_doc();
        assert_eq!(strip(&mut doc, Strip::NONE), 0);
    }

    #[test]
    fn each_flag_removes_only_its_keys() {
        let mut doc = loaded_doc();
        assert_eq!(strip(&mut doc, Strip::THUMBNAILS), 1);
        assert!(!keys(&doc, b"Page").contains(&"Thumb".to_string()));
        assert!(keys(&doc, b"Page").contains(&"B".to_string()));
        assert!(keys(&doc, b"Catalog").contains(&"Threads".to_string()));
    }

    #[test]
    fn standard_flags_strip_the_expected_set() {
        let mut doc = loaded_doc();
        let flags = Strip::THREADS
            | Strip::METADATA
            | Strip::PIECE_INFO
            | Strip::THUMBNAILS
            | Strip::SPIDER
            | Strip::ALTERNATES
            | Strip::OUTPUT_INTENTS;
        strip(&mut doc, flags);
        assert_eq!(
            keys(&doc, b"Catalog"),
            ["MarkInfo", "Pages", "StructTreeRoot", "Type"]
        );
        assert_eq!(
            keys(&doc, b"Page"),
            ["Parent", "Resources", "StructParents", "Type"]
        );
        assert_eq!(
            keys(&doc, b"XObject"),
            ["Length", "StructParent", "Subtype", "Type"]
        );
    }

    #[test]
    fn struct_tree_flag_removes_tree_and_parent_links() {
        let mut doc = loaded_doc();
        strip(&mut doc, Strip::STRUCT_TREE);
        for k in [
            "StructTreeRoot",
            "MarkInfo",
            "StructParents",
            "StructParent",
        ] {
            let all = [
                keys(&doc, b"Catalog"),
                keys(&doc, b"Page"),
                keys(&doc, b"XObject"),
            ]
            .concat();
            assert!(!all.contains(&k.to_string()), "{k} survived");
        }
    }
}
