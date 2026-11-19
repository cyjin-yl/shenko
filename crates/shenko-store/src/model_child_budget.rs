//! Finite-child spending is part of the existing task/T1 ledger. A nested
//! allocation is charged in full when its task is admitted; completion,
//! cancellation and Unknown never refund that allocation.
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
    operation::OperationRecord,
    output::ModelChildBudget,
    task::{TaskOwner, TaskRecord},
};

use crate::{
    Store,
    external::{MODEL_CHILD_KIND, MODEL_CHILD_STEP_OPERATION},
};

pub(crate) const BUDGET_USAGE: &str = "model_child_budget_usage";
pub(crate) const TOOL_REFUSALS: &str = "model_child_tool_refusals";
pub(crate) const CONTINUATION_RESERVATIONS: &str = "model_child_continuation_reservations";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChildBudgetUsage {
    pub reserved_tokens: u64,
    pub reserved_tool_calls: u32,
    pub steps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error(
    "model child budget exhausted: requested {requested_tokens} tokens / {requested_tool_calls} tool calls; remaining {remaining_tokens} tokens / {remaining_tool_calls} tool calls"
)]
#[serde(deny_unknown_fields)]
pub struct ModelChildBudgetExceeded {
    pub requested_tokens: u64,
    pub requested_tool_calls: u32,
    pub remaining_tokens: u64,
    pub remaining_tool_calls: u32,
}

impl ModelChildBudgetUsage {
    pub(crate) fn charge(
        &mut self,
        budget: &ModelChildBudget,
        tokens: u64,
        calls: u32,
    ) -> std::result::Result<(), ModelChildBudgetExceeded> {
        let remaining_tokens = budget.max_total_tokens.saturating_sub(self.reserved_tokens);
        let remaining_tool_calls = budget
            .max_tool_calls
            .saturating_sub(self.reserved_tool_calls);
        if tokens > remaining_tokens
            || calls > remaining_tool_calls
            || self.reserved_tokens > budget.max_total_tokens
            || self.reserved_tool_calls > budget.max_tool_calls
        {
            return Err(ModelChildBudgetExceeded {
                requested_tokens: tokens,
                requested_tool_calls: calls,
                remaining_tokens,
                remaining_tool_calls,
            });
        }
        self.reserved_tokens += tokens;
        self.reserved_tool_calls += calls;
        Ok(())
    }
}

impl Store {
    pub(crate) fn read_model_child_object(&self, object: &ObjectRef) -> Result<serde_json::Value> {
        let bytes = self.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
            "model child budget input closure mismatch"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub(crate) fn model_child_contract(&self, task: &TaskRecord) -> Result<serde_json::Value> {
        ensure!(
            task.kind == MODEL_CHILD_KIND,
            "budget owner is not a model child"
        );
        let input = self.read_model_child_object(&task.input_ref)?;
        if task
            .memos
            .get(crate::child::NATIVE_INPUT)
            .and_then(|value| value.as_str())
            == Some("object")
        {
            return Ok(input);
        }
        input
            .get("delegated_task_intents")
            .and_then(|value| value.as_array())
            .and_then(|intents| {
                intents.iter().find(|intent| {
                    intent.get("intent_id") == Some(&serde_json::json!(task.task_id))
                })
            })
            .and_then(|intent| intent.pointer("/definition/input"))
            .cloned()
            .context("model child budget has no frozen delegation")
    }

    pub(crate) fn model_child_budget(&self, task: &TaskRecord) -> Result<ModelChildBudget> {
        let contract = self.model_child_contract(task)?;
        let budget: ModelChildBudget = serde_json::from_value(
            contract
                .get("budget")
                .context("model child lacks frozen budget")?
                .clone(),
        )?;
        ensure!(budget.is_finite(), "model child budget is not finite");
        Ok(budget)
    }

    pub fn model_child_budget_usage(&self, task_id: EntityId) -> Result<ModelChildBudgetUsage> {
        let conn = self.ledger.conn.lock();
        let task = crate::child::read_task(&conn, task_id)?.context("model child task missing")?;
        self.model_child_budget_usage_tx(&conn, &task)
    }

    /// Current durable remainder for a new continuation. The immutable result
    /// reports what remained when the original run ended; this read additionally
    /// includes allocations already transferred to other continuation tasks.
    pub fn model_child_continuation_remaining_budget(
        &self,
        task_id: EntityId,
    ) -> Result<ModelChildBudget> {
        let conn = self.ledger.conn.lock();
        let task =
            crate::child::read_task(&conn, task_id)?.context("continued child task missing")?;
        ensure!(
            task.is_terminal(),
            "continuation requires a finished model child"
        );
        let mut budget = self.model_child_budget(&task)?;
        let usage = self.model_child_budget_usage_tx(&conn, &task)?;
        budget.max_total_tokens = budget
            .max_total_tokens
            .saturating_sub(usage.reserved_tokens);
        budget.max_tool_calls = budget
            .max_tool_calls
            .saturating_sub(usage.reserved_tool_calls);
        Ok(budget)
    }

    pub(crate) fn model_child_continuation_reservations_tx(
        &self,
        conn: &Connection,
        task: &TaskRecord,
    ) -> Result<std::collections::BTreeMap<EntityId, ModelChildBudget>> {
        if let Some(value) = task.memos.get(CONTINUATION_RESERVATIONS) {
            let reservations: std::collections::BTreeMap<EntityId, ModelChildBudget> =
                serde_json::from_value(value.clone())?;
            ensure!(
                reservations.values().all(ModelChildBudget::is_finite),
                "invalid frozen continuation allocation"
            );
            return Ok(reservations);
        }
        // Before durable continuation debits existed, the admitted task is the
        // reservation evidence. Count its whole original allocation even when
        // it failed, was cancelled, or has an unresolved provider request.
        let mut reservations = std::collections::BTreeMap::new();
        let mut query = conn.prepare(
            "SELECT intent_id FROM subject_intents WHERE kind='delegation' AND json_extract(payload,'$.definition.kind')=?1 AND json_extract(payload,'$.definition.input.continued_from')=?2"
        )?;
        for row in query.query_map(params![MODEL_CHILD_KIND, task.task_id.to_string()], |row| {
            row.get::<_, String>(0)
        })? {
            let id: EntityId = row?.parse()?;
            let child = crate::child::read_task(conn, id)?
                .context("continuation allocation has no original task")?;
            let contract = self.model_child_contract(&child)?;
            ensure!(
                contract.get("continued_from") == Some(&serde_json::json!(task.task_id)),
                "continuation allocation differs from its original frozen contract"
            );
            let budget: ModelChildBudget = serde_json::from_value(
                contract
                    .get("budget")
                    .context("continued child lacks frozen budget")?
                    .clone(),
            )?;
            ensure!(budget.is_finite(), "continued child budget is not finite");
            reservations.insert(id, budget);
        }
        Ok(reservations)
    }

    pub fn model_child_tool_refusal(
        &self,
        parent_id: EntityId,
        tool_task_id: EntityId,
    ) -> Result<Option<ModelChildBudgetExceeded>> {
        let conn = self.ledger.conn.lock();
        let task =
            crate::child::read_task(&conn, parent_id)?.context("model child task missing")?;
        task.memos
            .get(TOOL_REFUSALS)
            .and_then(|value| value.get(tool_task_id.to_string()))
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    pub(crate) fn model_child_budget_usage_tx(
        &self,
        conn: &Connection,
        task: &TaskRecord,
    ) -> Result<ModelChildBudgetUsage> {
        let mut usage = if let Some(value) = task.memos.get(BUDGET_USAGE) {
            serde_json::from_value(value.clone())?
        } else {
            // Existing installations acquire their initial balance from original
            // T1s and frozen child allocations, including unresolved operations.
            // A corrupt object fails closed; it is never silently counted as zero.
            let mut usage = ModelChildBudgetUsage::default();
            let mut query = conn.prepare("SELECT data FROM operations WHERE json_extract(data,'$.task_id')=?1 AND json_extract(data,'$.operation_name')=?2")?;
            for row in query.query_map(
                params![task.task_id.to_string(), MODEL_CHILD_STEP_OPERATION],
                |row| row.get::<_, String>(0),
            )? {
                let operation: OperationRecord = serde_json::from_str(&row?)?;
                let step = self.read_model_child_object(&operation.final_input_ref)?;
                let tokens = step
                    .get("max_tokens")
                    .and_then(|value| value.as_u64())
                    .context("original model child T1 lacks max_tokens")?;
                usage.reserved_tokens = usage
                    .reserved_tokens
                    .checked_add(tokens)
                    .context("model child token reservation overflow")?;
                usage.steps = usage
                    .steps
                    .checked_add(1)
                    .context("model child step reservation overflow")?;
            }
            let mut children =
                conn.prepare("SELECT data FROM tasks WHERE json_extract(data,'$.owner.id')=?1")?;
            for row in
                children.query_map([task.task_id.to_string()], |row| row.get::<_, String>(0))?
            {
                let child: TaskRecord = serde_json::from_str(&row?)?;
                if child.owner != (TaskOwner::Task { id: task.task_id }) {
                    continue;
                }
                let (tokens, calls) =
                    self.model_child_tool_charge(&child.kind, &child.input_ref)?;
                usage.reserved_tokens = usage
                    .reserved_tokens
                    .checked_add(tokens)
                    .context("model child token reservation overflow")?;
                usage.reserved_tool_calls = usage
                    .reserved_tool_calls
                    .checked_add(calls)
                    .context("model child tool reservation overflow")?;
            }
            usage
        };
        for budget in self
            .model_child_continuation_reservations_tx(conn, task)?
            .values()
        {
            usage.reserved_tokens = usage
                .reserved_tokens
                .checked_add(budget.max_total_tokens)
                .context("model child continuation token reservation overflow")?;
            usage.reserved_tool_calls = usage
                .reserved_tool_calls
                .checked_add(budget.max_tool_calls)
                .context("model child continuation tool reservation overflow")?;
        }
        Ok(usage)
    }

    pub(crate) fn model_child_tool_charge(
        &self,
        kind: &str,
        input: &ObjectRef,
    ) -> Result<(u64, u32)> {
        if kind != MODEL_CHILD_KIND {
            return Ok((0, 1));
        }
        let contract = self.read_model_child_object(input)?;
        let budget: ModelChildBudget = serde_json::from_value(
            contract
                .get("budget")
                .context("nested child lacks frozen budget")?
                .clone(),
        )?;
        ensure!(
            budget.is_finite(),
            "nested model child budget is not finite"
        );
        Ok((
            budget.max_total_tokens,
            budget
                .max_tool_calls
                .checked_add(1)
                .context("nested model child tool allocation overflow")?,
        ))
    }
}
