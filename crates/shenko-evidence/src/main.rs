//! `shenko-evidence` — build the SPEC obligation report for one candidate,
//! and produce the corrections that the report already understands.
//!
//! The report surface is the executable entry documented in
//! `docs/specs/verification.md`. It only reads retained inputs and prints what
//! is still owed: it never decides that a requirement is satisfied because a
//! file exists, and it never gates on an unmet count (the completion judgment
//! belongs to the deliverable report, not to this command's exit code).
//!
//! The withdrawal surface is the producer for the report's existing
//! `withdraws`/`withdrawn` vocabulary. It writes one new digest-bound record
//! and never rewrites the record it corrects.
//!
//! Usage:
//!   shenko-evidence --spec SPEC.md --evidence-dir docs/evidence \
//!       [--candidate-commit <sha>] [--json build/spec-obligation-report.json] \
//!       [--long-run <report.json>=<sha256>]...
//!   shenko-evidence --evidence-dir docs/evidence --withdraw <evidence-id> \
//!       --reason <text> --source-sha256 <sha256> \
//!       [--superseded-by <evidence-id>] [--json <outcome.json>]
//!
//! `--long-run` binds a retained 7/30-day sampler report by the digest the
//! caller expects it to have. Without it the report states
//! `duration_status: unobserved`, which is the honest state of a candidate
//! that has not actually run for its declared windows.

use std::path::PathBuf;
use std::process::ExitCode;

use shenko_evidence::{CorrectionRequest, LongRunInput, ReportInputs, analyze, write_correction};

const USAGE: &str = "usage: shenko-evidence --spec <SPEC.md> --evidence-dir <dir> \
[--candidate-commit <sha>] [--json <out.json>] [--long-run <report.json>=<sha256>]... \
| shenko-evidence --evidence-dir <dir> --withdraw <evidence-id> --reason <text> \
--source-sha256 <sha256> [--superseded-by <evidence-id>] [--json <out.json>]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
enum Command {
    Report(ReportArgs),
    Withdraw(WithdrawArgs),
}

#[derive(Debug)]
struct ReportArgs {
    spec: PathBuf,
    evidence_dir: PathBuf,
    candidate_commit: Option<String>,
    json: Option<PathBuf>,
    /// `(path, expected sha256)` for each retained long-run report.
    long_run: Vec<(PathBuf, String)>,
}

#[derive(Debug)]
struct WithdrawArgs {
    evidence_dir: PathBuf,
    evidence_id: String,
    reason: String,
    source_sha256: String,
    superseded_by: Option<String>,
    json: Option<PathBuf>,
}

fn parse_args() -> Result<Command, String> {
    let mut spec = None;
    let mut evidence_dir = None;
    let mut candidate_commit = None;
    let mut json = None;
    let mut long_run = Vec::new();
    let mut withdraw = None;
    let mut reason = None;
    let mut source_sha256 = None;
    let mut superseded_by = None;
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || {
            argv.next()
                .ok_or_else(|| format!("{flag} needs a value; {USAGE}"))
        };
        match flag.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--spec" => spec = Some(PathBuf::from(value()?)),
            "--evidence-dir" => evidence_dir = Some(PathBuf::from(value()?)),
            "--candidate-commit" => candidate_commit = Some(value()?),
            "--json" => json = Some(PathBuf::from(value()?)),
            "--long-run" => {
                let binding = value()?;
                let (path, sha256) = binding
                    .rsplit_once('=')
                    .ok_or_else(|| format!("--long-run expects PATH=SHA256; {USAGE}"))?;
                long_run.push((PathBuf::from(path), sha256.to_string()));
            }
            "--withdraw" => withdraw = Some(value()?),
            "--reason" => reason = Some(value()?),
            "--source-sha256" => source_sha256 = Some(value()?),
            "--superseded-by" => superseded_by = Some(value()?),
            other => return Err(format!("unknown argument {other}; {USAGE}")),
        }
    }
    if let Some(evidence_id) = withdraw {
        if spec.is_some() || candidate_commit.is_some() || !long_run.is_empty() {
            return Err(format!(
                "--withdraw does not take --spec, --candidate-commit or --long-run; {USAGE}"
            ));
        }
        return Ok(Command::Withdraw(WithdrawArgs {
            evidence_dir: evidence_dir
                .ok_or_else(|| format!("--withdraw needs --evidence-dir; {USAGE}"))?,
            evidence_id,
            reason: reason.ok_or_else(|| format!("--withdraw needs --reason; {USAGE}"))?,
            source_sha256: source_sha256
                .ok_or_else(|| format!("--withdraw needs --source-sha256; {USAGE}"))?,
            superseded_by,
            json,
        }));
    }
    if reason.is_some() || source_sha256.is_some() || superseded_by.is_some() {
        return Err(format!(
            "--reason, --source-sha256 and --superseded-by require --withdraw; {USAGE}"
        ));
    }
    Ok(Command::Report(ReportArgs {
        // A report must name the SPEC text and the evidence it read; neither
        // is guessed from the working directory.
        spec: spec.ok_or_else(|| USAGE.to_string())?,
        evidence_dir: evidence_dir.ok_or_else(|| USAGE.to_string())?,
        candidate_commit,
        json,
        long_run,
    }))
}

fn run() -> Result<(), String> {
    match parse_args()? {
        Command::Report(args) => run_report(&args),
        Command::Withdraw(args) => run_withdraw(&args),
    }
}

fn run_report(args: &ReportArgs) -> Result<(), String> {
    let spec_text = std::fs::read_to_string(&args.spec)
        .map_err(|error| format!("cannot read {}: {error}", args.spec.display()))?;
    let long_run: Vec<LongRunInput<'_>> = args
        .long_run
        .iter()
        .map(|(path, sha256)| LongRunInput {
            path: path.as_path(),
            sha256: sha256.as_str(),
        })
        .collect();
    let analysis = analyze(&ReportInputs {
        spec_text: &spec_text,
        evidence_dir: &args.evidence_dir,
        candidate_commit: args.candidate_commit.as_deref(),
        long_run: &long_run,
    })
    .map_err(|error| format!("SPEC index error: {error:?}"))?;
    println!("{}", analysis.summary());
    // A rejected report is not silence: the operator asked for duration
    // evidence and has to see why these bytes were not admitted, otherwise
    // `unobserved` reads as "no report was offered".
    for (path, reason) in &analysis.long_run.rejections {
        eprintln!("warning: long-run report {path} was not admitted: {reason}");
    }
    if let Some(path) = &args.json {
        let bytes =
            serde_json::to_vec_pretty(&analysis.to_json()).map_err(|error| error.to_string())?;
        std::fs::write(path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("report written to {}", path.display());
    }
    Ok(())
}

fn run_withdraw(args: &WithdrawArgs) -> Result<(), String> {
    let outcome = write_correction(&CorrectionRequest {
        evidence_dir: &args.evidence_dir,
        evidence_id: &args.evidence_id,
        reason: &args.reason,
        source_sha256: &args.source_sha256,
        superseded_by: args.superseded_by.as_deref(),
    })?;
    println!(
        "correction {} withdraws {} (sha256 {})",
        outcome.correction_id, outcome.withdraws, outcome.withdraws_sha256
    );
    if let Some(replacement) = &outcome.superseded_by {
        println!("superseded by {replacement}");
    }
    println!("record written to {}", outcome.correction_path.display());
    println!("original retained at {}", outcome.source_path.display());
    if let Some(path) = &args.json {
        let bytes = serde_json::to_vec_pretty(&outcome.to_json())
            .map_err(|error| format!("cannot encode outcome: {error}"))?;
        std::fs::write(path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        println!("outcome written to {}", path.display());
    }
    Ok(())
}
