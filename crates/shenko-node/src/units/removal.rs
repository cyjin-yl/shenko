//! Transactional version withdrawal and append-only removal facts.
//!
//! Dependency and live-trial ownership checks share the removal transaction;
//! artifact bytes, history and evidence remain addressable after withdrawal.

use anyhow::{Result, ensure};
use crate::Node;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use super::codec::MAX_TEXT_BYTES;
use super::installation::{StoredVersion, now, string_list};
use super::materialization::{MaterializationFact, RetainedPath, retire_package_directory};
use super::resolution::dependent_edges;

/// One retained version to make current while another is withdrawn.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UnitActivation {
    pub revision: String,
    pub artifact_digest: Digest,
}

/// The durable fact of one withdrawn unit version.
///
/// A removal is a lifecycle fact, not an erasure: the version's bytes remain
/// in the object store and its trials, observations and adoption stay
/// readable. This row explains why a previously installed identity is gone
/// from the live tables.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovedVersionFact {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    pub removed_at: String,
    pub reason: String,
    /// Whether the withdrawn version was the one this node offered.
    pub was_current: bool,
    /// Whether the withdrawn version was enabled before removal.
    pub was_enabled: bool,
    /// The version made current in the same transaction, if any.
    pub replacement: Option<UnitActivation>,
    /// The materialization record the version carried.
    pub materialization: Option<MaterializationFact>,
    /// Directories this uninstall actually removed.
    pub files_retired: Vec<String>,
    /// Paths it left in place, with the reason.
    pub files_retained: Vec<RetainedPath>,
    pub artifact_retained: bool,
    pub evidence_retained: bool,
    /// The adoption decision the archived descriptor carried, so a caller can
    /// see that an earlier judgement existed after the version left the live
    /// tables.
    pub adoption: Option<Value>,
}

impl Node {
    /// The durable removal facts of one unit's versions.
    pub fn removed_unit_facts(&self, unit_id: &str) -> Result<Vec<RemovedVersionFact>> {
        let rows: Vec<(String, String, String, String, String, String)> = self
            .conn
            .prepare(
                "SELECT unit_id,revision,artifact_digest,removed_at,reason,data \
                 FROM unit_removals WHERE unit_id=?1 ORDER BY seq",
            )?
            .query_map([unit_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut facts = Vec::new();
        for (unit_id, revision, digest, removed_at, reason, data) in rows {
            let stored: Value = serde_json::from_str(&data)?;
            facts.push(RemovedVersionFact {
                unit_id,
                revision,
                artifact_digest: Digest(digest),
                removed_at,
                reason,
                was_current: stored.get("was_current").and_then(Value::as_bool).unwrap_or(false),
                was_enabled: stored.get("was_enabled").and_then(Value::as_bool).unwrap_or(false),
                replacement: stored
                    .get("replacement")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?,
                materialization: stored
                    .get("materialization")
                    .filter(|value| !value.is_null())
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?,
                files_retired: string_list(stored.get("files_retired")),
                files_retained: stored
                    .get("files_retained")
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?
                    .unwrap_or_default(),
                artifact_retained: stored
                    .get("artifact_retained")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                evidence_retained: stored
                    .get("evidence_retained")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                adoption: stored.get("adoption").filter(|value| !value.is_null()).cloned(),
            });
        }
        Ok(facts)
    }

    /// Withdraw one installed version from this node.
    ///
    /// The removal is one immediate transaction: the version's closure
    /// dependents, a still-running trial and the requested successor are
    /// checked under the write lock, the version, its edges and its
    /// materialization record leave the live tables, the archived rows keep
    /// the descriptor and its evidence, and one append-only removal fact
    /// explains the gap. The package directory is retired inside the same
    /// transaction; a directory this node did not canonicalize for exactly
    /// this version is reported, never deleted. A crash after the directory
    /// is gone but before the commit leaves the record in place, and the next
    /// execution re-verifies the artifact instead of trusting it.
    pub(super) fn unit_uninstall(
        &mut self,
        unit_id: &str,
        revision: &str,
        artifact_digest: Option<&Digest>,
        reason: &str,
        activate: Option<&UnitActivation>,
    ) -> Result<Value> {
        ensure!(
            !unit_id.is_empty() && unit_id.len() <= 128,
            "invalid unit identity"
        );
        ensure!(
            !revision.is_empty() && revision.len() <= 128,
            "invalid unit revision"
        );
        ensure!(
            !reason.trim().is_empty() && reason.len() <= MAX_TEXT_BYTES,
            "an uninstall reason is required and bounded"
        );
        let removed_at = now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let digests: Vec<String> = tx
            .prepare(
                "SELECT artifact_digest FROM unit_versions \
                 WHERE unit_id=?1 AND revision=?2 ORDER BY artifact_digest",
            )?
            .query_map(params![unit_id, revision], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            !digests.is_empty(),
            "unit version is not retained on this node"
        );
        let digest = match artifact_digest {
            Some(digest) => {
                ensure!(
                    digests.iter().any(|stored| stored == &digest.0),
                    "unit revision does not retain the requested artifact digest"
                );
                digest.clone()
            }
            None => {
                ensure!(
                    digests.len() == 1,
                    "unit revision retains several artifacts; name the artifact digest"
                );
                Digest(digests[0].clone())
            }
        };
        // Withdrawing a version that a retained closure still selects would
        // silently turn a satisfied edge into a gap. Report the dependents and
        // leave the graph alone.
        let dependents = dependent_edges(&tx, unit_id, revision, &digest)?;
        ensure!(
            dependents.is_empty(),
            "UNIT_IN_USE:{}@{}: retained units still depend on this version: {}",
            unit_id,
            revision,
            dependents.join(",")
        );
        // A running trial owns the version's process and evidence. The attempt
        // must settle first; uninstall never kills or replays it.
        let running: Option<String> = tx
            .query_row(
                "SELECT attempt_id FROM unit_attempts \
                 WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 \
                   AND state IN ('trial_open','prepared','spawning','running') \
                 ORDER BY started_at LIMIT 1",
                params![unit_id, revision, digest.0],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            running.is_none(),
            "UNIT_TRIAL_IN_PROGRESS:{}@{}: trial attempt {} still owns this version",
            unit_id,
            revision,
            running.unwrap_or_default()
        );
        let current: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT revision,artifact_digest,enabled FROM unit_current WHERE unit_id=?1",
                [unit_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let was_current = current
            .as_ref()
            .is_some_and(|(current_revision, stored, _)| {
                current_revision == revision && stored == &digest.0
            });
        let was_enabled = was_current
            && current
                .as_ref()
                .is_some_and(|(_, _, enabled)| *enabled != 0);
        if let Some(activation) = activate {
            ensure!(
                was_current,
                "activate names a successor but the withdrawn version is not the current one"
            );
            ensure!(
                activation.revision != revision || activation.artifact_digest != digest,
                "activate must name a version other than the withdrawn one"
            );
            let retained: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM unit_versions \
                 WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3)",
                params![unit_id, activation.revision, activation.artifact_digest.0],
                |r| r.get(0),
            )?;
            ensure!(
                retained,
                "activate names a version this node does not retain"
            );
        }
        let stored_data: String = tx.query_row(
            "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, digest.0],
            |r| r.get(0),
        )?;
        let stored: StoredVersion = serde_json::from_str(&stored_data)?;
        let materialization: Option<MaterializationFact> = tx
            .query_row(
                "SELECT root,path,state,detail FROM unit_materializations \
                 WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, digest.0],
                |r| {
                    Ok(MaterializationFact {
                        root: r.get(0)?,
                        path: r.get(1)?,
                        state: r.get(2)?,
                        detail: r.get(3)?,
                    })
                },
            )
            .optional()?;
        let mut files_retired: Vec<String> = Vec::new();
        let mut files_retained: Vec<RetainedPath> = Vec::new();
        if let Some(materialization) = &materialization {
            retire_package_directory(
                materialization,
                unit_id,
                revision,
                &digest,
                &mut files_retired,
                &mut files_retained,
            )?;
        }
        let archived_edges: i64 = tx.query_row(
            "SELECT count(*) FROM unit_edges \
             WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, digest.0],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO unit_version_history(unit_id,revision,artifact_digest,imported_at,data) \
             SELECT unit_id,revision,artifact_digest,imported_at,data FROM unit_versions \
             WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, digest.0],
        )?;
        tx.execute(
            "INSERT INTO unit_edge_history(unit_id,revision,artifact_digest,edge_id,data) \
             SELECT unit_id,revision,artifact_digest,edge_id,data FROM unit_edges \
             WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, digest.0],
        )?;
        tx.execute(
            "DELETE FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, digest.0],
        )?;
        tx.execute(
            "DELETE FROM unit_materializations WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, digest.0],
        )?;
        tx.execute(
            "DELETE FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, digest.0],
        )?;
        if was_current {
            match activate {
                Some(activation) => {
                    tx.execute(
                        "INSERT INTO unit_current(unit_id,revision,artifact_digest,enabled,updated_at) \
                         VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id) DO UPDATE SET \
                         revision=excluded.revision,artifact_digest=excluded.artifact_digest,\
                         enabled=excluded.enabled,updated_at=excluded.updated_at",
                        params![
                            unit_id,
                            activation.revision,
                            activation.artifact_digest.0,
                            i64::from(was_enabled),
                            removed_at
                        ],
                    )?;
                }
                None => {
                    tx.execute("DELETE FROM unit_current WHERE unit_id=?1", [unit_id])?;
                }
            }
        }
        let evidence = json!({
            "was_current": was_current,
            "was_enabled": was_enabled,
            "replacement": activate,
            "materialization": materialization,
            "files_retired": files_retired,
            "files_retained": files_retained,
            "artifact_retained": true,
            "evidence_retained": true,
            // The adoption decision lived in the archived descriptor; it is
            // reported here so a caller does not have to guess whether an
            // earlier judgement existed.
            "adoption": stored.unit.config.pointer("/cognition/adoption").cloned(),
            "archived_edges": archived_edges.to_string(),
        });
        tx.execute(
            "INSERT INTO unit_removals(unit_id,revision,artifact_digest,removed_at,reason,data) \
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                unit_id,
                revision,
                digest.0,
                removed_at,
                reason,
                serde_json::to_string(&evidence)?
            ],
        )?;
        let removal_seq = tx.last_insert_rowid();
        tx.commit()?;
        Ok(json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": digest,
            "removed": true,
            "removal_seq": removal_seq.to_string(),
            "was_current": was_current,
            "was_enabled": was_enabled,
            "replacement": activate,
            // The receipt repeats what the durable row already holds.
            "materialization": evidence.get("materialization").cloned().unwrap_or(Value::Null),
            "files_retired": evidence.get("files_retired").cloned().unwrap_or(json!([])),
            "files_retained": evidence.get("files_retained").cloned().unwrap_or(json!([])),
            "artifact_retained": true,
            "evidence_retained": true,
            "note": "the version left discovery; its bytes, trial attempts, observations and adoption stay readable",
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::Node;
    use crate::units::UnitRequest;
    use crate::units::adoption::{AdoptionDecision, AdoptionMethod};
    use crate::units::test_support::{exact, export, import, package, scratch};
    use serde_json::json;
    use shenko_types::object::Digest;
    use super::UnitActivation;

    #[test]
    fn uninstall_retires_the_package_and_keeps_the_evidence() {
        let dir = scratch("uninstall-node");
        let packages = scratch("uninstall-packages");
        let mut node = Node::open(&dir).unwrap();
        let unit = package("demo.skill.remove", "1.0.0", b"removable", vec![]);
        let digest = unit.unit.artifact_digest.clone();
        node.handle_unit_request(UnitRequest::Import {
            export: export(vec![unit]),
            package_root: packages.to_string_lossy().into_owned(),
            enable: Some(true),
        })
        .unwrap();
        let found = node
            .installed_unit_fact("demo.skill.remove")
            .unwrap()
            .unwrap();
        let path = found.materialization.as_ref().unwrap().path.clone();
        let directory = std::path::Path::new(&path)
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(std::path::Path::new(&path).is_file());
        node.handle_unit_request(UnitRequest::RecordAdoption {
            unit_id: "demo.skill.remove".into(),
            revision: "1.0.0".into(),
            artifact_digest: digest.clone(),
            decision: AdoptionDecision::Adopted,
            method: AdoptionMethod::OperatorStatement,
            evidence: vec![],
            confidence: None,
            notes: Some("kept by the owner".into()),
            enable: None,
        })
        .unwrap();

        let receipt = node
            .handle_unit_request(UnitRequest::Uninstall {
                unit_id: "demo.skill.remove".into(),
                revision: "1.0.0".into(),
                artifact_digest: None,
                reason: "replaced by an external implementation".into(),
                activate: None,
            })
            .unwrap();
        assert_eq!(receipt["removed"], json!(true));
        assert_eq!(receipt["was_current"], json!(true));
        assert_eq!(receipt["was_enabled"], json!(true));
        assert_eq!(receipt["files_retired"], json!([directory]));
        assert_eq!(receipt["files_retained"], json!([]));
        assert!(
            !std::path::Path::new(&path).exists(),
            "the materialized package directory is gone"
        );
        // Nothing offers the unit any more; the artifact bytes and the
        // archived judgement remain readable facts.
        assert!(node.unit_status("demo.skill.remove").is_err());
        assert_eq!(node.unit_object_bytes(&digest).unwrap(), b"removable".to_vec());
        let lineage = node
            .handle_unit_request(UnitRequest::Lineage {
                unit_id: "demo.skill.remove".into(),
            })
            .unwrap();
        assert_eq!(lineage["versions"], json!([]));
        let removed = lineage["removed"].as_array().unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0]["revision"], json!("1.0.0"));
        assert_eq!(removed[0]["was_current"], json!(true));
        assert_eq!(removed[0]["artifact_retained"], json!(true));
        assert_eq!(removed[0]["evidence_retained"], json!(true));
        assert_eq!(removed[0]["adoption"]["decision"], json!("adopted"));
        assert_eq!(
            removed[0]["reason"],
            json!("replaced by an external implementation")
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&packages);
    }

    #[test]
    fn uninstall_refuses_a_version_a_retained_closure_still_selects() {
        let dir = scratch("uninstall-dependent-node");
        let packages = scratch("uninstall-dependent-packages");
        let mut node = Node::open(&dir).unwrap();
        let dependency = package("demo.dep", "1.0.0", b"dependency", vec![]);
        import(&mut node, &packages, vec![dependency]);
        let consumer = package(
            "demo.root",
            "1.0.0",
            b"consumer",
            vec![exact("edge", "demo.dep", None)],
        );
        import(&mut node, &packages, vec![consumer]);
        assert_eq!(
            node.unit_status("demo.root").unwrap()["closure"]["state"],
            json!("complete")
        );

        let refused = node
            .handle_unit_request(UnitRequest::Uninstall {
                unit_id: "demo.dep".into(),
                revision: "1.0.0".into(),
                artifact_digest: None,
                reason: "attempted while a consumer exists".into(),
                activate: None,
            })
            .expect_err("a satisfied closure must not be broken silently");
        let message = format!("{refused:#}");
        assert!(message.contains("UNIT_IN_USE:"), "{message}");
        assert!(message.contains("demo.root@1.0.0#edge"), "{message}");
        // The refused withdrawal changed nothing.
        assert_eq!(
            node.unit_status("demo.dep").unwrap()["installed"],
            json!(true)
        );
        assert_eq!(
            node.unit_status("demo.root").unwrap()["closure"]["state"],
            json!("complete")
        );

        // Retiring the consumer first makes the dependency removable.
        node.handle_unit_request(UnitRequest::Uninstall {
            unit_id: "demo.root".into(),
            revision: "1.0.0".into(),
            artifact_digest: None,
            reason: "the consumer is retired first".into(),
            activate: None,
        })
        .unwrap();
        node.handle_unit_request(UnitRequest::Uninstall {
            unit_id: "demo.dep".into(),
            revision: "1.0.0".into(),
            artifact_digest: None,
            reason: "no retained consumer remains".into(),
            activate: None,
        })
        .unwrap();
        assert!(node.unit_status("demo.dep").is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&packages);
    }

    #[test]
    fn uninstall_of_the_current_version_activates_the_named_successor() {
        let dir = scratch("uninstall-activate-node");
        let packages = scratch("uninstall-activate-packages");
        let mut node = Node::open(&dir).unwrap();
        let old = package("demo.swap", "1.0.0", b"swap-one", vec![]);
        let new = package("demo.swap", "2.0.0", b"swap-two", vec![]);
        node.handle_unit_request(UnitRequest::Import {
            export: export(vec![new.clone()]),
            package_root: packages.to_string_lossy().into_owned(),
            enable: Some(true),
        })
        .unwrap();
        // The earlier revision arrives later as a retained version and is the
        // one this node currently offers; importing a version never deletes
        // the other retained one.
        import(&mut node, &packages, vec![old.clone()]);
        assert_eq!(
            node.unit_status("demo.swap").unwrap()["revision"],
            json!("1.0.0")
        );
        assert_eq!(node.retained_unit_facts("demo.swap").unwrap().len(), 2);
        let old_path = node
            .installed_unit_version("demo.swap", "1.0.0", &old.unit.artifact_digest)
            .unwrap()
            .unwrap()
            .materialization
            .as_ref()
            .unwrap()
            .path
            .clone();
        let new_path = node
            .installed_unit_version("demo.swap", "2.0.0", &new.unit.artifact_digest)
            .unwrap()
            .unwrap()
            .materialization
            .as_ref()
            .unwrap()
            .path
            .clone();

        let receipt = node
            .handle_unit_request(UnitRequest::Uninstall {
                unit_id: "demo.swap".into(),
                revision: "1.0.0".into(),
                artifact_digest: Some(old.unit.artifact_digest.clone()),
                reason: "upgrade to the retained successor".into(),
                activate: Some(UnitActivation {
                    revision: "2.0.0".into(),
                    artifact_digest: new.unit.artifact_digest.clone(),
                }),
            })
            .unwrap();
        assert_eq!(receipt["replacement"]["revision"], json!("2.0.0"));
        let status = node.unit_status("demo.swap").unwrap();
        assert_eq!(status["revision"], json!("2.0.0"));
        // The successor inherits the enable state the withdrawn version had.
        assert_eq!(status["enabled"], json!(true));
        assert!(!std::path::Path::new(&old_path).exists());
        assert!(std::path::Path::new(&new_path).is_file());
        assert_eq!(node.retained_unit_facts("demo.swap").unwrap().len(), 1);

        // Naming a successor that is not retained is refused before anything
        // is withdrawn.
        let refused = node
            .handle_unit_request(UnitRequest::Uninstall {
                unit_id: "demo.swap".into(),
                revision: "2.0.0".into(),
                artifact_digest: None,
                reason: "successor must exist".into(),
                activate: Some(UnitActivation {
                    revision: "9.9.9".into(),
                    artifact_digest: Digest::blake3(b"absent"),
                }),
            })
            .expect_err("an activation must name a retained version");
        assert!(format!("{refused:#}").contains("activate names a version this node does not retain"));
        assert_eq!(
            node.unit_status("demo.swap").unwrap()["revision"],
            json!("2.0.0")
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&packages);
    }
}
