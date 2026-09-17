//! `cargo evals score`: run a subset through the pipeline and compare
//! output sizes with reference outputs under `evals/reference/<name>/`,
//! each at the same relative path as its original under `evals/corpus/`
//! (CLAUDE.md, "Evals tooling").

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use compress_pdf::config::{Config, Preset};
use serde::Deserialize;

const REFERENCE_DIR: &str = "evals/reference";
/// Our outputs, as `<preset>/<path under evals/corpus>`, for viewing.
const OUTPUT_DIR: &str = "evals/output";
const PRESETS: [&str; 3] = ["less", "standard", "more"];

/// A reference directory: an external tool's outputs, laid out like the
/// corpus.
struct Reference {
    name: String,
    /// From `tool.toml`, when present.
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolInfo {
    name: Option<String>,
    version: Option<String>,
    date: Option<String>,
    settings: Option<String>,
}

pub fn score(
    preset: Option<&str>,
    reference: Option<&str>,
    subset: &str,
    render: bool,
) -> Result<()> {
    let config = super::load_config()?;
    let files = config
        .subsets
        .get(subset)
        .with_context(|| format!("no subset `{subset}` in {}", super::CONFIG))?;
    let presets: Vec<&str> = match preset {
        Some(p) => vec![p],
        None => PRESETS.to_vec(),
    };
    for p in presets {
        let refs = references(p, reference, preset.is_some())?;
        score_preset(p, files, &refs, render)?;
    }
    Ok(())
}

/// Reference directories to compare with: the named one, all of them when
/// a preset was chosen explicitly, else those whose name ends in the
/// preset.
fn references(preset: &str, only: Option<&str>, explicit_preset: bool) -> Result<Vec<Reference>> {
    let Ok(entries) = fs::read_dir(REFERENCE_DIR) else {
        return Ok(Vec::new());
    };
    let mut refs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() || name.starts_with('.') {
            continue;
        }
        let wanted = match only {
            Some(o) => name == o,
            None => explicit_preset || name.ends_with(&format!("-{preset}")),
        };
        if !wanted {
            continue;
        }
        let label = fs::read_to_string(entry.path().join("tool.toml"))
            .ok()
            .and_then(|t| toml::from_str::<ToolInfo>(&t).ok())
            .map(|t| {
                [t.name, t.version, t.date, t.settings]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ")
            });
        refs.push(Reference { name, label });
    }
    refs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(refs)
}

struct Row {
    file: String,
    input: u64,
    ours: Option<u64>,
    /// Minimum page SSIM between input and our output, when rendered.
    ssim: Option<f32>,
    refs: Vec<Option<u64>>,
}

fn score_preset(preset: &str, files: &[String], refs: &[Reference], render: bool) -> Result<()> {
    let (config, preset_value) = preset_config(preset)?;
    println!("preset {preset}");
    for r in refs {
        println!(
            "  reference {}: {}",
            r.name,
            r.label.as_deref().unwrap_or("(no tool.toml)")
        );
    }
    let mut rows = Vec::new();
    for f in files {
        let path = Path::new(super::CORPUS_DIR).join(f);
        let Ok(input) = fs::read(&path) else {
            println!("  missing: {f}");
            continue;
        };
        let refs_sizes = refs
            .iter()
            .map(|r| {
                fs::metadata(Path::new(REFERENCE_DIR).join(&r.name).join(f))
                    .ok()
                    .map(|m| m.len())
            })
            .collect();
        let (ours, ssim) = compress(&input, &config, render.then_some(preset_value));
        if let Some(bytes) = &ours {
            write_output(preset, f, bytes)?;
        }
        rows.push(Row {
            file: f.clone(),
            input: input.len() as u64,
            ours: ours.map(|b| b.len() as u64),
            ssim,
            refs: refs_sizes,
        });
    }
    print_table(&rows, refs);
    Ok(())
}

/// Writes our output where a reference output would sit, under
/// `evals/output/<preset>/` instead of `evals/reference/<name>/`.
fn write_output(preset: &str, file: &str, bytes: &[u8]) -> Result<()> {
    let path = Path::new(OUTPUT_DIR).join(preset).join(file);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn preset_config(name: &str) -> Result<(Config, Preset)> {
    let preset = match name {
        "less" => Preset::Less,
        "standard" => Preset::Standard,
        "more" => Preset::More,
        other => bail!("unknown preset `{other}`"),
    };
    Ok((Config::preset(preset), preset))
}

/// Our output for an input, and the minimum page SSIM against it when
/// `render` names the preset, on a thread with room for deep parsers.
/// The output is `None` when the pipeline refuses or fails.
fn compress(
    input: &[u8],
    config: &Config,
    render: Option<Preset>,
) -> (Option<Vec<u8>>, Option<f32>) {
    let input = input.to_vec();
    let config = config.clone();
    let result = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || run_pipeline(&input, &config, render))
        .ok()
        .and_then(|h| h.join().ok())
        .flatten();
    match result {
        Some((bytes, ssim)) => (Some(bytes), ssim),
        None => (None, None),
    }
}

fn run_pipeline(
    input: &[u8],
    config: &Config,
    render: Option<Preset>,
) -> Option<(Vec<u8>, Option<f32>)> {
    let mut doc = lopdf::Document::load_mem(input).ok()?;
    let mut report = compress_pdf::report::Report::new(input.len());
    compress_pdf::pipeline::run(&mut doc, config, &mut report).ok()?;
    let out = compress_pdf::pipeline::serialize(&mut doc, input, &mut report).ok()?;
    let ssim = render.and_then(|p| {
        compress_pdf::verify::render::compare(input, &out, p)
            .ok()
            .and_then(|c| c.min())
    });
    Some((out, ssim))
}

fn print_table(rows: &[Row], refs: &[Reference]) {
    let name_width = rows
        .iter()
        .map(|r| r.file.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 60);
    let render = rows.iter().any(|r| r.ssim.is_some());
    print!("  {:<name_width$} {:>10} {:>16}", "file", "input", "ours");
    if render {
        print!(" {:>8}", "min ssim");
    }
    for r in refs {
        print!(" {:>16}", truncate(&r.name, 16));
    }
    println!();
    for row in rows {
        print!(
            "  {:<name_width$} {:>10} {:>16}",
            truncate(&row.file, name_width),
            row.input,
            cell(row.ours, row.input)
        );
        if render {
            print!(" {:>8}", ssim_cell(row.ssim));
        }
        for size in &row.refs {
            print!(" {:>16}", cell(*size, row.input));
        }
        println!();
    }
    print_totals(rows, refs, name_width, render);
}

fn print_totals(rows: &[Row], refs: &[Reference], name_width: usize, render: bool) {
    let input_total: u64 = rows.iter().map(|r| r.input).sum();
    let ours_total: u64 = rows.iter().filter_map(|r| r.ours).sum();
    print!(
        "  {:<name_width$} {:>10} {:>16}",
        "total",
        input_total,
        cell(Some(ours_total), input_total)
    );
    if render {
        print!(
            " {:>8}",
            ssim_cell(rows.iter().filter_map(|r| r.ssim).reduce(f32::min))
        );
    }
    for (i, _) in refs.iter().enumerate() {
        // Totals over the files the reference covers.
        let covered: Vec<&Row> = rows.iter().filter(|r| r.refs[i].is_some()).collect();
        let ref_total: u64 = covered.iter().filter_map(|r| r.refs[i]).sum();
        let base: u64 = covered.iter().map(|r| r.input).sum();
        print!(" {:>16}", cell(Some(ref_total), base));
    }
    println!();
    if refs.is_empty() {
        println!();
        return;
    }
    print!("  {:<name_width$} {:>10} {:>16}", "coverage", "", "");
    if render {
        print!(" {:>8}", "");
    }
    for (i, _) in refs.iter().enumerate() {
        // Ours on the same files, for a fair comparison.
        let covered: Vec<&Row> = rows.iter().filter(|r| r.refs[i].is_some()).collect();
        let ours_same: u64 = covered.iter().filter_map(|r| r.ours).sum();
        let base: u64 = covered.iter().map(|r| r.input).sum();
        print!(
            " {:>16}",
            format!(
                "{}/{} ours {}",
                covered.len(),
                rows.len(),
                ratio(ours_same, base)
            )
        );
    }
    println!();
    println!();
}

fn ssim_cell(ssim: Option<f32>) -> String {
    ssim.map_or("-".to_string(), |s| format!("{s:.3}"))
}

fn cell(size: Option<u64>, input: u64) -> String {
    match size {
        Some(s) => format!("{s} ({})", ratio(s, input)),
        None => "-".into(),
    }
}

fn ratio(size: u64, input: u64) -> String {
    if input == 0 {
        return "-".into();
    }
    format!("{:.0}%", size as f64 * 100.0 / input as f64)
}

fn truncate(s: &str, width: usize) -> String {
    if s.len() <= width {
        s.to_string()
    } else {
        format!("...{}", &s[s.len() - (width - 3)..])
    }
}
