//! Stage orchestration.
//!
//! A stage is a black box with one contract: it receives the whole document
//! plus shared context, mutates the document in place, and records what it did
//! in the report. Stages do not call each other. Anything one stage needs from
//! another travels through [`Context`] (today: the image usage analysis).

use std::time::Instant;

use anyhow::{Context as _, Result};
use lopdf::Document;

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

pub fn run(doc: &mut Document, config: &Config, report: &mut Report) -> Result<()> {
    if doc.trailer.has(b"Encrypt") {
        // Out of scope for v1 (CLAUDE.md). Refusing is safer than writing a
        // file that claims to be encrypted but is not, or vice versa.
        anyhow::bail!(ENCRYPTED_INPUT);
    }
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
