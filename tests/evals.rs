//! Corpus evals: one test per PDF per preset over the fetched corpus.
//!
//! Selection (see AGENTS.md, "Testing"):
//! - `EVALS_RENDER=1` also renders every page before and after and fails a
//!   trial whose pages fall below the preset's similarity floor.
//! - `EVALS_SUBSET` names a subset from `evals.toml` (default `quick`) or
//!   `full` for every PDF under `evals/corpus/`.
//! - `EVALS_PRESETS` is a comma list of presets (default: all three for a
//!   named subset, `standard` for `full`).
//! - Files outside the selected subset, and files listed in
//!   `evals-expectations.toml`, are registered as ignored so that
//!   `--ignored` can still run them.
//!
//! Per file the invariants are: the pipeline completes; the output is not
//! larger than the input; the output re-parses with the same page count;
//! no image row in the report grew.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use std::{env, fs, thread};

use compress_pdf::config::{Config, Preset};
use compress_pdf::error::Refusal;
use compress_pdf::pipeline;
use compress_pdf::report::Report;
use compress_pdf::verify;
use libtest_mimic::{Arguments, Failed, Trial};
use lopdf::Document;
use serde::Deserialize;

const CORPUS: &str = "evals/corpus";
const ALL_PRESETS: [Preset; 3] = [Preset::Less, Preset::Standard, Preset::More];

#[derive(Deserialize)]
struct EvalsToml {
    subsets: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize, Default)]
struct Expectations {
    #[serde(default)]
    expect: Vec<Expect>,
}

#[derive(Deserialize)]
struct Expect {
    file: String,
    preset: Option<String>,
    #[allow(dead_code)]
    reason: String,
}

fn main() {
    let args = Arguments::from_args();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let corpus = root.join(CORPUS);
    let trials = if corpus.is_dir() {
        build_trials(root, &corpus)
    } else {
        eprintln!(
            "evals corpus not found at {}; run `cargo evals fetch` (no tests registered)",
            corpus.display()
        );
        Vec::new()
    };
    libtest_mimic::run(&args, trials).exit();
}

fn build_trials(root: &Path, corpus: &Path) -> Vec<Trial> {
    let files = list_pdfs(corpus);
    let subset_name = env::var("EVALS_SUBSET").unwrap_or_else(|_| "quick".into());
    let selected = selected_files(root, &subset_name, &files);
    let presets = presets_from_env(&subset_name);
    let expectations = load_expectations(root);
    let mut trials = Vec::with_capacity(files.len() * presets.len());
    for file in &files {
        let in_subset = selected.contains(file);
        let presets_for_file: &[Preset] = if in_subset {
            &presets
        } else {
            &[Preset::Standard]
        };
        for &preset in presets_for_file {
            let expected_failure = expectations.iter().any(|e| e.matches(file, preset));
            let path = corpus.join(file);
            let name = format!("{file}::{}", preset_name(preset));
            trials.push(
                Trial::test(name, move || run_one(&path, preset))
                    .with_ignored_flag(!in_subset || expected_failure),
            );
        }
    }
    trials
}

fn list_pdfs(corpus: &Path) -> Vec<String> {
    let mut files: Vec<String> = walkdir::WalkDir::new(corpus)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("pdf"))
        })
        .filter_map(|e| {
            e.path()
                .strip_prefix(corpus)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .collect();
    files.sort();
    files
}

fn selected_files(root: &Path, subset: &str, all: &[String]) -> HashSet<String> {
    if subset == "full" {
        return all.iter().cloned().collect();
    }
    let text = fs::read_to_string(root.join("evals.toml")).expect("evals.toml is readable");
    let cfg: EvalsToml = toml::from_str(&text).expect("evals.toml parses");
    let files = cfg
        .subsets
        .get(subset)
        .unwrap_or_else(|| panic!("no subset `{subset}` in evals.toml"));
    files.iter().cloned().collect()
}

fn presets_from_env(subset: &str) -> Vec<Preset> {
    match env::var("EVALS_PRESETS") {
        Ok(list) => list.split(',').map(|s| parse_preset(s.trim())).collect(),
        Err(_) if subset == "full" => vec![Preset::Standard],
        Err(_) => ALL_PRESETS.to_vec(),
    }
}

fn load_expectations(root: &Path) -> Vec<Expect> {
    let path = root.join("evals-expectations.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let parsed: Expectations = toml::from_str(&text).expect("evals-expectations.toml parses");
    parsed.expect
}

impl Expect {
    fn matches(&self, file: &str, preset: Preset) -> bool {
        self.file == file
            && self
                .preset
                .as_deref()
                .is_none_or(|p| p == preset_name(preset))
    }
}

fn preset_name(p: Preset) -> &'static str {
    match p {
        Preset::Less => "less",
        Preset::Standard => "standard",
        Preset::More => "more",
        _ => "unknown",
    }
}

fn parse_preset(s: &str) -> Preset {
    match s {
        "less" => Preset::Less,
        "standard" => Preset::Standard,
        "more" => Preset::More,
        other => panic!("unknown preset `{other}` in EVALS_PRESETS"),
    }
}

// ------------------------------------------------------------- one trial

/// Per-trial wall-clock limit. A file that exceeds it is a failure in its
/// own right (rule: pathological inputs must not hang the tool). Trials run
/// in parallel, so the limit is generous: the largest corpus files (tens of
/// megabytes of JPX images) take close to a minute alone. Override with
/// `EVALS_TIMEOUT_SECS`.
fn trial_timeout() -> Duration {
    let secs = env::var("EVALS_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180);
    Duration::from_secs(secs)
}

/// Runs the trial on a helper thread so a hang becomes a timeout failure
/// instead of stalling the whole suite. The thread is leaked on timeout;
/// the process exits when the suite ends.
/// Deeply nested objects recurse deeply in both parsers; give trials room.
const TRIAL_STACK_BYTES: usize = 256 * 1024 * 1024;

fn run_one(path: &Path, preset: Preset) -> Result<(), Failed> {
    let (tx, rx) = mpsc::channel();
    let path = path.to_path_buf();
    thread::Builder::new()
        .stack_size(TRIAL_STACK_BYTES)
        .spawn(move || {
            let _ = tx.send(run_one_inner(&path, preset));
        })
        .map_err(|e| format!("spawn: {e}"))?;
    match rx.recv_timeout(trial_timeout()) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(format!("timed out after {:?}", trial_timeout()).into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("trial thread panicked".into()),
    }
}

/// `EVALS_RENDER=1` adds the visual level: every page of input and output
/// rendered and compared, failing below the preset's floor.
fn render_enabled() -> bool {
    env::var("EVALS_RENDER").is_ok_and(|v| v == "1")
}

fn run_one_inner(path: &Path, preset: Preset) -> Result<(), Failed> {
    let input = fs::read(path).map_err(|e| format!("read: {e}"))?;
    let mut doc = Document::load_mem(&input).map_err(|e| format!("input does not parse: {e}"))?;
    let pages_in = doc.get_pages().len();

    let mut report = Report::new(input.len());
    if doc.trailer.has(b"Encrypt") {
        // Input that needs a password is out of scope: the expected outcome
        // is a clean refusal, not an output file. Encrypted input that opens
        // without one was decrypted on load and is compressed like any other.
        return match pipeline::run(&mut doc, &Config::preset(preset), &mut report) {
            Err(Refusal::PasswordRequired) => Ok(()),
            Err(e) => Err(format!("password-protected input: wrong error: {e:#}").into()),
            Ok(()) => Err("password-protected input was not refused".into()),
        };
    }
    match pipeline::run(&mut doc, &Config::preset(preset), &mut report) {
        // A page tree with kids the parser could not load is refused by
        // design (repair is out of scope); that refusal is the expected
        // outcome, the same as for encrypted input.
        Err(
            Refusal::DamagedPageTree | Refusal::DamagedResources | Refusal::UndecryptedCryptFilters,
        ) => return Ok(()),
        Err(e) => return Err(format!("pipeline failed: {e:#}").into()),
        Ok(()) => {}
    }

    let output = pipeline::serialize(&mut doc, &input, &mut report)
        .map_err(|e| format!("serialize failed: {e:#}"))?;
    check_output(&input, &output, pages_in, &report)?;
    if render_enabled() && output != input {
        // An input the rasterizer cannot open is its problem, not ours.
        let rendered = match verify::render::compare(&input, &output, preset) {
            Err(e) if e.starts_with("input does not load") => return Ok(()),
            other => other.map_err(|e| format!("render: {e}"))?,
        };
        if !rendered.below_floor().is_empty() {
            return Err(format!("{rendered}").into());
        }
    }
    Ok(())
}

fn check_output(
    input: &[u8],
    output: &[u8],
    pages_in: usize,
    report: &Report,
) -> Result<(), Failed> {
    if output.len() > input.len() {
        return Err(format!(
            "output grew: {} -> {} bytes (+{})",
            input.len(),
            output.len(),
            output.len() - input.len()
        )
        .into());
    }
    let doc = Document::load_mem(output).map_err(|e| format!("output does not re-parse: {e}"))?;
    let pages_out = doc.get_pages().len();
    if pages_out != pages_in {
        return Err(format!("page count changed: {pages_in} -> {pages_out}").into());
    }
    for row in &report.images {
        if row.bytes_out > row.bytes_in {
            return Err(format!(
                "image {} {} grew: {} -> {} bytes",
                row.object.0, row.object.1, row.bytes_in, row.bytes_out
            )
            .into());
        }
    }
    if output != input {
        let v = verify::verify(output, pages_in);
        if !v.is_ok() {
            // Damage the input already had is not ours; only new problems fail.
            let baseline = verify::verify(input, pages_in);
            let regressions = v.regressions_from(&baseline);
            if !regressions.is_empty() {
                return Err(format!("verification regressions {regressions:?}\n{v}").into());
            }
        }
    }
    Ok(())
}
