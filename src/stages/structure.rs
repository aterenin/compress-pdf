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
use lopdf::{Document, Object};

use crate::pipeline::{Context, Stage};

pub struct CleanStructure;

impl Stage for CleanStructure {
    fn name(&self) -> &'static str {
        "structure"
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        if ctx.config.optimize_resources {
            let removed = resources::prune_unused(doc);
            if removed > 0 {
                ctx.report.note(format!(
                    "structure: removed {removed} unused resource entries"
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
        doc.renumber_objects();
        bump_version(doc);
        Ok(())
    }
}

/// Raise the header version to the minimum the content requires. Never
/// lowers it. Object streams (1.5) are handled by lopdf's writer.
fn bump_version(doc: &mut Document) {
    if uses_filter(doc, b"JBIG2Decode") && doc.version.as_str() < "1.4" {
        doc.version = "1.4".into();
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
    use lopdf::{Stream, dictionary};

    use super::*;

    fn doc_with_stream(version: &str, filter: &str) -> Document {
        let mut doc = Document::with_version(version);
        let stream = Stream::new(dictionary! { "Filter" => filter }, vec![0u8; 4]);
        let id = doc.add_object(stream);
        doc.trailer.set("Root", id);
        doc
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
}
