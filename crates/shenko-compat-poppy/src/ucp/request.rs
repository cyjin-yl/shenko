use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{
    CART, CATALOG_LOOKUP, CATALOG_SEARCH, CHECKOUT, NegotiatedProfile, ORDER, bounded_json,
    https_url,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Mcp,
    Rest,
}

/// Names are the actual UCP shopping binding, not model-selected arbitrary tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    SearchCatalog,
    LookupCatalog,
    GetProduct,
    CreateCart,
    GetCart,
    UpdateCart,
    CancelCart,
    CreateCheckout,
    GetCheckout,
    UpdateCheckout,
    CompleteCheckout,
    CancelCheckout,
    GetOrder,
}

impl Operation {
    pub fn tool_name(self) -> &'static str {
        match self {
            Self::SearchCatalog => "search_catalog",
            Self::LookupCatalog => "lookup_catalog",
            Self::GetProduct => "get_product",
            Self::CreateCart => "create_cart",
            Self::GetCart => "get_cart",
            Self::UpdateCart => "update_cart",
            Self::CancelCart => "cancel_cart",
            Self::CreateCheckout => "create_checkout",
            Self::GetCheckout => "get_checkout",
            Self::UpdateCheckout => "update_checkout",
            Self::CompleteCheckout => "complete_checkout",
            Self::CancelCheckout => "cancel_checkout",
            Self::GetOrder => "get_order",
        }
    }

    pub fn capability(self) -> &'static str {
        match self {
            Self::SearchCatalog => CATALOG_SEARCH,
            Self::LookupCatalog | Self::GetProduct => CATALOG_LOOKUP,
            Self::CreateCart | Self::GetCart | Self::UpdateCart | Self::CancelCart => CART,
            Self::GetOrder => ORDER,
            _ => CHECKOUT,
        }
    }

    pub fn changes_state(self) -> bool {
        !matches!(
            self,
            Self::SearchCatalog
                | Self::LookupCatalog
                | Self::GetProduct
                | Self::GetCart
                | Self::GetCheckout
                | Self::GetOrder
        )
    }

    pub fn needs_resource_id(self) -> bool {
        !matches!(
            self,
            Self::SearchCatalog
                | Self::LookupCatalog
                | Self::GetProduct
                | Self::CreateCart
                | Self::CreateCheckout
        )
    }

    fn payload_name(self) -> Option<&'static str> {
        match self {
            Self::SearchCatalog | Self::LookupCatalog | Self::GetProduct => Some("catalog"),
            Self::CreateCart | Self::UpdateCart => Some("cart"),
            Self::CreateCheckout | Self::UpdateCheckout | Self::CompleteCheckout => {
                Some("checkout")
            }
            _ => None,
        }
    }

    fn rest_method(self) -> &'static str {
        match self {
            Self::GetCart | Self::GetCheckout | Self::GetOrder => "GET",
            Self::UpdateCart | Self::UpdateCheckout => "PUT",
            _ => "POST",
        }
    }

    fn rest_path(self) -> &'static [&'static str] {
        match self {
            Self::SearchCatalog => &["catalog", "search"],
            Self::LookupCatalog => &["catalog", "lookup"],
            Self::GetProduct => &["catalog", "product"],
            Self::CreateCart | Self::GetCart | Self::UpdateCart | Self::CancelCart => &["carts"],
            Self::GetOrder => &["orders"],
            _ => &["checkout-sessions"],
        }
    }
}

/// A sendable request can include a payment credential. It intentionally has no
/// Debug or Serialize implementation. Persist a host credential reference and
/// the immutable binding before resolving its secret into this request.
pub struct RequestPlan {
    pub operation: Operation,
    pub transport: Transport,
    pub endpoint: String,
    pub method: &'static str,
    pub headers: BTreeMap<String, String>,
    arguments: Value,
    rest_body: Option<Value>,
}

impl RequestPlan {
    /// Preserve the caller's original idempotency key. Every mutating operation
    /// carries one even where the binding only recommends it. This function does
    /// not generate a replacement key or retry a dispatched request.
    pub fn prepare(
        negotiated: &NegotiatedProfile,
        transport: Transport,
        operation: Operation,
        resource_id: Option<&str>,
        payload: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Self> {
        bounded_json(&payload, 512 * 1024)?;
        ensure!(
            negotiated.capabilities.contains_key(operation.capability()),
            "operation capability was not negotiated"
        );
        ensure!(
            resource_id.is_some() == operation.needs_resource_id(),
            "operation resource id presence mismatch"
        );
        if let Some(id) = resource_id {
            ensure!(!id.is_empty(), "resource id is empty");
        }
        ensure!(
            !operation.changes_state() || idempotency_key.is_some(),
            "mutating UCP operation requires its durable idempotency key"
        );
        if let Some(key) = idempotency_key {
            validate_idempotency_key(key)?;
        }
        if let Some(name) = operation.payload_name() {
            ensure!(payload.is_object(), "operation payload must be an object");
            if name != "catalog" {
                ensure!(
                    payload.get("id").is_none(),
                    "UCP resource id belongs outside the payload"
                );
            }
        } else {
            ensure!(
                payload.is_null() || payload.as_object().is_some_and(|v| v.is_empty()),
                "operation has no payload"
            );
        }
        if operation == Operation::CreateCheckout && payload.get("cart_id").is_some() {
            ensure!(
                negotiated.capabilities.contains_key(CART),
                "cart-to-checkout requires negotiated cart capability"
            );
        }
        let profile = https_url(&negotiated.platform_profile)?;
        let mut headers = BTreeMap::from([("UCP-Agent".into(), format!("profile=\"{profile}\""))]);
        let mut meta = json!({"ucp-agent": {"profile": profile.as_str()}});
        if let Some(key) = idempotency_key {
            headers.insert("Idempotency-Key".into(), key.into());
            meta["idempotency-key"] = key.into();
        }
        let mut arguments = json!({"meta": meta});
        if let Some(id) = resource_id {
            arguments["id"] = id.into();
        }
        if let Some(name) = operation.payload_name() {
            arguments[name] = payload.clone();
        }
        let service = negotiated.service(transport)?;
        let mut endpoint = https_url(&service.endpoint)?;
        let (method, rest_body) = match transport {
            Transport::Mcp => ("POST", None),
            Transport::Rest => {
                ensure!(
                    endpoint.query().is_none(),
                    "REST service base cannot contain query parameters"
                );
                {
                    let mut path = endpoint
                        .path_segments_mut()
                        .map_err(|()| anyhow::anyhow!("invalid REST endpoint base"))?;
                    path.pop_if_empty();
                    for segment in operation.rest_path() {
                        path.push(segment);
                    }
                    if let Some(id) = resource_id {
                        path.push(id);
                    }
                    if matches!(operation, Operation::CompleteCheckout) {
                        path.push("complete");
                    }
                    if matches!(operation, Operation::CancelCart | Operation::CancelCheckout) {
                        path.push("cancel");
                    }
                }
                let body = if operation.payload_name().is_some() {
                    Some(payload)
                } else if operation.rest_method() == "POST" {
                    Some(json!({}))
                } else {
                    None
                };
                (operation.rest_method(), body)
            }
        };
        if transport == Transport::Mcp || rest_body.is_some() {
            headers.insert("Content-Type".into(), "application/json".into());
        }
        Ok(Self {
            operation,
            transport,
            endpoint: endpoint.into(),
            method,
            headers,
            arguments,
            rest_body,
        })
    }

    /// Exact `tools/call` params for the existing MCP SDK. The SDK owns JSON-RPC
    /// request IDs; the transport signer signs the actual resulting HTTP bytes.
    pub fn mcp_params(&self) -> Value {
        json!({"name": self.operation.tool_name(), "arguments": self.arguments})
    }

    pub fn arguments(&self) -> &Value {
        &self.arguments
    }

    pub fn rest_body(&self) -> Option<&Value> {
        self.rest_body.as_ref()
    }

    /// Hashes the logical request including the body. It does not serialize the
    /// credential itself into the durable ledger and is not a server receipt.
    pub fn payload_sha256(&self) -> Result<String> {
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&self.arguments)?)
        ))
    }

    /// Token-tier Shopify calls require the actual buyer's IP, not the agent's
    /// address or a placeholder. This setter validates the supplied address only.
    pub fn with_buyer_ip(mut self, buyer_ip: &str) -> Result<Self> {
        let ip: std::net::IpAddr = buyer_ip.parse().context("invalid buyer IP")?;
        self.headers
            .insert("Shopify-Buyer-IP".into(), ip.to_string());
        Ok(self)
    }
}

pub(super) fn validate_idempotency_key(key: &str) -> Result<()> {
    let bytes = key.as_bytes();
    ensure!(
        bytes.len() == 36
            && bytes.iter().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    *b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }),
        "UCP idempotency key must be a UUID"
    );
    Ok(())
}
