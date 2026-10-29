//! Runtime roles are mutable facts of one Node, never device subclasses.
use super::{Node, digest, get, number, text};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::ids::EntityId;

pub(crate) const ROLE_SCHEMA: &str = "
CREATE TABLE node_role_assignments(revision INTEGER PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE node_runtime_observations(seq INTEGER PRIMARY KEY AUTOINCREMENT,data TEXT NOT NULL);";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Primary,
    Executor,
    Replica,
    Recovery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleAssignmentRequest {
    pub operation_id: EntityId,
    pub expected_revision: String,
    pub roles: Vec<NodeRole>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleAssignment {
    pub node_id: EntityId,
    pub operation_id: EntityId,
    pub revision: String,
    pub roles: Vec<NodeRole>,
    pub reason: String,
    pub decided_at: chrono::DateTime<chrono::Utc>,
}

/// Last observed runtime state, not a persistent claim that a process is alive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRoleObservation {
    pub observation_id: EntityId,
    pub node_id: EntityId,
    pub runtime_instance_id: EntityId,
    pub roles: Vec<NodeRole>,
    pub subject_id: Option<EntityId>,
    pub process_id: u32,
    pub details: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

impl Node {
    pub fn local_identity(&self) -> Result<Option<super::Identity>> {
        let value = self
            .conn
            .query_row("SELECT v FROM meta WHERE k='identity'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(value
            .map(|s| serde_json::from_str::<Option<super::Identity>>(&s))
            .transpose()?
            .flatten())
    }

    pub fn local_node_id(&self) -> Result<Option<EntityId>> {
        self.conn
            .query_row("SELECT v FROM meta WHERE k='node'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|s| Ok(serde_json::from_str(&s)?))
            .transpose()
    }

    /// Assignment is autonomous intent. It does not itself start a subject,
    /// change controller authority, or prove any device capability.
    pub fn assign_roles(&mut self, mut request: RoleAssignmentRequest) -> Result<RoleAssignment> {
        ensure!(
            !request.reason.trim().is_empty(),
            "role change requires a reason"
        );
        let expected = number(&request.expected_revision)?;
        request.roles.sort();
        request.roles.dedup();
        let seal = digest(&request)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((old, result)) = tx
            .query_row(
                "SELECT seal,result FROM commands WHERE id=?1",
                [request.operation_id.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        {
            ensure!(old == seal.0, "role operation payload conflict");
            return Ok(serde_json::from_str(&result)?);
        }
        let node_id: EntityId = get(&tx, "node")?;
        let revision: u64 = tx.query_row(
            "SELECT COALESCE(MAX(revision),0) FROM node_role_assignments",
            [],
            |r| r.get(0),
        )?;
        ensure!(revision == expected, "role revision conflict");
        let revision = revision.checked_add(1).context("role revision exhausted")?;
        ensure!(
            i64::try_from(revision).is_ok(),
            "role revision exceeds SQLite INTEGER"
        );
        let assignment = RoleAssignment {
            node_id,
            operation_id: request.operation_id,
            revision: revision.to_string(),
            roles: request.roles,
            reason: request.reason,
            decided_at: chrono::Utc::now(),
        };
        let result = text(&assignment)?;
        tx.execute(
            "INSERT INTO node_role_assignments VALUES(?1,?2)",
            params![revision, result],
        )?;
        tx.execute(
            "INSERT INTO commands VALUES(?1,?2,?3)",
            params![assignment.operation_id.to_string(), seal.0, result],
        )?;
        tx.commit()?;
        Ok(assignment)
    }

    /// Called by the actual native runtime, separately from model-facing role
    /// assignment. A receipt records what was observed, not future liveness.
    pub fn observe_runtime_roles(
        &mut self,
        runtime_instance_id: EntityId,
        mut roles: Vec<NodeRole>,
        subject_id: Option<EntityId>,
        details: String,
    ) -> Result<RuntimeRoleObservation> {
        ensure!(
            !details.trim().is_empty(),
            "runtime observation requires details"
        );
        ensure!(
            !roles.contains(&NodeRole::Primary) || subject_id.is_some(),
            "primary observation requires the actual subject identity"
        );
        roles.sort();
        roles.dedup();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let observation = RuntimeRoleObservation {
            observation_id: EntityId::new(),
            node_id: get(&tx, "node")?,
            runtime_instance_id,
            roles,
            subject_id,
            process_id: std::process::id(),
            details,
            observed_at: chrono::Utc::now(),
        };
        tx.execute(
            "INSERT INTO node_runtime_observations(data) VALUES(?1)",
            [text(&observation)?],
        )?;
        tx.commit()?;
        Ok(observation)
    }

    pub(crate) fn latest_assignment(&self) -> Result<Option<RoleAssignment>> {
        let assignment: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM node_role_assignments ORDER BY revision DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        assignment
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn latest_observation(&self) -> Result<Option<RuntimeRoleObservation>> {
        let observed: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM node_runtime_observations ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        observed
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }

    pub fn role_status(&self) -> Result<Value> {
        let node_id = self.local_node_id()?.context("node not initialized")?;
        let assignment = self.latest_assignment()?;
        let observed = self.latest_observation()?;
        Ok(json!({
            "node_id": node_id,
            "assignment": assignment,
            "last_runtime_observation": observed,
            "persisted_observation_is_liveness": false,
            "device_type_is_role": false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn role_assignment_and_observation_remain_distinct_across_reopen() {
        let dir = std::env::temp_dir().join(format!("shenko-node-role-{}", EntityId::new()));
        let node_id = EntityId::new();
        let mut node = Node::open(&dir).unwrap();
        node.handle(super::super::Request::Initialize {
            node_id,
            identity: None,
        })
        .unwrap();
        let request = RoleAssignmentRequest {
            operation_id: EntityId::new(),
            expected_revision: "0".into(),
            roles: vec![NodeRole::Primary, NodeRole::Executor],
            reason: "available model changed".into(),
        };
        let first = node.assign_roles(request.clone()).unwrap();
        assert_eq!(
            node.assign_roles(request).unwrap().decided_at,
            first.decided_at
        );
        assert!(node.role_status().unwrap()["last_runtime_observation"].is_null());
        let runtime = EntityId::new();
        node.observe_runtime_roles(
            runtime,
            vec![NodeRole::Executor],
            None,
            "native executor actually installed; subject not started".into(),
        )
        .unwrap();
        node.assign_roles(RoleAssignmentRequest {
            operation_id: EntityId::new(),
            expected_revision: "1".into(),
            roles: vec![NodeRole::Replica],
            reason: "primary moved elsewhere".into(),
        })
        .unwrap();
        drop(node);
        let node = Node::open(&dir).unwrap();
        let status = node.role_status().unwrap();
        assert_eq!(node.local_node_id().unwrap(), Some(node_id));
        assert_eq!(status["assignment"]["revision"], "2");
        assert_eq!(status["assignment"]["roles"], json!(["replica"]));
        assert_eq!(
            status["last_runtime_observation"]["roles"],
            json!(["executor"])
        );
        assert_eq!(
            status["last_runtime_observation"]["runtime_instance_id"],
            json!(runtime)
        );
        assert_eq!(status["persisted_observation_is_liveness"], false);
    }
}
