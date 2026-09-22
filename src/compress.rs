//! One call that compresses a PDF the way the command line does: run the
//! stages, serialize under the never-grow rule, check the output with the
//! independent parser, and optionally render both documents and compare
//! them. The binary is a thin client of [`compress`]; so is any program
//! that wants the same guarantees.

use std::fmt;

use anyhow::{Context as _, Result, bail};
use lopdf::Document;

use crate::config::{Config, Preset};
use crate::report::Report;
use crate::{pipeline, verify};

/// How much of the output to check before handing it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verify {
    /// Re-parse the output with a reader that shares no code with the
    /// writer; a problem the input did not have rejects the output.
    Structural,
    /// Structural, plus every page of input and output rendered and
    /// compared. A page under the preset's similarity floor is a warning
    /// in the report, or with `strict` a rejection.
    Render { preset: Preset, strict: bool },
}

/// A compressed document and the report of what was done to it.
#[derive(Debug)]
#[non_exhaustive]
pub struct Compressed {
    /// The output bytes; the input bytes unchanged when nothing was smaller.
    pub output: Vec<u8>,
    pub report: Report,
}

/// Why nothing was produced, with the report up to that point.
#[derive(Debug)]
#[non_exhaustive]
pub struct Rejected {
    pub report: Report,
    pub reason: anyhow::Error,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.reason)
    }
}

impl std::error::Error for Rejected {}

/// Compresses `input` under `config`. Refuses input the pipeline cannot
/// handle safely (a password it needs, damage it cannot see past), and
/// output that verifies worse than the input.
pub fn compress(input: &[u8], config: &Config, verify: Verify) -> Result<Compressed, Rejected> {
    let mut report = Report::new(input.len());
    match run(input, config, verify, &mut report) {
        Ok(output) => Ok(Compressed { output, report }),
        Err(reason) => Err(Rejected { report, reason }),
    }
}

fn run(input: &[u8], config: &Config, verify: Verify, report: &mut Report) -> Result<Vec<u8>> {
    let mut doc = Document::load_mem(input).context("parsing the input")?;
    let pages = doc.get_pages().len();
    pipeline::run(&mut doc, config, report)?;
    let output = pipeline::serialize(&mut doc, input, report)?;
    // Nothing to verify when the output is the input unchanged.
    if output != input {
        check_structure(input, &output, pages, report)?;
        check_render(input, &output, verify, report)?;
    }
    Ok(output)
}

/// Problems the input already had are warnings; new ones are bugs.
fn check_structure(input: &[u8], output: &[u8], pages: usize, report: &mut Report) -> Result<()> {
    let verification = verify::verify(output, pages);
    report.note(verification.to_string());
    if verification.is_ok() {
        return Ok(());
    }
    let baseline = verify::verify(input, pages);
    let regressions = verification.regressions_from(&baseline);
    if !regressions.is_empty() {
        bail!(
            "output failed verification with new problems {regressions:?}; nothing written (this is a bug, please report it)"
        );
    }
    report.note("warning: the input already had these problems; output written anyway");
    Ok(())
}

fn check_render(input: &[u8], output: &[u8], verify: Verify, report: &mut Report) -> Result<()> {
    let Verify::Render { preset, strict } = verify else {
        return Ok(());
    };
    match verify::render::compare(input, output, preset) {
        Ok(comparison) => {
            report.note(comparison.to_string());
            if comparison.below_floor().is_empty() {
                return Ok(());
            }
            if strict {
                bail!("pages below the similarity floor; nothing written (--strict)");
            }
            report.note("warning: pages below the similarity floor; output written anyway");
        }
        Err(e) => report.note(format!("render: skipped, {e}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use lopdf::{Object, dictionary};

    use super::*;

    /// A one-page document with an uncompressed content stream, as bytes.
    fn small_pdf() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        // Uncompressed and repetitive, so the structure stage has something
        // to shrink and the output is not the input unchanged.
        let content = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            b"0 0 1 rg 10 10 100 100 re f\n".repeat(200),
        ));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn compresses_and_verifies_a_small_document() {
        let input = small_pdf();
        let config = Config::preset(Preset::Standard);
        let verify = Verify::Render {
            preset: Preset::Standard,
            strict: true,
        };
        let done = compress(&input, &config, verify).unwrap();
        assert!(done.output.len() <= input.len());
        assert!(done.report.notes.iter().any(|n| n.starts_with("verify:")));
        assert!(done.report.notes.iter().any(|n| n.starts_with("render:")));
    }

    #[test]
    fn unparseable_input_is_rejected_with_its_report() {
        let config = Config::preset(Preset::Less);
        let rejected = compress(b"not a pdf", &config, Verify::Structural).unwrap_err();
        assert!(rejected.to_string().contains("parsing the input"));
        assert_eq!(rejected.report.notes.len(), 0);
    }
}
