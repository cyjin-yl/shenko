use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context, Result, anyhow, ensure};
use jsonschema::{PatternOptions, Retrieve, Uri, Validator};
use serde_json::Value;

use super::{NegotiatedProfile, Operation, RequestPlan, bounded_json, https_url};

/// Validates the actual merchant's MCP input/output schemas. Network and file
/// retrieval are disabled even if another workspace crate enables jsonschema's
/// default features: references resolve only to explicitly supplied documents.
pub struct ToolSchemas {
    tools: BTreeMap<String, ToolSchema>,
}

struct ToolSchema {
    input: Validator,
    output: Option<Validator>,
}

#[derive(Clone)]
pub(super) struct SuppliedDocuments(Arc<BTreeMap<String, Value>>);

impl SuppliedDocuments {
    pub(super) fn new(documents: BTreeMap<String, Value>) -> Self {
        Self(Arc::new(documents))
    }
}

impl Retrieve for SuppliedDocuments {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        self.0
            .get(uri.as_str())
            .cloned()
            .ok_or_else(|| "schema reference has not been explicitly supplied".into())
    }
}

impl ToolSchemas {
    /// `documents` are schema resources fetched by the host with the same
    /// authority, HTTPS, address-pinning and no-redirect rules as discovery.
    /// This function itself cannot fetch a missing reference.
    pub fn from_tools_list(list: &Value, documents: BTreeMap<String, Value>) -> Result<Self> {
        bounded_json(list, 512 * 1024)?;
        ensure!(documents.len() <= 128, "too many schema documents");
        let mut bytes = 0_usize;
        for (uri, document) in &documents {
            https_url(uri)?;
            bounded_json(document, 512 * 1024)?;
            bytes = bytes.saturating_add(serde_json::to_vec(document)?.len());
            ensure!(
                bytes <= 8 * 1024 * 1024,
                "schema document set exceeds size bound"
            );
        }
        let documents = SuppliedDocuments(Arc::new(documents));
        let list = list.get("result").unwrap_or(list);
        let tools = list["tools"]
            .as_array()
            .context("MCP tools/list response missing tools")?;
        ensure!(tools.len() <= 128, "too many MCP tools");
        let mut result = Self {
            tools: BTreeMap::new(),
        };
        for tool in tools {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            if serde_json::from_value::<Operation>(Value::String(name.into())).is_err() {
                continue;
            }
            ensure!(
                !result.tools.contains_key(name),
                "MCP tool name is duplicated"
            );
            let input = tool
                .get("inputSchema")
                .context("MCP tool has no input schema")?;
            let input =
                compile(input, documents.clone()).context("invalid merchant input schema")?;
            let output = tool
                .get("outputSchema")
                .map(|schema| compile(schema, documents.clone()))
                .transpose()
                .context("invalid merchant output schema")?;
            result
                .tools
                .insert(name.into(), ToolSchema { input, output });
        }
        Ok(result)
    }

    pub fn contains(&self, operation: Operation) -> bool {
        self.tools.contains_key(operation.tool_name())
    }

    pub fn validate_request(&self, request: &RequestPlan) -> Result<()> {
        let schema = self
            .tools
            .get(request.operation.tool_name())
            .context("merchant does not expose the requested UCP tool")?;
        schema.input.validate(request.arguments()).map_err(|error| {
            // Error Display can contain the submitted payment credential. Only
            // the location and validation keyword are safe diagnostic details.
            anyhow!(
                "UCP input schema rejected {} at {}",
                error.kind().keyword(),
                error.instance_path()
            )
        })
    }

    /// Accept either a raw JSON-RPC response or the MCP SDK's CallToolResult.
    /// A successful HTTP exchange is not enough: JSON-RPC errors, isError,
    /// UCP status, messages and the checkout lifecycle remain separate facts.
    pub fn response_resource(
        &self,
        negotiated: &NegotiatedProfile,
        operation: Operation,
        response: &Value,
    ) -> Result<Value> {
        bounded_json(response, 512 * 1024)?;
        ensure!(
            response.get("error").is_none(),
            "MCP returned a protocol error"
        );
        let result = response.get("result").unwrap_or(response);
        ensure!(
            result["isError"].as_bool() != Some(true),
            "MCP tool reported an error"
        );
        let resource = if let Some(resource) = result.get("structuredContent") {
            resource.clone()
        } else {
            // Older MCP servers send only the JSON text form. Do not concatenate
            // unrelated text blocks or interpret arbitrary prose as a receipt.
            let content = result["content"]
                .as_array()
                .context("MCP result has no structured content")?;
            ensure!(
                content.len() == 1 && content[0]["type"].as_str() == Some("text"),
                "MCP UCP result has no unambiguous JSON text body"
            );
            let text = content[0]["text"]
                .as_str()
                .context("MCP text body missing")?;
            serde_json::from_str(text).context("MCP text body is not UCP JSON")?
        };
        bounded_json(&resource, 512 * 1024)?;
        negotiated.validate_response_capabilities(&resource, operation)?;
        if resource["ucp"]["status"].as_str() != Some("error") {
            let schema = self
                .tools
                .get(operation.tool_name())
                .context("unknown response tool")?;
            if let Some(output) = &schema.output {
                output.validate(&resource).map_err(|error| {
                    anyhow!(
                        "UCP output schema rejected {} at {}",
                        error.kind().keyword(),
                        error.instance_path()
                    )
                })?;
            }
        }
        // A UCP business error is returned intact, not raised as a transport
        // failure or silently converted into a successful order observation.
        Ok(resource)
    }
}

pub(super) fn compile(schema: &Value, documents: SuppliedDocuments) -> Result<Validator> {
    bounded_json(schema, 256 * 1024)?;
    jsonschema::options()
        .with_retriever(documents)
        // Keep the ECMA-compatible engine, with explicit finite per-pattern
        // execution and allocation bounds instead of relying on its defaults.
        .with_pattern_options(
            PatternOptions::fancy_regex()
                .backtrack_limit(1_000_000)
                .size_limit(2 * 1024 * 1024)
                .dfa_size_limit(2 * 1024 * 1024),
        )
        .should_validate_formats(true)
        .build(schema)
        .map_err(|_| anyhow!("merchant JSON Schema could not be compiled without implicit I/O"))
}
