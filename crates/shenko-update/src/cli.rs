//! The deterministic `shenko update` command surface.
//!
//! Every command is inspectable and recoverable without a model: it reads
//! and writes the durable state directly. The subject/child route goes
//! through the same `UpdateService` via the `update.control` native handler.

use std::path::PathBuf;

use anyhow::{Result, bail};

use shenko_types::ids::EntityId;

use crate::build::BuildProfile;
use crate::handoff::{
    CandidateSource, PrepareRequest, UpdateService,
};
use crate::slots;
use crate::smoke::{SmokeCheckOutcome, SmokeReport, SMOKE_SCHEMA_VERSION};

/// The `shenko update` command surface. The binary owns clap parsing and
/// converts its own subcommand into this enum, so this crate carries no
/// argument-parsing dependency.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum UpdateCommand {
    /// Show the selector, the activation records and the slot inventory.
    Status,
    /// Build, copy, migrate and smoke a candidate generation.
    Prepare {
        /// The generation name the candidate becomes.
        generation: String,
        /// Build the candidate from this source directory.
        source_dir: Option<PathBuf>,
        /// Or accept an artifact that was built elsewhere.
        artifact: Option<PathBuf>,
        profile: String,
        target: Option<String>,
        extra_args: Option<String>,
    },
    /// Swap the selector to a prepared activation. This is the one
    /// linearization point of a hand-over.
    Activate {
        activation: EntityId,
        expected_selector_revision: u64,
    },
    /// Roll the control authority back to the previous generation.
    Rollback {
        activation: EntityId,
        expected_selector_revision: u64,
    },
    /// Deterministic recovery read. No model is required.
    Recover,
    /// Readiness check of a candidate slot, run inside the candidate
    /// program itself. Never starts a subject loop.
    Smoke {
        /// Print the machine-readable report.
        json: bool,
    },
}

pub async fn run(command: UpdateCommand, data_dir: PathBuf) -> Result<()> {
    match command {
        UpdateCommand::Smoke { json } => run_smoke(data_dir, json),
        _ => bail!("update mutations require the installation-owned binary control transport"),
    }
}

/// Shared executor for the authenticated runtime endpoint and offline CLI.
/// Its caller owns the real resource registry and installation authority.
pub async fn execute(command: UpdateCommand, service: &UpdateService, agent_id: EntityId) -> Result<serde_json::Value> {
    match command {
        UpdateCommand::Status => {
            let status = service.status(agent_id)?;
            Ok(serde_json::to_value(status)?)
        }
        UpdateCommand::Recover => {
            service.finish_committed_handoff(agent_id)?;
            let report = service.recover(agent_id)?;
            Ok(serde_json::to_value(report)?)
        }
        UpdateCommand::Prepare {
            generation,
            source_dir,
            artifact,
            profile,
            target,
            extra_args,
        } => {
            let source = match (source_dir, artifact) {
                (Some(source_dir), None) => CandidateSource::Build {
                    source_dir,
                    profile: match profile.as_str() {
                        "debug" => BuildProfile::Debug,
                        _ => BuildProfile::Release,
                    },
                    target,
                    extra_args: extra_args
                        .map(|args| {
                            args.split_whitespace().map(|part| part.to_string()).collect()
                        })
                        .unwrap_or_default(),
                },
                (None, Some(artifact)) => CandidateSource::Prebuilt { path: artifact },
                _ => bail!("prepare needs exactly one of --source-dir or --artifact"),
            };
            // Capture actual node or explicit branch-scoped authority from
            // the owning runtime, never from caller/model parameters.
            let record = service
                .prepare(PrepareRequest {
                    agent_id,
                    generation: shenko_types::ids::RuntimeGeneration(generation),
                    source,
                    primary_claim: service.current_primary_claim()?,
                })
                .await?;
            Ok(serde_json::to_value(record)?)
        }
        UpdateCommand::Activate {
            activation,
            expected_selector_revision,
        } => {
            let outcome = service
                .activate(activation, expected_selector_revision)
                .await?;
            Ok(serde_json::json!({
                "activation_id": outcome.record.activation_id,
                "commit": outcome.commit,
                "selector": outcome.selector,
                "fenced_mode": outcome.fenced_head.mode,
                "participants": outcome.participants,
            }))
        }
        UpdateCommand::Rollback {
            activation,
            expected_selector_revision,
        } => {
            let outcome = service
                .rollback(activation, expected_selector_revision)
                .await?;
            Ok(serde_json::json!({
                "activation_id": outcome.record.activation_id,
                "commit": outcome.commit,
                "selector": outcome.selector,
                "fenced_mode": outcome.fenced_head.mode,
            }))
        }
        UpdateCommand::Smoke { .. } => unreachable!("handled by run()"),
    }
}

/// The candidate readiness check, run inside the candidate program.
///
/// It opens the slot's own copy, checks the state and exits. It never
/// starts a subject loop, never sends a message and never drives a seat.
fn run_smoke(data_dir: PathBuf, json: bool) -> Result<()> {
    let manifest = slots::read_slot_manifest(&data_dir)?;
    let generation = manifest.generation;
    let cas_dir = PathBuf::from(manifest.cas_dir);
    let checks = local_checks(&data_dir, &cas_dir);
    let passed = checks.iter().all(|check| check.passed);
    let report = SmokeReport {
        schema_version: SMOKE_SCHEMA_VERSION,
        generation,
        checks,
        passed,
        exit_code: if passed { 0 } else { 1 },
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        launched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        for check in &report.checks {
            println!(
                "{} {}",
                if check.passed { "ok" } else { "FAIL" },
                check.name
            );
            if let Some(detail) = &check.detail {
                println!("    {detail}");
            }
        }
        println!("passed: {}", report.passed);
    }
    std::process::exit(report.exit_code);
}

/// The deterministic checks of one candidate copy.
fn local_checks(data_dir: &std::path::Path, cas_dir: &std::path::Path) -> Vec<SmokeCheckOutcome> {
    let mut checks = Vec::new();

    let store = match shenko_store::Store::open(data_dir.join(slots::SLOT_DB_FILENAME), cas_dir.to_path_buf()) {
        Ok(store) => {
            checks.push(SmokeCheckOutcome {
                name: "store_opens".into(),
                passed: true,
                detail: None,
            });
            Some(store)
        }
        Err(error) => {
            checks.push(SmokeCheckOutcome {
                name: "store_opens".into(),
                passed: false,
                detail: Some(format!("{error}")),
            });
            None
        }
    };

    if let Some(store) = store {
        match store.capabilities.wal_mode {
            true => checks.push(SmokeCheckOutcome {
                name: "wal_mode".into(),
                passed: true,
                detail: None,
            }),
            false => checks.push(SmokeCheckOutcome {
                name: "wal_mode".into(),
                passed: false,
                detail: Some("the store is not in WAL mode".to_string()),
            }),
        }
        let agent = store.get_existing_agent_id();
        match agent {
            Ok(Some(agent_id)) => {
                checks.push(SmokeCheckOutcome {
                    name: "subject_identity".into(),
                    passed: true,
                    detail: None,
                });
                match store.load_subject(agent_id) {
                    Ok((head, step)) => {
                        let binding_ok = step.step_id == head.next_step_id
                            && step.branch_id == head.branch_id;
                        checks.push(SmokeCheckOutcome {
                            name: "subject_head_and_step".into(),
                            passed: binding_ok,
                            detail: if binding_ok {
                                None
                            } else {
                                Some("the planned step does not match the branch head".to_string())
                            },
                        });
                    }
                    Err(error) => checks.push(SmokeCheckOutcome {
                        name: "subject_head_and_step".into(),
                        passed: false,
                        detail: Some(format!("{error}")),
                    }),
                }
                match store.activation_selector(agent_id) {
                    Ok(Some(selector)) => checks.push(SmokeCheckOutcome {
                        name: "activation_selector".into(),
                        passed: true,
                        detail: Some(format!(
                            "revision {} epoch {} generation {}",
                            selector.selector_revision,
                            selector.active_epoch,
                            selector.generation.0
                        )),
                    }),
                    Ok(None) => checks.push(SmokeCheckOutcome {
                        name: "activation_selector".into(),
                        passed: false,
                        detail: Some("the candidate has no activation selector".to_string()),
                    }),
                    Err(error) => checks.push(SmokeCheckOutcome {
                        name: "activation_selector".into(),
                        passed: false,
                        detail: Some(format!("{error}")),
                    }),
                }
            }
            Ok(None) => checks.push(SmokeCheckOutcome {
                name: "subject_identity".into(),
                passed: false,
                detail: Some("no initialized subject in this copy".to_string()),
            }),
            Err(error) => checks.push(SmokeCheckOutcome {
                name: "subject_identity".into(),
                passed: false,
                detail: Some(format!("{error}")),
            }),
        }
    }

    if data_dir.join("cas").is_dir() || cas_dir.is_dir() {
        checks.push(SmokeCheckOutcome {
            name: "content_store".into(),
            passed: true,
            detail: None,
        });
    } else {
        checks.push(SmokeCheckOutcome {
            name: "content_store".into(),
            passed: false,
            detail: Some("no content store is reachable".to_string()),
        });
    }
    if data_dir.join("home").is_dir() {
        checks.push(SmokeCheckOutcome {
            name: "managed_home".into(),
            passed: true,
            detail: None,
        });
    } else {
        checks.push(SmokeCheckOutcome {
            name: "managed_home".into(),
            passed: false,
            detail: Some("no managed home in this copy".to_string()),
        });
    }

    checks
}
