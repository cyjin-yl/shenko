//! UCP 2026-08-25 negotiation and commerce request preparation.
//!
//! This module does no network or ledger I/O. A profile is a capability claim,
//! not a grant. The host owns authenticated transport, durable dispatch, local
//! credentials, and the decision to perform a business operation. MCP schemas
//! come from the actual server; missing references never trigger implicit I/O.
//!
//! Protocol sources: <https://ucp.dev/2026-08-25/specification/overview/>,
//! <https://personalagentprotocol.org/docs/1.0/extensions/ucp>.

mod continuation;
mod request;
mod rest_schema;
mod schema;
mod schema_resolver;

pub use continuation::{CheckoutObservation, CheckoutStatus, CompletionBinding, ResourceIdentity};
pub use request::{Operation, RequestPlan, Transport};
pub use rest_schema::{RestSchemas, referenced_schema_documents};
pub use schema::ToolSchemas;

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::{Host, Url};

pub const VERSION: &str = "2026-08-25";
pub const SHOPPING_SERVICE: &str = "dev.ucp.shopping";
pub const CHECKOUT: &str = "dev.ucp.shopping.checkout";
pub const CART: &str = "dev.ucp.shopping.cart";
pub const ORDER: &str = "dev.ucp.shopping.order";
pub const CATALOG_SEARCH: &str = "dev.ucp.shopping.catalog.search";
pub const CATALOG_LOOKUP: &str = "dev.ucp.shopping.catalog.lookup";

/// Public, operator-wide platform metadata. Never put a user, company-specific
/// identifier, account token, or private JWK in this document or its URL.
#[derive(Clone, Debug)]
pub struct PlatformProfile {
    url: String,
    document: Value,
}

impl PlatformProfile {
    /// Advertise the implemented shopping client. There is no invented callback
    /// endpoint, payment processor, webhook receiver, or independent IdP flow.
    pub fn new(profile_url: &str, public_jwks: &Value) -> Result<Self> {
        let url = https_url(profile_url)?;
        ensure!(
            url.query().is_none(),
            "platform profile URL cannot contain a query"
        );
        let keys = public_jwks["keys"]
            .as_array()
            .context("public JWKS needs keys")?;
        ensure!(!keys.is_empty(), "platform signing keys are empty");
        for key in keys {
            let object = key.as_object().context("public JWK must be an object")?;
            ensure!(
                object.get("kty").and_then(Value::as_str) != Some("oct")
                    && ["d", "k", "p", "q", "dp", "dq", "qi", "oth"]
                        .iter()
                        .all(|field| !object.contains_key(*field)),
                "a UCP platform profile must not publish private key material"
            );
            nonempty(&key["kid"], "public JWK kid")?;
        }
        let mut capabilities = Map::new();
        for (name, schema, spec) in [
            (CHECKOUT, "checkout", "checkout"),
            (CART, "cart", "cart"),
            (ORDER, "order", "order"),
            (CATALOG_SEARCH, "catalog_search", "catalog/search"),
            (CATALOG_LOOKUP, "catalog_lookup", "catalog/lookup"),
        ] {
            capabilities.insert(
                name.into(),
                json!([{
                    "version": VERSION,
                    "spec": format!("https://ucp.dev/{VERSION}/specification/shopping/{spec}/"),
                    "schema": format!("https://ucp.dev/{VERSION}/schemas/shopping/{schema}.json")
                }]),
            );
        }
        Ok(Self {
            url: url.into(),
            document: json!({
                "ucp": {"version": VERSION, "services": {},
                    "capabilities": capabilities, "payment_handlers": {}},
                "keys": keys
            }),
        })
    }

    /// Declare a separately installed client capability or extension. The host
    /// must actually implement its semantics and composed schema before adding
    /// it. This is not an inference from a merchant's advertisement.
    pub fn with_capability(mut self, name: &str, declaration: Value) -> Result<Self> {
        validate_entity(name, &declaration, true)?;
        self.document["ucp"]["capabilities"]
            .as_object_mut()
            .context("platform capabilities")?
            .insert(name.into(), json!([declaration]));
        Ok(self)
    }

    /// Advertise an actually available credential provider. The default profile
    /// has no handlers; a merchant's `continue_url` still permits user handoff.
    pub fn with_payment_handler(mut self, name: &str, declaration: Value) -> Result<Self> {
        validate_entity(name, &declaration, true)?;
        nonempty(&declaration["id"], "payment handler id")?;
        self.document["ucp"]["payment_handlers"]
            .as_object_mut()
            .context("platform payment handlers")?
            .entry(name)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("platform handler versions")?
            .push(declaration);
        Ok(self)
    }

    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn document(&self) -> &Value {
        &self.document
    }

    /// RFC 8941 dictionary syntax; a URL serialization cannot contain raw quotes
    /// or backslashes, but escape defensively for the structured string anyway.
    pub fn agent_header(&self) -> String {
        format!(
            "profile=\"{}\"",
            self.url.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }
}

/// The locally computed intersection. Only a real merchant response establishes
/// which capabilities that particular request actually used.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NegotiatedProfile {
    pub version: String,
    pub platform_profile: String,
    pub capabilities: BTreeMap<String, Value>,
    pub payment_handlers: BTreeMap<String, Vec<Value>>,
    pub services: Vec<Service>,
    pub excluded: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub transport: Transport,
    pub endpoint: String,
    pub schema: String,
}

impl NegotiatedProfile {
    pub fn from_business(business: &Value, platform: &PlatformProfile) -> Result<Self> {
        bounded_json(business, 512 * 1024)?;
        let ucp = business["ucp"]
            .as_object()
            .context("UCP profile needs ucp")?;
        ensure!(
            ucp.get("version").and_then(Value::as_str) == Some(VERSION),
            "unsupported UCP profile version; select a complete supported version profile first"
        );
        let declarations = ucp.get("capabilities").and_then(Value::as_object);
        let mut result = Self {
            version: VERSION.into(),
            platform_profile: platform.url.clone(),
            capabilities: BTreeMap::new(),
            payment_handlers: BTreeMap::new(),
            services: Vec::new(),
            excluded: BTreeMap::new(),
        };
        if let Some(declarations) = declarations {
            for (name, offered) in declarations {
                let Some(supported) = platform.document["ucp"]["capabilities"][name].as_array()
                else {
                    result
                        .excluded
                        .insert(name.clone(), "not implemented by this platform".into());
                    continue;
                };
                if let Some(selected) = select_entity(name, offered, supported, true) {
                    result.capabilities.insert(name.clone(), selected);
                } else {
                    result
                        .excluded
                        .insert(name.clone(), "no valid exact version intersection".into());
                }
            }
        }
        result.prune_extensions()?;
        let handlers = ucp
            .get("payment_handlers")
            .and_then(Value::as_object)
            .context("UCP profile needs payment_handlers registry")?;
        for (name, offered) in handlers {
            let Some(supported) = platform.document["ucp"]["payment_handlers"][name].as_array()
            else {
                continue;
            };
            let Some(entries) = offered.as_array() else {
                continue;
            };
            let matching: Vec<_> = entries
                .iter()
                .filter(|entry| {
                    validate_entity(name, entry, true).is_ok()
                        && entry["id"].as_str().is_some_and(|id| !id.is_empty())
                        && supported
                            .iter()
                            .any(|own| own["version"] == entry["version"])
                })
                .cloned()
                .collect();
            if !matching.is_empty() {
                result.payment_handlers.insert(name.clone(), matching);
            }
        }
        let services = ucp
            .get("services")
            .and_then(Value::as_object)
            .context("UCP profile needs services registry")?;
        if let Some(services) = services.get(SHOPPING_SERVICE).and_then(Value::as_array) {
            for service in services {
                if service["version"].as_str() != Some(VERSION)
                    || validate_entity(SHOPPING_SERVICE, service, true).is_err()
                {
                    continue;
                }
                let transport = match service["transport"].as_str() {
                    Some("mcp") => Transport::Mcp,
                    Some("rest") => Transport::Rest,
                    _ => continue,
                };
                let endpoint = nonempty(&service["endpoint"], "service endpoint")?;
                https_url(endpoint)?;
                result.services.push(Service {
                    name: SHOPPING_SERVICE.into(),
                    transport,
                    endpoint: endpoint.into(),
                    schema: nonempty(&service["schema"], "service schema")?.into(),
                });
            }
        }
        ensure!(
            !result.services.is_empty(),
            "merchant has no supported shopping transport"
        );
        Ok(result)
    }

    fn prune_extensions(&mut self) -> Result<()> {
        loop {
            let mut remove = Vec::new();
            for (name, capability) in &self.capabilities {
                let parents = parents(capability)?;
                if !parents.is_empty()
                    && !parents
                        .iter()
                        .any(|parent| self.capabilities.contains_key(*parent))
                {
                    remove.push(name.clone());
                }
            }
            if remove.is_empty() {
                break;
            }
            for name in remove {
                self.capabilities.remove(&name);
                self.excluded
                    .insert(name, "no negotiated parent capability".into());
            }
        }
        Ok(())
    }

    pub fn service(&self, transport: Transport) -> Result<&Service> {
        self.services
            .iter()
            .find(|service| service.transport == transport)
            .context("merchant did not advertise the requested transport")
    }

    /// Check the response's actual capability declaration, without upgrading the
    /// response from an observation into authorization or a purchase receipt.
    pub fn validate_response_capabilities(
        &self,
        resource: &Value,
        operation: Operation,
    ) -> Result<()> {
        ensure!(
            resource["ucp"]["version"].as_str() == Some(&self.version),
            "UCP response version changed"
        );
        if resource["ucp"]["status"].as_str() == Some("error") {
            return Ok(());
        }
        let active = resource["ucp"]["capabilities"]
            .as_object()
            .context("UCP response capabilities missing")?;
        ensure!(
            active.contains_key(operation.capability()),
            "response omits the operation's capability"
        );
        for (name, versions) in active {
            let negotiated = self
                .capabilities
                .get(name)
                .context("response activates an unnegotiated capability")?;
            let versions = versions
                .as_array()
                .context("invalid response capability versions")?;
            ensure!(
                versions.len() == 1 && versions[0]["version"] == negotiated["version"],
                "response capability version does not match negotiation"
            );
        }
        Ok(())
    }
}

fn select_entity(
    name: &str,
    offered: &Value,
    supported: &[Value],
    schema_required: bool,
) -> Option<Value> {
    offered
        .as_array()?
        .iter()
        .filter(|entry| {
            validate_entity(name, entry, schema_required).is_ok()
                && supported
                    .iter()
                    .any(|own| own["version"] == entry["version"])
        })
        .max_by_key(|entry| entry["version"].as_str())
        .cloned()
}

fn parents(entity: &Value) -> Result<Vec<&str>> {
    match entity.get("extends") {
        None => Ok(Vec::new()),
        Some(Value::String(parent)) => Ok(vec![parent]),
        Some(Value::Array(parents)) => parents
            .iter()
            .map(|v| nonempty(v, "extension parent"))
            .collect(),
        _ => Err(anyhow!("invalid extension parent declaration")),
    }
}

fn validate_entity(name: &str, entity: &Value, schema_required: bool) -> Result<()> {
    nonempty(&entity["version"], "entity version")?;
    if let Some(schema) = entity.get("schema") {
        validate_schema_authority(name, nonempty(schema, "entity schema")?)?;
    } else {
        ensure!(!schema_required, "entity schema missing");
    }
    if let Some(spec) = entity.get("spec") {
        https_url(nonempty(spec, "entity spec")?)?;
    }
    parents(entity)?;
    Ok(())
}

/// UCP authority binding follows the schema URL's reversed DNS labels. This is
/// provenance only, separate from DNS/address checks before an actual fetch.
pub fn validate_schema_authority(name: &str, schema: &str) -> Result<()> {
    let url = https_url(schema)?;
    let Some(Host::Domain(host)) = url.host() else {
        anyhow::bail!("schema authority must be a DNS domain")
    };
    let labels: Vec<_> = host.trim_end_matches('.').split('.').collect();
    ensure!(
        labels.len() >= 2 && labels.iter().all(|label| !label.is_empty()),
        "invalid schema authority domain"
    );
    let authority = labels.into_iter().rev().collect::<Vec<_>>().join(".");
    ensure!(
        name == authority
            || name
                .strip_prefix(&authority)
                .is_some_and(|suffix| suffix.starts_with('.')),
        "schema URL does not own the entity namespace"
    );
    Ok(())
}

pub(super) fn https_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid UCP URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "UCP URL must be HTTPS without userinfo or fragment"
    );
    Ok(url)
}

pub(super) fn nonempty<'a>(value: &'a Value, label: &str) -> Result<&'a str> {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{label} must be nonempty"))
}

pub(super) fn bounded_json(value: &Value, max_bytes: usize) -> Result<()> {
    ensure!(
        serde_json::to_vec(value)?.len() <= max_bytes,
        "UCP document exceeds size bound"
    );
    let mut pending = vec![(value, 0_usize)];
    let mut count = 0_usize;
    while let Some((value, depth)) = pending.pop() {
        count += 1;
        ensure!(
            depth <= 48 && count <= 32_768,
            "UCP document exceeds structural bound"
        );
        match value {
            Value::Array(values) => pending.extend(values.iter().map(|v| (v, depth + 1))),
            Value::Object(values) => pending.extend(values.values().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}
