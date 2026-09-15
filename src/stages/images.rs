//! Stage 2: recompress image XObjects.
//!
//! Per image, in order:
//!   1. classify: bitonal / indexed / continuous (gray or color), or
//!      unsupported (route to "kept", note why);
//!   2. decode to a raster (`decode` submodule, the largest piece of work);
//!   3. transform: color conversion, color-complexity reduction,
//!      downsampling when effective DPI exceeds the class threshold;
//!   4. encode with every codec allowed for the class, plus the original
//!      bytes when `SOURCE` is allowed, and keep the smallest;
//!   5. rewrite the stream and its dictionary, keeping SMask/Mask consistent.
//!
//! Invariant: an image never gets larger, and an image we cannot fully
//! understand is left byte-for-byte untouched.

use anyhow::Result;
use lopdf::Document;

use crate::config::Config;
use crate::pipeline::{Context, Stage};

pub struct RecompressImages;

impl Stage for RecompressImages {
    fn name(&self) -> &'static str {
        "images"
    }

    fn enabled(&self, config: &Config) -> bool {
        !(config.bitonal.is_empty() && config.continuous.is_empty() && config.indexed.is_empty())
    }

    fn run(&self, _doc: &mut Document, ctx: &mut Context<'_>) -> Result<()> {
        // TODO(stage: images): see module docs and CLAUDE.md for the plan.
        ctx.report.note("image recompression not implemented");
        Ok(())
    }
}
