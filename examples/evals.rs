//! `cargo evals <subcommand>`: evaluation tooling (see AGENTS.md, "Evals tooling").
//!
//! All subcommands are implemented; see the `Cmd` enum.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};

#[path = "../tests/probes/generators.rs"]
mod generators;
#[path = "evals/score.rs"]
mod score;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

const CONFIG: &str = "evals.toml";
const CORPUS_DIR: &str = "evals/corpus";
const MANIFEST: &str = "evals/corpus/MANIFEST.json";
const LINKED_DIR: &str = "evals/corpus/pdfjs-linked";
const DOWNLOAD_WORKERS: usize = 8;

// ---------------------------------------------------------------- config

#[derive(Debug, Deserialize)]
struct Config {
    source: Vec<Source>,
    /// Named file lists relative to evals/corpus/ (`quick`, `scoring`, ...).
    #[allow(dead_code)] // read by `score` and the test harness, not by fetch/status
    subsets: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct Source {
    name: String,
    git: String,
    rev: String,
    paths: Vec<String>,
    license: String,
    #[allow(dead_code)]
    roles: Vec<String>,
    #[serde(default)]
    links: bool,
}

fn load_config() -> Result<Config> {
    let text = fs::read_to_string(CONFIG).with_context(|| format!("reading {CONFIG}"))?;
    toml::from_str(&text).with_context(|| format!("parsing {CONFIG}"))
}

// -------------------------------------------------------------- manifest

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    sources: BTreeMap<String, FetchedSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FetchedSource {
    rev: String,
    pdf_count: usize,
    #[serde(default)]
    linked_ok: usize,
    #[serde(default)]
    link_failures: Vec<LinkFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LinkFailure {
    link: String,
    url: String,
    error: String,
}

fn load_manifest() -> Manifest {
    fs::read_to_string(MANIFEST)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_manifest(m: &Manifest) -> Result<()> {
    fs::create_dir_all(CORPUS_DIR)?;
    fs::write(MANIFEST, serde_json::to_string_pretty(m)?).context("writing manifest")
}

// ------------------------------------------------------------------- cli

#[derive(Parser)]
#[command(name = "cargo evals", about = "Evaluation corpus and scoring tooling")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Clone or download every source in evals.toml at its pinned commit (idempotent).
    Fetch {
        /// Only this source.
        name: Option<String>,
    },
    /// Show what is present, missing, or stale versus the pins.
    Status,
    /// Run a subset through the pipeline, write our outputs under
    /// evals/output/<preset>/, and compare their sizes with reference
    /// outputs under evals/reference/<name>/.
    Score {
        /// Our preset to score (default: each preset against the references
        /// whose name ends in it).
        #[arg(long)]
        preset: Option<String>,
        /// One reference directory name; default: every matching one.
        #[arg(long)]
        reference: Option<String>,
        /// Subset from evals.toml.
        #[arg(long, default_value = "scoring")]
        subset: String,
        /// Skip rendering; the minimum page SSIM column is then omitted.
        #[arg(long)]
        no_render: bool,
    },
    /// Write the synthetic probe PDFs to a directory, one per probe, for
    /// running through a reference tool.
    Probes {
        /// Output directory (created if needed).
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Fetch { name } => fetch(name.as_deref()),
        Cmd::Status => status(),
        Cmd::Score {
            preset,
            reference,
            subset,
            no_render,
        } => score::score(preset.as_deref(), reference.as_deref(), &subset, !no_render),
        Cmd::Probes { out } => probes(&out),
    }
}

// ---------------------------------------------------------------- probes

fn probes(out: &Path) -> Result<()> {
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let mut index = String::from(
        "# Probes\n\nOne variable each; see tests/probes.rs for the expected behavior.\n\n",
    );
    for probe in generators::all() {
        let mut doc = probe.doc;
        let bytes = generators::to_bytes(&mut doc);
        let path = out.join(format!("{}.pdf", probe.name));
        fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
        index.push_str(&format!(
            "- `{}.pdf`: {} ({} bytes)\n",
            probe.name,
            probe.about,
            bytes.len()
        ));
        println!(
            "{:<28} {:>8} bytes  {}",
            probe.name,
            bytes.len(),
            probe.about
        );
    }
    fs::write(out.join("README.md"), index)?;
    Ok(())
}

// ----------------------------------------------------------------- fetch

fn fetch(only: Option<&str>) -> Result<()> {
    let config = load_config()?;
    let mut manifest = load_manifest();
    for source in config
        .source
        .iter()
        .filter(|s| only.is_none_or(|n| n == s.name))
    {
        let fetched = fetch_source(source, manifest.sources.get(&source.name))?;
        manifest.sources.insert(source.name.clone(), fetched);
        save_manifest(&manifest)?;
    }
    if let Some(n) = only
        && !config.source.iter().any(|s| s.name == n)
    {
        bail!("no source named `{n}` in {CONFIG}");
    }
    Ok(())
}

fn fetch_source(source: &Source, previous: Option<&FetchedSource>) -> Result<FetchedSource> {
    let dest = Path::new(CORPUS_DIR).join(&source.name);
    let up_to_date = previous.is_some_and(|p| p.rev == source.rev) && dest.is_dir();
    if up_to_date {
        println!("{:<10} up to date at {}", source.name, &source.rev[..12]);
    } else {
        println!(
            "{:<10} fetching {} @ {}",
            source.name,
            source.git,
            &source.rev[..12]
        );
        clone_sparse(source, &dest)?;
    }
    let pdf_count = count_pdfs(&dest);
    let (linked_ok, link_failures) = if source.links {
        resolve_links(&dest)?
    } else {
        (0, Vec::new())
    };
    println!(
        "{:<10} {} PDFs, {} linked, {} link failures ({})",
        source.name,
        pdf_count,
        linked_ok,
        link_failures.len(),
        source.license
    );
    Ok(FetchedSource {
        rev: source.rev.clone(),
        pdf_count,
        linked_ok,
        link_failures,
    })
}

/// Shallow, sparse checkout of exactly `source.rev`. Re-creates the
/// directory from scratch so a stale or partial checkout cannot linger.
fn clone_sparse(source: &Source, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest).with_context(|| format!("removing {}", dest.display()))?;
    }
    fs::create_dir_all(dest)?;
    git(dest, &["init", "-q"])?;
    git(dest, &["remote", "add", "origin", &source.git])?;
    let sparse: Vec<&str> = source.paths.iter().map(String::as_str).collect();
    if sparse != ["."] {
        git(dest, &["sparse-checkout", "init", "--cone"])?;
        let mut args = vec!["sparse-checkout", "set"];
        args.extend(sparse);
        git(dest, &args)?;
    }
    git(
        dest,
        &["fetch", "-q", "--depth", "1", "origin", &source.rev],
    )?;
    git(dest, &["checkout", "-q", "FETCH_HEAD"])?;
    Ok(())
}

fn git(cwd: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}:\n{}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn count_pdfs(dir: &Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && has_ext(e.path(), "pdf"))
        .count()
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

// ----------------------------------------------------------------- links

/// pdf.js keeps some test files out of the repository as `<name>.pdf.link`
/// files whose content is a URL. Download each into LINKED_DIR, skipping
/// ones already present; failures are recorded, not fatal.
fn resolve_links(dir: &Path) -> Result<(usize, Vec<LinkFailure>)> {
    fs::create_dir_all(LINKED_DIR)?;
    let jobs: Vec<(PathBuf, PathBuf)> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && has_ext(e.path(), "link"))
        .map(|e| {
            let target = Path::new(LINKED_DIR).join(e.path().file_stem().unwrap_or_default());
            (e.into_path(), target)
        })
        .filter(|(_, target)| !target.exists())
        .collect();
    let already = count_pdfs(Path::new(LINKED_DIR));
    if jobs.is_empty() {
        return Ok((already, Vec::new()));
    }
    println!("{:<10} downloading {} linked files", "pdfjs", jobs.len());
    let failures = download_all(jobs);
    Ok((count_pdfs(Path::new(LINKED_DIR)), failures))
}

fn download_all(jobs: Vec<(PathBuf, PathBuf)>) -> Vec<LinkFailure> {
    let queue = Arc::new(Mutex::new(jobs));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .user_agent("compress-pdf-evals")
        .build()
        .into();
    let workers: Vec<_> = (0..DOWNLOAD_WORKERS)
        .map(|_| {
            let (queue, failures, agent) = (queue.clone(), failures.clone(), agent.clone());
            std::thread::spawn(move || worker(&queue, &failures, &agent))
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    let mut out = failures.lock().unwrap_or_else(|e| e.into_inner()).clone();
    out.sort_by(|a, b| a.link.cmp(&b.link));
    out
}

fn worker(
    queue: &Mutex<Vec<(PathBuf, PathBuf)>>,
    failures: &Mutex<Vec<LinkFailure>>,
    agent: &ureq::Agent,
) {
    loop {
        let job = queue.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let Some((link, target)) = job else { return };
        if let Err(e) = download_one(agent, &link, &target) {
            let url = fs::read_to_string(&link)
                .unwrap_or_default()
                .trim()
                .to_string();
            failures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(LinkFailure {
                    link: link.display().to_string(),
                    url,
                    error: e.to_string(),
                });
        }
    }
}

fn download_one(agent: &ureq::Agent, link: &Path, target: &Path) -> Result<()> {
    let url = fs::read_to_string(link)?.trim().to_string();
    let mut resp = agent.get(&url).call().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut bytes = Vec::new();
    resp.body_mut().as_reader().read_to_end(&mut bytes)?;
    if !bytes.starts_with(b"%PDF") {
        bail!("response is not a PDF ({} bytes)", bytes.len());
    }
    let tmp = target.with_extension("part");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, target)?;
    Ok(())
}

// ---------------------------------------------------------------- status

fn status() -> Result<()> {
    let config = load_config()?;
    let manifest = load_manifest();
    println!(
        "{:<10} {:<10} {:>6} {:>7} {:>8}  state",
        "source", "pinned", "pdfs", "linked", "failed"
    );
    for s in &config.source {
        let dir = Path::new(CORPUS_DIR).join(&s.name);
        let have = manifest.sources.get(&s.name);
        let state = match have {
            None => "missing",
            Some(_) if !dir.is_dir() => "missing (manifest only)",
            Some(h) if h.rev != s.rev => "stale",
            Some(_) => "ok",
        };
        println!(
            "{:<10} {:<10} {:>6} {:>7} {:>8}  {}",
            s.name,
            &s.rev[..10],
            have.map_or(0, |h| h.pdf_count),
            have.map_or(0, |h| h.linked_ok),
            have.map_or(0, |h| h.link_failures.len()),
            state
        );
    }
    subset_status(&config);
    Ok(())
}

/// One line per named subset: how many of its files are present on disk.
fn subset_status(config: &Config) {
    println!();
    println!(
        "{:<10} {:>6} {:>8} {:>10}",
        "subset", "files", "present", "size"
    );
    for (name, files) in &config.subsets {
        let present: Vec<u64> = files
            .iter()
            .filter_map(|f| fs::metadata(Path::new(CORPUS_DIR).join(f)).ok())
            .map(|m| m.len())
            .collect();
        println!(
            "{:<10} {:>6} {:>8} {:>7.1} MB",
            name,
            files.len(),
            present.len(),
            present.iter().sum::<u64>() as f64 / 1e6
        );
        for f in files {
            if !Path::new(CORPUS_DIR).join(f).is_file() {
                println!("           missing: {f}");
            }
        }
    }
}
