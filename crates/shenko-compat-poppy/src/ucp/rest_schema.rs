//! Pure UCP/OpenAPI schema composition. Fetching stays in the bounded host
//! transport; the upstream annotation resolver is used only as a pure function.

use std::collections::{BTreeMap, BTreeSet};

use super::schema_resolver::{self, Direction, Requires, ResolveOptions};
use anyhow::{Context, Result, anyhow, ensure};
use jsonschema::Validator;
use serde_json::{Value, json};

use super::schema::{SuppliedDocuments, compile};
use super::{
    NegotiatedProfile, Operation, RequestPlan, bounded_json, https_url, nonempty, parents,
};

pub struct RestSchemas {
    input: Option<Validator>,
    outputs: BTreeMap<String, Validator>,
    extensions: BTreeMap<String, Validator>,
}

impl RestSchemas {
    pub fn from_openapi(
        negotiated: &NegotiatedProfile,
        operation: Operation,
        source: &str,
        document: &Value,
        resources: BTreeMap<String, Value>,
    ) -> Result<Self> {
        super::validate_schema_authority(super::SHOPPING_SERVICE, source)?;
        bounded_json(document, 512 * 1024)?;
        let paths = document["paths"]
            .as_object()
            .context("UCP REST schema has no paths")?;
        let mut selected = None;
        for (path, item) in paths {
            for method in ["get", "post", "put", "delete", "patch"] {
                if item[method]["operationId"].as_str() == Some(operation.tool_name()) {
                    ensure!(selected.is_none(), "UCP REST operation is ambiguous");
                    selected = Some((path.as_str(), method, &item[method]));
                }
            }
        }
        let (path, method, endpoint) =
            selected.context("UCP REST schema does not declare this operation")?;
        validate_route(operation, path, method)?;
        let mut resources = resources;
        resources.insert(source.into(), document.clone());
        ensure!(
            resources.len() <= 128,
            "UCP schema closure exceeds document bound"
        );
        let mut bytes = 0;
        for (url, value) in &resources {
            https_url(url)?;
            bounded_json(value, 512 * 1024)?;
            bytes += serde_json::to_vec(value)?.len();
            ensure!(
                bytes <= 8 * 1024 * 1024,
                "UCP schema closure exceeds byte bound"
            );
        }
        let annotations = match operation {
            Operation::CreateCart | Operation::CreateCheckout => "create",
            Operation::UpdateCart | Operation::UpdateCheckout => "update",
            Operation::CompleteCheckout => "complete",
            Operation::CancelCart | Operation::CancelCheckout => "cancel",
            Operation::SearchCatalog => "search",
            Operation::LookupCatalog => "lookup",
            Operation::GetProduct => "get_product",
            _ => "read",
        };
        let extensions = extension_refs(negotiated, operation, &resources)?;
        let pointer_path = path.replace('~', "~0").replace('/', "~1");
        let base_pointer = format!("{source}#/paths/{pointer_path}/{method}");
        let input = if endpoint.get("requestBody").is_some() {
            let reference = format!("{base_pointer}/requestBody/content/application~1json/schema");
            let schema = compose(reference, extensions.values());
            Some(project(
                &schema,
                source,
                &resources,
                Direction::Request,
                annotations,
            )?)
        } else {
            None
        };
        let responses = endpoint["responses"]
            .as_object()
            .context("UCP REST responses missing")?;
        let mut outputs = BTreeMap::new();
        for (status, response) in responses {
            if response["content"]["application/json"]
                .get("schema")
                .is_none()
            {
                continue;
            }
            let reference =
                format!("{base_pointer}/responses/{status}/content/application~1json/schema");
            // A business error has no resource onto which an extension can be
            // composed. Validation selects that shape at observation time.
            let schema = json!({"$ref": reference});
            outputs.insert(
                status.clone(),
                project(
                    &schema,
                    source,
                    &resources,
                    Direction::Response,
                    annotations,
                )?,
            );
        }
        ensure!(
            !outputs.is_empty(),
            "UCP REST schema has no JSON response schema"
        );
        let mut extension_validators = BTreeMap::new();
        for (name, reference) in extensions {
            extension_validators.insert(
                name,
                project(
                    &json!({"$ref": reference}),
                    source,
                    &resources,
                    Direction::Response,
                    annotations,
                )?,
            );
        }
        Ok(Self {
            input,
            outputs,
            extensions: extension_validators,
        })
    }

    pub fn validate_request(&self, request: &RequestPlan) -> Result<()> {
        match (&self.input, request.rest_body()) {
            (Some(schema), Some(body)) => schema.validate(body).map_err(|error| {
                anyhow!(
                    "UCP REST input rejected {} at {}",
                    error.kind().keyword(),
                    error.instance_path()
                )
            }),
            (None, None) => Ok(()),
            _ => Err(anyhow!(
                "UCP REST request body does not match the service schema"
            )),
        }
    }

    pub fn validate_response(&self, status: u16, resource: &Value) -> Result<()> {
        let key = status.to_string();
        let group = format!("{}XX", status / 100);
        let schema = self
            .outputs
            .get(&key)
            .or_else(|| self.outputs.get(&group))
            .or_else(|| self.outputs.get("default"))
            .context("UCP REST response status was not declared")?;
        schema.validate(resource).map_err(|error| {
            anyhow!(
                "UCP REST response rejected {} at {}",
                error.kind().keyword(),
                error.instance_path()
            )
        })?;
        if resource["ucp"]["status"] != "error" {
            for (name, schema) in &self.extensions {
                if resource["ucp"]["capabilities"].get(name).is_some() {
                    schema.validate(resource).map_err(|error| {
                        anyhow!(
                            "UCP REST extension rejected {} at {}",
                            error.kind().keyword(),
                            error.instance_path()
                        )
                    })?;
                }
            }
        }
        Ok(())
    }
}

fn compose<'a>(reference: String, extensions: impl Iterator<Item = &'a String>) -> Value {
    let mut parts = vec![json!({"$ref": reference})];
    parts.extend(extensions.map(|reference| json!({"$ref": reference})));
    json!({"allOf": parts})
}

fn project(
    schema: &Value,
    source: &str,
    resources: &BTreeMap<String, Value>,
    direction: Direction,
    operation: &str,
) -> Result<Validator> {
    // Resolve references before applying the upstream allOf semantics, so an
    // extension can annotate fields inherited through its base's $ref.
    let expanded = inline_schema(
        schema,
        &https_url(source)?,
        resources,
        &mut BTreeSet::new(),
        &mut 0,
        0,
    )?;
    let resolved = schema_resolver::resolve(&expanded, &ResolveOptions::new(direction, operation))
        .map_err(|_| anyhow!("invalid UCP schema annotations"))?;
    compile(&resolved, SuppliedDocuments::new(BTreeMap::new()))
}

/// In-memory adaptation of upstream loader.rs's reference inlining. There is
/// no loader, implicit URL remapping, or unbounded recursive expansion here.
fn inline_schema(
    value: &Value,
    base: &url::Url,
    documents: &BTreeMap<String, Value>,
    active: &mut BTreeSet<String>,
    nodes: &mut usize,
    depth: usize,
) -> Result<Value> {
    *nodes += 1;
    ensure!(
        *nodes <= 32768 && depth <= 48,
        "UCP schema expansion exceeds complexity bound"
    );
    match value {
        Value::Object(object) => {
            let base = if let Some(id) = object.get("$id").and_then(Value::as_str) {
                let resolved = base.join(id).context("invalid schema id")?;
                https_url(resolved.as_str())?
            } else {
                base.clone()
            };
            ensure!(
                !object.contains_key("$dynamicRef") && !object.contains_key("$recursiveRef"),
                "dynamic schema references need an explicitly supported resolver"
            );
            let mut expanded = serde_json::Map::new();
            for (name, child) in object {
                if !matches!(name.as_str(), "$id" | "$ref" | "$defs" | "definitions") {
                    expanded.insert(
                        name.clone(),
                        inline_schema(child, &base, documents, active, nodes, depth + 1)?,
                    );
                }
            }
            if let Some(reference) = object.get("$ref") {
                let url = base
                    .join(
                        reference
                            .as_str()
                            .context("schema reference must be a string")?,
                    )
                    .context("invalid schema reference")?;
                let key = url.to_string();
                ensure!(
                    active.insert(key.clone()),
                    "recursive UCP schema cannot be safely inlined"
                );
                let mut source = url.clone();
                source.set_fragment(None);
                https_url(source.as_str())?;
                let document = documents
                    .get(source.as_str())
                    .context("schema reference has not been explicitly supplied")?;
                let fragment = url.fragment().unwrap_or("");
                ensure!(
                    fragment.is_empty() || fragment.starts_with('/'),
                    "schema anchor has not been explicitly bound"
                );
                let target = if fragment.is_empty() {
                    document
                } else {
                    document
                        .pointer(fragment)
                        .context("schema reference fragment does not exist")?
                };
                let target = inline_schema(target, &source, documents, active, nodes, depth + 1)?;
                active.remove(&key);
                if let Value::Object(target) = target {
                    // Preserve the original reference's siblings, matching the
                    // official UCP bundler's entry-or-insert precedence.
                    for (name, value) in target {
                        expanded.entry(name).or_insert(value);
                    }
                } else {
                    ensure!(
                        expanded.is_empty(),
                        "boolean schema reference has unsupported siblings"
                    );
                    return Ok(target);
                }
            }
            Ok(Value::Object(expanded))
        }
        Value::Array(values) => Ok(Value::Array(
            values
                .iter()
                .map(|value| inline_schema(value, base, documents, active, nodes, depth + 1))
                .collect::<Result<_>>()?,
        )),
        other => Ok(other.clone()),
    }
}

fn extension_refs(
    negotiated: &NegotiatedProfile,
    operation: Operation,
    resources: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, String>> {
    let mut references = BTreeMap::new();
    for (name, descriptor) in &negotiated.capabilities {
        let cart_checkout = name == super::CART && operation.capability() == super::CHECKOUT;
        if !cart_checkout && !parents(descriptor)?.contains(&operation.capability()) {
            continue;
        }
        let source = nonempty(&descriptor["schema"], "extension schema")?;
        let schema = resources
            .get(source)
            .context("negotiated extension schema has not been fetched")?;
        if let Some(requires) = schema.get("requires") {
            let requires = Requires::parse(requires)
                .map_err(|_| anyhow!("invalid UCP extension version requirements"))?;
            ensure!(
                requires
                    .protocol
                    .as_ref()
                    .is_none_or(|range| range.satisfied_by(&negotiated.version)),
                "negotiated extension requires a different UCP protocol version"
            );
            for (capability, range) in requires.capabilities {
                let selected = negotiated
                    .capabilities
                    .get(&capability)
                    .context("extension parent was not negotiated")?;
                ensure!(
                    range.satisfied_by(nonempty(&selected["version"], "parent version")?),
                    "negotiated extension requires a different parent version"
                );
            }
        }
        let definition = if cart_checkout {
            "checkout"
        } else {
            operation.capability()
        };
        ensure!(
            schema["$defs"].get(definition).is_some(),
            "extension does not define this parent capability"
        );
        super::validate_schema_authority(name, source)?;
        references.insert(
            name.clone(),
            format!(
                "{source}#/$defs/{}",
                definition.replace('~', "~0").replace('/', "~1")
            ),
        );
    }
    Ok(references)
}

/// Dependency URLs for an explicit bounded host fetch. Nested `$id` establishes
/// the reference base; anchors and local references stay in their own document.
pub fn referenced_schema_documents(source: &str, document: &Value) -> Result<BTreeSet<String>> {
    bounded_json(document, 512 * 1024)?;
    let mut urls = BTreeSet::new();
    let mut pending = vec![(document, https_url(source)?)];
    while let Some((value, mut base)) = pending.pop() {
        match value {
            Value::Object(object) => {
                if let Some(id) = object.get("$id").and_then(Value::as_str) {
                    base = base.join(id).context("invalid schema resource id")?;
                    https_url(base.as_str())?;
                }
                for reference_key in ["$ref", "$dynamicRef"] {
                    if let Some(reference) = object.get(reference_key).and_then(Value::as_str) {
                        let mut url = base.join(reference).context("invalid schema reference")?;
                        url.set_fragment(None);
                        https_url(url.as_str())?;
                        if url.as_str() != source {
                            urls.insert(url.into());
                        }
                    }
                }
                pending.extend(object.values().map(|value| (value, base.clone())));
            }
            Value::Array(array) => pending.extend(array.iter().map(|value| (value, base.clone()))),
            _ => {}
        }
    }
    Ok(urls)
}

fn validate_route(operation: Operation, path: &str, method: &str) -> Result<()> {
    let mut expected = format!("/{}", operation.rest_path().join("/"));
    if operation.needs_resource_id() {
        expected.push_str("/{id}");
    }
    if operation == Operation::CompleteCheckout {
        expected.push_str("/complete");
    }
    if matches!(operation, Operation::CancelCart | Operation::CancelCheckout) {
        expected.push_str("/cancel");
    }
    ensure!(
        path == expected && method.eq_ignore_ascii_case(operation.rest_method()),
        "advertised REST operation does not match the selected UCP binding"
    );
    Ok(())
}
