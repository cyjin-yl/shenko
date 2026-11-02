//! Life identity adapter — SPEC §15.2.1, R102.
//!
//! Reuses the *actual* schemas of the private `cyjin-yl/life` project
//! (pinned by SPEC at `618f0dcb`) instead of inventing a parallel numbering:
//!
//! * the people catalog gives every person a stable slug id in front matter
//!   (`person-advisor-wangxu`, …) — never a real name as the key;
//! * the voiceprint registry documents `rank_id` (cross-recording candidate
//!   speaker id), `cluster_id` (recording-local, may change on a re-run),
//!   `candidates` (each with `person_id`, status `confirmed|suspected|
//!   unresolved`, evidence and confidence), `confirmed_identity` (explicit
//!   user confirmation only) and a monotonically increasing `model_revision`.
//!
//! The adapter imports *identifiers and statuses* from the owner's own
//! checkout. It never reads the encrypted batches, embeddings or audio, and
//! its typed inputs reject unknown fields, so private material cannot be
//! copied into this ledger by accident.
use anyhow::{Context as _, Result, bail, ensure};
use shenko_store::Store;
use shenko_types::contacts::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Outcome of one import run, reported to the CLI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LifeImportReport {
    pub persons_seen: usize,
    pub persons_created: usize,
    pub persons_updated: usize,
    pub claims_admitted: usize,
    pub claims_replayed: usize,
    pub ranks_seen: usize,
    pub ranks_admitted: usize,
    pub ranks_replayed: usize,
    pub links: usize,
    pub skipped: Vec<String>,
}

impl LifeImportReport {
    fn new() -> Self {
        Self {
            persons_seen: 0,
            persons_created: 0,
            persons_updated: 0,
            claims_admitted: 0,
            claims_replayed: 0,
            ranks_seen: 0,
            ranks_admitted: 0,
            ranks_replayed: 0,
            links: 0,
            skipped: Vec::new(),
        }
    }
}

fn is_private(path: &Path) -> bool {
    path.components().any(|component| {
        let text = component.as_os_str().to_string_lossy();
        text == "encrypted" || text == "sources" || text == ".private-work"
    })
}

fn parse_frontmatter(markdown: &str) -> (Vec<(String, String)>, String) {
    let mut fields = Vec::new();
    let mut heading = String::new();
    let mut lines = markdown.lines();
    let mut in_frontmatter = false;
    if lines.next().is_some_and(|line| line.trim() == "---") {
        in_frontmatter = true;
    }
    if in_frontmatter {
        for line in lines.by_ref() {
            let line = line.trim_end();
            if line.trim() == "---" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                fields.push((key.trim().to_owned(), value.trim().to_owned()));
            }
        }
    }
    for line in lines {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("# ") {
            heading = rest.trim().to_owned();
            break;
        }
        if !line.is_empty() {
            break;
        }
    }
    (fields, heading)
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// Parse one Life people-catalog markdown entry.
pub fn parse_person_entry(path: &Path, markdown: &str) -> Result<LifePersonEntry> {
    let (fields, heading) = parse_frontmatter(markdown);
    let life_person_id = field(&fields, "id")
        .ok_or_else(|| anyhow::anyhow!("{}: catalog entry has no id", path.display()))?
        .to_owned();
    ensure!(
        field(&fields, "entity_type") == Some("person"),
        "{}: catalog entry is not a person",
        path.display()
    );
    Ok(LifePersonEntry {
        evidence_ref: Some(path.display().to_string()),
        life_person_id,
        display_name: field(&fields, "name").map(str::to_owned),
        catalog_heading: Some(heading).filter(|h| !h.is_empty()),
        relation_to_user: field(&fields, "relation_to_user").map(str::to_owned),
        privacy: field(&fields, "privacy").map(str::to_owned),
        profile_status: field(&fields, "profile_status").map(str::to_owned),
        updated: field(&fields, "updated").map(str::to_owned),
    })
}

/// Read every people-catalog entry directly under `catalog/people`.
///
/// `reference/` and `sources/` subdirectories (and anything named
/// `encrypted`) are never touched.
pub fn read_people_catalog(catalog_dir: &Path) -> Result<Vec<LifePersonEntry>> {
    ensure!(
        catalog_dir.is_dir(),
        "people catalog directory {} does not exist",
        catalog_dir.display()
    );
    let mut entries = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(catalog_dir)
        .with_context(|| format!("reading {}", catalog_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter(|path| path.file_name().is_some_and(|name| name != "README.md"))
        .collect();
    files.sort();
    for path in files {
        if is_private(&path) || !path.is_file() {
            continue;
        }
        let markdown = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        entries.push(parse_person_entry(&path, &markdown)?);
    }
    Ok(entries)
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum LifeRanksFile {
    One(LifeRankRecord),
    Many(Vec<LifeRankRecord>),
    Wrapped { ranks: Vec<LifeRankRecord> },
}

/// Read a Life voiceprint rank export. The typed schema rejects unknown
/// fields, so a document that carries private material (embeddings, audio
/// references) fails validation instead of being silently copied.
pub fn read_ranks_export(path: &Path) -> Result<Vec<LifeRankRecord>> {
    ensure!(!is_private(path), "refusing to read a private path");
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let file: LifeRanksFile = serde_json::from_str(&text)
        .with_context(|| format!("{}: not a valid Life ranks export", path.display()))?;
    let records = match file {
        LifeRanksFile::One(one) => vec![one],
        LifeRanksFile::Many(many) => many,
        LifeRanksFile::Wrapped { ranks } => ranks,
    };
    for record in &records {
        ensure!(
            !record.model_revision.trim().is_empty(),
            "{}: rank {} has no model_revision",
            path.display(),
            record.rank_id
        );
        if let Some(confirmed) = &record.confirmed_identity {
            let candidate = record
                .candidates
                .iter()
                .find(|c| c.life_person_id.as_deref() == Some(confirmed.as_str()));
            match candidate {
                Some(c) if c.status == LifeCandidateStatus::Confirmed => {}
                _ => bail!(
                    "{}: confirmed_identity '{}' has no confirmed candidate; \
                     treating a suspected or unresolved candidate as confirmed is forbidden",
                    path.display(),
                    confirmed
                ),
            }
        }
    }
    Ok(records)
}

/// Imports Life identity material into the person directory.
pub struct LifeIdentityAdapter {
    store: Arc<Store>,
}

impl LifeIdentityAdapter {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Import the people catalog. Every person keeps the deterministic id
    /// derived from the catalog slug, so re-imports update rather than
    /// duplicate. Nothing overwrites the subject's own notes: existing
    /// profile claims gain a new revision with the catalog's facts, and the
    /// history stays readable.
    pub fn import_people(
        &self,
        agent_id: shenko_types::ids::EntityId,
        entries: &[LifePersonEntry],
    ) -> Result<LifeImportReport> {
        let mut report = LifeImportReport::new();
        for entry in entries {
            report.persons_seen += 1;
            let person_id = life_person_id(&entry.life_person_id);
            let provenance = Provenance {
                source: ClaimSource::LifeAdapter {
                    life_person_id: Some(entry.life_person_id.clone()),
                    model_revision: None,
                },
                evidence_ref: entry.evidence_ref.clone(),
                observed_at_ms: None,
            };

            let profile = PersonClaimInput {
                person_id,
                kind: PersonClaimKind::Profile,
                claim_key: "profile".into(),
                payload: serde_json::json!({
                    "display_name": entry.display_name,
                    "life_person_id": entry.life_person_id,
                }),
                provenance: provenance.clone(),
                confidence: Confidence::unknown(),
                state: PersonClaimState::Active,
                scope: ClaimScope {
                    kind: ClaimScopeKind::Catalog,
                    reference: entry.evidence_ref.clone(),
                },
                slot_id: None,
            };
            match self.store.admit_person_claim(agent_id, &profile) {
                Ok(receipt) => {
                    if receipt.replayed {
                        report.claims_replayed += 1;
                    } else {
                        report.claims_admitted += 1;
                        if receipt.revision == 1 {
                            report.persons_created += 1;
                        } else {
                            report.persons_updated += 1;
                        }
                    }
                }
                Err(error) => {
                    report
                        .skipped
                        .push(format!("{}: {error:#}", entry.life_person_id));
                    continue;
                }
            }

            let external = PersonClaimInput {
                person_id,
                kind: PersonClaimKind::ExternalIdentity,
                claim_key: format!("life|person|{}", entry.life_person_id),
                payload: serde_json::to_value(entry)?,
                provenance: provenance.clone(),
                // The catalog is authoritative for its own ids.
                confidence: Confidence::verified(),
                state: PersonClaimState::Confirmed,
                scope: ClaimScope {
                    kind: ClaimScopeKind::Catalog,
                    reference: entry.evidence_ref.clone(),
                },
                slot_id: Some(global_slot_id(
                    agent_id,
                    PersonClaimKind::ExternalIdentity,
                    &format!("life|person|{}", entry.life_person_id),
                )),
            };
            match self.store.admit_person_claim(agent_id, &external) {
                Ok(receipt) if receipt.replayed => report.claims_replayed += 1,
                Ok(_) => report.claims_admitted += 1,
                Err(error) => report
                    .skipped
                    .push(format!("{}: {error:#}", entry.life_person_id)),
            }

            if let Some(relation) = &entry.relation_to_user {
                let relation_claim = PersonClaimInput {
                    person_id,
                    kind: PersonClaimKind::Relationship,
                    claim_key: format!("relationship|life|{relation}"),
                    payload: serde_json::json!({
                        "relation": relation,
                        "source": "life-catalog",
                    }),
                    provenance,
                    confidence: Confidence::reported(),
                    state: PersonClaimState::Active,
                    scope: ClaimScope {
                        kind: ClaimScopeKind::Catalog,
                        reference: entry.evidence_ref.clone(),
                    },
                    slot_id: Some(person_slot_id(
                        agent_id,
                        PersonClaimKind::Relationship,
                        person_id,
                        &format!("life|{relation}"),
                    )),
                };
                match self.store.admit_person_claim(agent_id, &relation_claim) {
                    Ok(receipt) if receipt.replayed => report.claims_replayed += 1,
                    Ok(_) => report.claims_admitted += 1,
                    Err(error) => report
                        .skipped
                        .push(format!("{}: {error:#}", entry.life_person_id)),
                }
            }
        }
        Ok(report)
    }

    /// Import voiceprint rank records. Each keeps its recording scope and
    /// model revision; candidates become candidate links, and only an
    /// explicit confirmation produces a confirmed link (§15.2.1).
    pub fn import_ranks(
        &self,
        agent_id: shenko_types::ids::EntityId,
        records: &[LifeRankRecord],
    ) -> Result<LifeImportReport> {
        let mut report = LifeImportReport::new();
        for record in records {
            report.ranks_seen += 1;
            match self.store.admit_life_rank(agent_id, record) {
                Ok(receipt) => {
                    report.links += receipt.links;
                    if receipt.replayed {
                        report.ranks_replayed += 1;
                    } else {
                        report.ranks_admitted += 1;
                    }
                }
                Err(error) => report.skipped.push(format!(
                    "{} {}: {error:#}",
                    record.recording_scope, record.rank_id
                )),
            }
        }
        Ok(report)
    }
}
