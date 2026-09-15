//! Stage 3: fonts.
//!
//! Planned, in order of value: unembed the 14 standard fonts when the font's
//! Unicode mapping is trustworthy; subset embedded TrueType/CFF programs to
//! the glyphs actually referenced by content streams; merge duplicate
//! embeddings of the same font. Deferred until the image stage is solid.

use anyhow::Result;
use lopdf::Document;

use crate::config::Config;
use crate::pipeline::{Context, Stage};

pub struct OptimizeFonts;

impl Stage for OptimizeFonts {
    fn name(&self) -> &'static str {
        "fonts"
    }

    fn enabled(&self, config: &Config) -> bool {
        config.subset_fonts || config.merge_fonts || config.remove_standard_fonts
    }

    fn run(&self, _doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        // TODO(stage: fonts)
        ctx.report.note("font optimization not implemented");
        Ok(())
    }
}
