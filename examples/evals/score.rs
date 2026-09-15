//! `cargo evals score`: run a subset through the pipeline and compare
//! output sizes with reference outputs under `evals/reference/<name>/`,
//! matched by file name (CLAUDE.md, "Evals tooling").

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const REFERENCE_DIR: &str = "evals/reference";
const PRESETS: [&str; 3] = ["less", "standard", "more"];

/// A reference directory: an external tool's outputs matched by file name.
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

pub fn score(preset: Option<&str>, reference: Option<&str>, subset: &str) -> Result<()> {
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
        score_preset(p, files, &refs)?;
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
    refs: Vec<Option<u64>>,
}

fn score_preset(preset: &str, files: &[String], refs: &[Reference]) -> Result<()> {
    let config = preset_config(preset)?;
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
        let base = Path::new(f).file_name().unwrap_or_default().to_os_string();
        let refs_sizes = refs
            .iter()
            .map(|r| {
                fs::metadata(Path::new(REFERENCE_DIR).join(&r.name).join(&base))
                    .ok()
                    .map(|m| m.len())
            })
            .collect();
        rows.push(Row {
            file: f.clone(),
            input: input.len() as u64,
            ours: compress(&input, &config),
            refs: refs_sizes,
        });
    }
    print_table(&rows, refs);
    Ok(())
}

fn preset_config(name: &str) -> Result<compress_pdf::config::Config> {
    use compress_pdf::config::{Config, Preset};
    let preset = match name {
        "less" => Preset::Less,
        "standard" => Preset::Standard,
        "more" => Preset::More,
        other => bail!("unknown preset `{other}`"),
    };
    Ok(Config::preset(preset))
}

/// Our output size for an input, on a thread with room for deep parsers.
/// `None` when the pipeline refuses or fails.
fn compress(input: &[u8], config: &compress_pdf::config::Config) -> Option<u64> {
    let input = input.to_vec();
    let config = config.clone();
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let mut doc = lopdf::Document::load_mem(&input).ok()?;
            let mut report = compress_pdf::report::Report::new(input.len());
            compress_pdf::pipeline::run(&mut doc, &config, &mut report).ok()?;
            let out = compress_pdf::pipeline::serialize(&mut doc, &input, &mut report).ok()?;
            Some(out.len() as u64)
        })
        .ok()?
        .join()
        .ok()
        .flatten()
}

fn print_table(rows: &[Row], refs: &[Reference]) {
    let name_width = rows
        .iter()
        .map(|r| r.file.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 60);
    print!("  {:<name_width$} {:>10} {:>16}", "file", "input", "ours");
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
        for size in &row.refs {
            print!(" {:>16}", cell(*size, row.input));
        }
        println!();
    }
    let input_total: u64 = rows.iter().map(|r| r.input).sum();
    let ours_total: u64 = rows.iter().filter_map(|r| r.ours).sum();
    print!(
        "  {:<name_width$} {:>10} {:>16}",
        "total",
        input_total,
        cell(Some(ours_total), input_total)
    );
    for (i, _) in refs.iter().enumerate() {
        // Totals over the files the reference covers, ours on the same files.
        let covered: Vec<&Row> = rows.iter().filter(|r| r.refs[i].is_some()).collect();
        let ref_total: u64 = covered.iter().filter_map(|r| r.refs[i]).sum();
        let base: u64 = covered.iter().map(|r| r.input).sum();
        print!(" {:>16}", cell(Some(ref_total), base));
    }
    println!();
    if !refs.is_empty() {
        print!("  {:<name_width$} {:>10} {:>16}", "coverage", "", "");
        for (i, _) in refs.iter().enumerate() {
            let n = rows.iter().filter(|r| r.refs[i].is_some()).count();
            let ours_same: u64 = rows
                .iter()
                .filter(|r| r.refs[i].is_some())
                .filter_map(|r| r.ours)
                .sum();
            let base: u64 = rows
                .iter()
                .filter(|r| r.refs[i].is_some())
                .map(|r| r.input)
                .sum();
            print!(
                " {:>16}",
                format!("{n}/{} ours {}", rows.len(), ratio(ours_same, base))
            );
        }
        println!();
    }
    println!();
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
