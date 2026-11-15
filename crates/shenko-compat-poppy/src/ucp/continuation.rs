use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::request::validate_idempotency_key;
use super::{
    NegotiatedProfile, Operation, RequestPlan, Transport, bounded_json, https_url, nonempty,
};

/// Workflow context survives a Poppy Session and a node change; it cannot be
/// silently reused for a different company or pairwise user. This is not a grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceIdentity {
    pub company: String,
    pub pairwise_user_id: String,
    pub resource_id: String,
}

impl ResourceIdentity {
    pub fn new(company: &str, pairwise_user_id: &str, resource_id: &str) -> Result<Self> {
        https_url(company)?;
        ensure!(
            !pairwise_user_id.is_empty() && !resource_id.is_empty(),
            "workflow identity is incomplete"
        );
        Ok(Self {
            company: company.into(),
            pairwise_user_id: pairwise_user_id.into(),
            resource_id: resource_id.into(),
        })
    }

    pub fn validate_owner(&self, company: &str, pairwise_user_id: &str) -> Result<()> {
        ensure!(
            self.company == company && self.pairwise_user_id == pairwise_user_id,
            "UCP workflow belongs to a different company or user"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStatus {
    Incomplete,
    RequiresEscalation,
    ReadyForComplete,
    CompleteInProgress,
    Completed,
    Canceled,
}

/// A merchant observation, including the exact purchase terms. Recording it
/// does not authorize an order. Runtime receipts preserve its source and time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckoutObservation {
    pub identity: ResourceIdentity,
    pub status: CheckoutStatus,
    pub resource: Value,
}

impl CheckoutObservation {
    pub fn from_resource(company: &str, pairwise_user_id: &str, resource: Value) -> Result<Self> {
        bounded_json(&resource, 512 * 1024)?;
        ensure!(
            resource["ucp"]["status"].as_str() != Some("error"),
            "merchant returned a UCP business error"
        );
        let id = nonempty(&resource["id"], "checkout id")?;
        let identity = ResourceIdentity::new(company, pairwise_user_id, id)?;
        let status: CheckoutStatus = serde_json::from_value(resource["status"].clone())
            .context("unknown checkout status")?;
        match status {
            CheckoutStatus::Completed => {
                nonempty(&resource["order"]["id"], "completed checkout order id")?;
            }
            CheckoutStatus::CompleteInProgress => ensure!(
                resource.get("order").is_none(),
                "accepted completion cannot claim an order"
            ),
            CheckoutStatus::RequiresEscalation => {
                https_url(nonempty(&resource["continue_url"], "checkout handoff URL")?)?;
            }
            _ => {}
        }
        if let Some(instruments) = resource["payment"]["instruments"].as_array() {
            ensure!(
                instruments
                    .iter()
                    .all(|instrument| instrument.get("credential").is_none()),
                "merchant response must not echo a payment credential"
            );
        }
        Ok(Self {
            identity,
            status,
            resource,
        })
    }

    /// Send this URL to the user's browser, without Poppy/Authorization headers.
    /// It is not an agent browser-session join endpoint. A later Get Checkout
    /// establishes what actually happened on the merchant's page.
    pub fn user_handoff_url(&self) -> Result<Option<String>> {
        self.resource
            .get("continue_url")
            .map(|value| Ok(https_url(nonempty(value, "checkout handoff URL")?)?.into()))
            .transpose()
    }

    /// Preserve every unrecognized extension field as a term. Only fields with
    /// defined protocol, lifecycle or payment-execution meanings are excluded; prices are never
    /// recomputed or substituted into the merchant's terms.
    pub fn terms_sha256(&self) -> Result<String> {
        let mut terms = self
            .resource
            .as_object()
            .context("checkout resource must be an object")?
            .clone();
        for field in [
            "ucp",
            "payment",
            "messages",
            "status",
            "continue_url",
            "expires_at",
            "order",
            "actions",
        ] {
            terms.remove(field);
        }
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&canonical(&Value::Object(terms)))?)
        ))
    }

    fn ready_for_completion(&self) -> Result<()> {
        ensure!(
            self.status == CheckoutStatus::ReadyForComplete,
            "checkout is not ready for a new completion"
        );
        if let Some(actions) = self.resource.get("actions") {
            ensure!(
                actions.as_object().is_some_and(|v| v.is_empty()),
                "checkout still has outstanding actions"
            );
        }
        let totals = self.resource["totals"]
            .as_array()
            .context("checkout totals missing")?;
        let mut total = None;
        let mut subtotal_count = 0;
        let mut sum = 0_i128;
        for entry in totals {
            let amount = entry["amount"]
                .as_i64()
                .context("checkout amount must be an integer")?;
            match entry["type"].as_str() {
                Some("total") => {
                    ensure!(total.is_none(), "checkout has multiple grand totals");
                    total = Some(i128::from(amount));
                }
                Some(kind) => {
                    if kind == "subtotal" {
                        subtotal_count += 1;
                    }
                    sum += i128::from(amount);
                }
                None => anyhow::bail!("checkout total type missing"),
            }
        }
        ensure!(
            subtotal_count == 1 && total == Some(sum),
            "merchant checkout totals do not reconcile"
        );
        Ok(())
    }
}

/// Durable purchase binding. The credential stays in the node-local provider;
/// this record carries only its reference, the terms, and the original key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletionBinding {
    pub identity: ResourceIdentity,
    pub terms_sha256: String,
    /// Handler availability/configuration must still match immediately before
    /// completion, but it is execution metadata and may disappear on receipts.
    pub payment_handlers_sha256: String,
    pub idempotency_key: String,
    pub credential_ref: String,
}

impl CompletionBinding {
    pub fn new(
        checkout: &CheckoutObservation,
        idempotency_key: &str,
        credential_ref: &str,
    ) -> Result<Self> {
        checkout.ready_for_completion()?;
        validate_idempotency_key(idempotency_key)?;
        ensure!(
            !credential_ref.is_empty(),
            "payment provider credential reference missing"
        );
        Ok(Self {
            identity: checkout.identity.clone(),
            terms_sha256: checkout.terms_sha256()?,
            payment_handlers_sha256: payment_handlers_sha256(checkout)?,
            idempotency_key: idempotency_key.into(),
            credential_ref: credential_ref.into(),
        })
    }

    /// This enforces structural continuity, not a new human approval workflow.
    /// The caller already owns the business decision represented by the binding.
    pub fn validate_terms(&self, latest: &CheckoutObservation) -> Result<()> {
        ensure!(
            self.identity == latest.identity,
            "checkout identity changed since purchase binding"
        );
        ensure!(
            self.terms_sha256 == latest.terms_sha256()?,
            "checkout terms changed since purchase binding"
        );
        ensure!(
            self.payment_handlers_sha256 == payment_handlers_sha256(latest)?,
            "checkout payment handler metadata changed since purchase binding"
        );
        latest.ready_for_completion()
    }

    /// `instrument` is supplied by the selected, installed payment provider or a
    /// merchant-owned saved instrument. This library does not create card tokens.
    /// The returned request is intentionally not Debug/Serialize.
    pub fn prepare_completion(
        &self,
        latest: &CheckoutObservation,
        negotiated: &NegotiatedProfile,
        transport: Transport,
        instrument: Value,
    ) -> Result<RequestPlan> {
        self.validate_terms(latest)?;
        bounded_json(&instrument, 64 * 1024)?;
        nonempty(&instrument["id"], "payment instrument id")?;
        let handler_id = nonempty(&instrument["handler_id"], "payment handler id")?;
        let instrument_type = nonempty(&instrument["type"], "payment instrument type")?;
        let available = latest.resource["ucp"]["payment_handlers"]
            .as_object()
            .context("checkout payment handlers missing")?;
        let mut matched = false;
        for (name, instances) in available {
            let Some(supported) = negotiated.payment_handlers.get(name) else {
                continue;
            };
            let Some(instances) = instances.as_array() else {
                continue;
            };
            for instance in instances {
                if instance["id"].as_str() != Some(handler_id) {
                    continue;
                }
                ensure!(
                    supported
                        .iter()
                        .any(|candidate| candidate["version"] == instance["version"]),
                    "payment handler version was not negotiated"
                );
                if let Some(instruments) = instance["available_instruments"].as_array() {
                    ensure!(
                        instruments
                            .iter()
                            .any(|candidate| candidate["type"].as_str() == Some(instrument_type)),
                        "payment instrument type is unavailable for this checkout"
                    );
                }
                matched = true;
            }
        }
        ensure!(
            matched,
            "payment handler was not advertised for this checkout"
        );
        RequestPlan::prepare(
            negotiated,
            transport,
            Operation::CompleteCheckout,
            Some(&self.identity.resource_id),
            json!({"payment": {"instruments": [instrument]}}),
            Some(&self.idempotency_key),
        )
    }

    /// Recovery queries the original checkout. It never sends Complete Checkout,
    /// invents a new key, or treats an absent response as a safe replay signal.
    pub fn recovery_request(
        &self,
        negotiated: &NegotiatedProfile,
        transport: Transport,
        company: &str,
        pairwise_user_id: &str,
    ) -> Result<RequestPlan> {
        self.identity.validate_owner(company, pairwise_user_id)?;
        RequestPlan::prepare(
            negotiated,
            transport,
            Operation::GetCheckout,
            Some(&self.identity.resource_id),
            Value::Null,
            None,
        )
    }

    /// An order ID is an observed merchant result, not a claim that payment
    /// settled, fulfillment completed, or an unrelated local attempt succeeded.
    pub fn observed_order_id<'a>(
        &self,
        observation: &'a CheckoutObservation,
    ) -> Result<Option<&'a str>> {
        ensure!(
            self.identity == observation.identity,
            "recovery returned a different checkout"
        );
        if observation.status == CheckoutStatus::Completed {
            Ok(Some(nonempty(
                &observation.resource["order"]["id"],
                "completed order id",
            )?))
        } else {
            Ok(None)
        }
    }
}

fn payment_handlers_sha256(checkout: &CheckoutObservation) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical(
            &checkout.resource["ucp"]["payment_handlers"],
        ))?)
    ))
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), canonical(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
        other => other.clone(),
    }
}
