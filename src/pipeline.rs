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

pub fn run(doc: &mut Document, config: &Config, report: &mut Report) -> Result<()> {
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
