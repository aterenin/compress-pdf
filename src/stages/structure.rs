//! Stage 5: structural cleanup. Runs last so it collects everything the
//! earlier stages orphaned.
//!
//! Today: Flate-compress uncompressed streams, drop unreferenced objects,
//! renumber. Object streams and xref streams come from `save_modern` in
//! `main`.
//!
//! Planned under `remove_redundant_objects`: deduplicate identical streams
//! and dictionaries by content hash and repoint references. Planned under
//! `optimize_resources`: drop unused entries from page /Resources.

use anyhow::Result;
use lopdf::Document;

use crate::pipeline::{Context, Stage};

pub struct CleanStructure;

impl Stage for CleanStructure {
    fn name(&self) -> &'static str {
        "structure"
    }

    fn run(&self, doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        doc.compress();
        let pruned = doc.prune_objects();
        doc.renumber_objects();
        if !pruned.is_empty() {
            tracing::debug!(count = pruned.len(), "pruned unreferenced objects");
        }
        if ctx.config.remove_redundant_objects {
            // TODO(stage: structure): content-hash dedupe.
        }
        Ok(())
    }
}
