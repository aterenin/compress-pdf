//! Stage 4: remove non-visual document parts selected by [`Strip`] flags.
//!
//! Each flag maps to concrete dictionary keys:
//!   THREADS          catalog /Threads, page /B
//!   METADATA         catalog and per-object /Metadata streams
//!   PIECE_INFO       /PieceInfo on catalog, pages and XObjects
//!   STRUCT_TREE      catalog /StructTreeRoot, /MarkInfo; page /StructParents;
//!                    marked-content operators are left in place
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
use lopdf::Document;

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

    fn run(&self, _doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        // TODO(stage: strip)
        ctx.report.note(format!(
            "strip not implemented (requested: {:?})",
            ctx.config.strip
        ));
        Ok(())
    }
}
