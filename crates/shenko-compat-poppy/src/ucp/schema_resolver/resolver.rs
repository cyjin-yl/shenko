// Derived from ucp-schema 1.4.1 (Apache-2.0). See README.md and LICENSE.
// Modified for Shenko: private modules and only the pure resolution API.

//! Schema resolution - transforms UCP annotated schemas into standard JSON Schema.

use serde_json::{Map, Value};

use super::error::ResolveError;
use super::types::{
    Direction, ResolveOptions, SchemaTransitionInfo, UCP_ANNOTATIONS, Visibility,
    is_valid_schema_transition, json_type_name,
};

/// Resolve a schema for a specific direction and operation.
///
/// Returns a standard JSON Schema with UCP annotations removed.
/// When `options.strict` is true, sets `additionalProperties: false`
/// on all object schemas to reject unknown fields. Default is false
/// to respect UCP's extensibility model.
///
/// # Errors
///
/// Returns `ResolveError` if the schema contains invalid annotations.
pub fn resolve(schema: &Value, options: &ResolveOptions) -> Result<Value, ResolveError> {
    let mut resolved = resolve_value(schema, options, "")?;

    if options.strict {
        close_additional_properties(&mut resolved);
    }

    Ok(resolved)
}

/// Recursively close object schemas to reject unknown properties.
///
/// For simple object schemas: sets `additionalProperties: false`
/// For schemas with composition (allOf/anyOf/oneOf): sets `unevaluatedProperties: false`
///
/// The distinction matters because `additionalProperties` is evaluated per-schema,
/// while `unevaluatedProperties` (JSON Schema 2020-12) looks across all subschemas.
/// This allows $ref inheritance patterns to work correctly in strict mode.
fn close_additional_properties(value: &mut Value) {
    close_additional_properties_inner(value, false);
}

/// Inner implementation with context tracking.
///
/// `in_composition_branch` is true when processing direct children of allOf/anyOf/oneOf.
/// We skip setting additionalProperties on these because each branch is validated
/// independently and doesn't see properties from sibling branches.
fn close_additional_properties_inner(value: &mut Value, in_composition_branch: bool) {
    if let Value::Object(map) = value {
        // Check if this schema uses composition keywords
        let has_composition =
            map.contains_key("allOf") || map.contains_key("anyOf") || map.contains_key("oneOf");

        // Check if this is an object schema (has "type": "object" or has "properties")
        let is_object_schema = map
            .get("type")
            .and_then(|t| t.as_str())
            .map(|t| t == "object")
            .unwrap_or(false)
            || map.contains_key("properties");

        // Close the schema if we're not inside a composition branch
        if !in_composition_branch && (is_object_schema || has_composition) {
            if has_composition {
                // Use unevaluatedProperties for composition - it looks across all subschemas
                // so $ref inheritance works correctly
                match map.get("unevaluatedProperties") {
                    None => {
                        map.insert("unevaluatedProperties".to_string(), Value::Bool(false));
                    }
                    Some(Value::Bool(true)) => {
                        map.insert("unevaluatedProperties".to_string(), Value::Bool(false));
                    }
                    _ => {}
                }
            } else {
                // Simple object schema - use additionalProperties
                match map.get("additionalProperties") {
                    None => {
                        map.insert("additionalProperties".to_string(), Value::Bool(false));
                    }
                    Some(Value::Bool(true)) => {
                        map.insert("additionalProperties".to_string(), Value::Bool(false));
                    }
                    _ => {}
                }
            }
        }

        // Recurse into all values
        for (key, child) in map.iter_mut() {
            match key.as_str() {
                "properties" => {
                    // Recurse into each property definition
                    if let Value::Object(props) = child {
                        for prop_value in props.values_mut() {
                            close_additional_properties_inner(prop_value, false);
                        }
                    }
                }
                "items" | "additionalProperties" | "unevaluatedProperties" => {
                    // Schema values - recurse
                    close_additional_properties_inner(child, false);
                }
                "$defs" | "definitions" => {
                    // Definitions - recurse into each
                    if let Value::Object(defs) = child {
                        for def_value in defs.values_mut() {
                            close_additional_properties_inner(def_value, false);
                        }
                    }
                }
                "allOf" | "anyOf" | "oneOf" => {
                    // Composition branches - recurse but mark as in_composition
                    // so we don't set additionalProperties on them directly
                    if let Value::Array(arr) = child {
                        for item in arr {
                            close_additional_properties_inner(item, true);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Get visibility for a single property.
///
/// Looks up the appropriate annotation (`ucp_request` or `ucp_response`) and
/// determines the visibility for the given operation.
///
/// # Errors
///
/// Returns `ResolveError` if the annotation has invalid type or unknown visibility value.
pub fn get_visibility(
    prop: &Value,
    direction: Direction,
    operation: &str,
    path: &str,
) -> Result<(Visibility, Option<SchemaTransitionInfo>), ResolveError> {
    let key = direction.annotation_key();
    let Some(annotation) = prop.get(key) else {
        return Ok((Visibility::Include, None));
    };
    get_visibility_from_annotation(annotation, operation, path)
}

/// Parse visibility (and optional transition info) from a raw annotation value.
///
/// Shared between `get_visibility` (which extracts annotation by direction key)
/// and `inject_annotations` (which already has the annotation from allOf propagation).
fn get_visibility_from_annotation(
    annotation: &Value,
    operation: &str,
    path: &str,
) -> Result<(Visibility, Option<SchemaTransitionInfo>), ResolveError> {
    match annotation {
        // Shorthand: "ucp_request": "omit" - applies to all operations
        Value::String(s) => Ok((parse_visibility_string(s, path)?, None)),

        // Object form: "ucp_request": { "create": "omit", "update": "required" }
        Value::Object(map) => {
            // Lookup operation (already lowercase from ResolveOptions)
            match map.get(operation) {
                Some(Value::String(s)) => Ok((parse_visibility_string(s, path)?, None)),
                Some(Value::Object(obj)) => {
                    parse_transition_value(obj, &format!("{}/{}", path, operation))
                }
                Some(other) => Err(ResolveError::InvalidAnnotationType {
                    path: format!("{}/{}", path, operation),
                    actual: json_type_name(other).to_string(),
                }),
                None => {
                    // Check for shorthand transition form
                    if let Some(Value::Object(t)) = map.get("transition") {
                        parse_transition_value(t, path)
                    } else {
                        Ok((Visibility::Include, None))
                    }
                }
            }
        }

        // Invalid type
        other => Err(ResolveError::InvalidAnnotationType {
            path: path.to_string(),
            actual: json_type_name(other).to_string(),
        }),
    }
}

fn parse_transition_value(
    obj: &Map<String, Value>,
    path: &str,
) -> Result<(Visibility, Option<SchemaTransitionInfo>), ResolveError> {
    let t = obj
        .get("transition")
        .and_then(|v| v.as_object())
        .unwrap_or(obj);

    let from = t.get("from").and_then(|v| v.as_str()).unwrap_or("");
    let to = t.get("to").and_then(|v| v.as_str()).unwrap_or("");
    let description = t.get("description").and_then(|v| v.as_str()).unwrap_or("");

    if description.is_empty() {
        return Err(ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: "missing required field \"description\"".to_string(),
        });
    }
    if !is_valid_schema_transition(from, to) {
        return Err(ResolveError::InvalidSchemaTransition {
            path: path.to_string(),
            message: format!(
                "\"from\" ({}) and \"to\" ({}) must be distinct visibility values",
                from, to
            ),
        });
    }

    let vis = parse_visibility_string(from, path)?;
    Ok((
        vis,
        Some(SchemaTransitionInfo {
            from: from.to_string(),
            to: to.to_string(),
            description: description.to_string(),
        }),
    ))
}

/// Strip all UCP annotations from a schema.
///
/// Recursively removes `ucp_request` and `ucp_response`.
pub fn strip_annotations(schema: &Value) -> Value {
    strip_annotations_recursive(schema)
}

// --- Internal implementation ---

fn resolve_value(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    match value {
        Value::Object(map) => resolve_object(map, options, path),
        Value::Array(arr) => resolve_array(arr, options, path),
        // Primitives pass through unchanged
        other => Ok(other.clone()),
    }
}

fn resolve_object(
    map: &Map<String, Value>,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let mut result = Map::new();

    // Track required array modifications
    let original_required: Vec<String> = map
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut new_required: Vec<String> = original_required.clone();

    for (key, value) in map {
        // Skip UCP annotations in output
        if UCP_ANNOTATIONS.contains(&key.as_str()) {
            continue;
        }

        let child_path = format!("{}/{}", path, key);

        match key.as_str() {
            "properties" => {
                let resolved = resolve_properties(value, options, &child_path, &mut new_required)?;
                result.insert(key.clone(), resolved);
            }
            "items" => {
                // Array items - recurse
                let resolved = resolve_value(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
            "$defs" | "definitions" => {
                // Definitions - recurse into each definition
                let resolved = resolve_defs(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
            "allOf" => {
                // allOf gets special handling: annotations from later branches
                // propagate to earlier branches (last-writer-wins), enabling
                // extension schemas to control visibility of inherited fields.
                let resolved = resolve_allof(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
            "anyOf" | "oneOf" => {
                // anyOf/oneOf branches are independent alternatives —
                // no annotation propagation across branches.
                let resolved = resolve_composition(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
            "additionalProperties" => {
                // If it's a schema (object), recurse; otherwise keep as-is
                if value.is_object() {
                    let resolved = resolve_value(value, options, &child_path)?;
                    result.insert(key.clone(), resolved);
                } else {
                    result.insert(key.clone(), value.clone());
                }
            }
            "required" => {
                // Will be handled at the end after processing properties
                continue;
            }
            _ => {
                // Other keys - recurse if object/array, otherwise copy
                let resolved = resolve_value(value, options, &child_path)?;
                result.insert(key.clone(), resolved);
            }
        }
    }

    // Add updated required array if non-empty or if original existed
    if !new_required.is_empty() || map.contains_key("required") {
        result.insert(
            "required".to_string(),
            Value::Array(new_required.into_iter().map(Value::String).collect()),
        );
    }

    Ok(Value::Object(result))
}

fn resolve_properties(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
    required: &mut Vec<String>,
) -> Result<Value, ResolveError> {
    let Some(props) = value.as_object() else {
        return Ok(value.clone());
    };

    let mut result = Map::new();

    for (prop_name, prop_value) in props {
        let prop_path = format!("{}/{}", path, prop_name);

        // Get visibility for this property
        let (visibility, transition) = get_visibility(
            prop_value,
            options.direction,
            &options.operation,
            &prop_path,
        )?;

        match visibility {
            Visibility::Omit => {
                // Include future fields: currently omit but transitioning to non-omit.
                // Completes transition lifecycle symmetry — deprecations (to=omit) are
                // already surfaced; this surfaces planned additions (from=omit).
                let is_future =
                    options.include_future && transition.as_ref().is_some_and(|t| t.to != "omit");

                if is_future {
                    let resolved = resolve_value(prop_value, options, &prop_path)?;
                    let mut stripped = strip_annotations(&resolved);
                    apply_transition_metadata(&mut stripped, &transition);
                    result.insert(prop_name.clone(), stripped);
                    // NOT added to required — current visibility is omit
                }
                required.retain(|r| r != prop_name);
            }
            Visibility::Required => {
                // Keep property, ensure in required
                let resolved = resolve_value(prop_value, options, &prop_path)?;
                let mut stripped = strip_annotations(&resolved);
                apply_transition_metadata(&mut stripped, &transition);
                result.insert(prop_name.clone(), stripped);
                if !required.contains(prop_name) {
                    required.push(prop_name.clone());
                }
            }
            Visibility::Optional => {
                // Keep property, remove from required
                let resolved = resolve_value(prop_value, options, &prop_path)?;
                let mut stripped = strip_annotations(&resolved);
                apply_transition_metadata(&mut stripped, &transition);
                result.insert(prop_name.clone(), stripped);
                required.retain(|r| r != prop_name);
            }
            Visibility::Include => {
                // Keep as-is (preserve original required status)
                let resolved = resolve_value(prop_value, options, &prop_path)?;
                let mut stripped = strip_annotations(&resolved);
                apply_transition_metadata(&mut stripped, &transition);
                result.insert(prop_name.clone(), stripped);
            }
        }
    }

    Ok(Value::Object(result))
}

fn resolve_defs(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let Some(defs) = value.as_object() else {
        return Ok(value.clone());
    };

    let mut result = Map::new();
    for (name, def) in defs {
        let def_path = format!("{}/{}", path, name);
        let resolved = resolve_value(def, options, &def_path)?;
        result.insert(name.clone(), resolved);
    }

    Ok(Value::Object(result))
}

fn resolve_array(
    arr: &[Value],
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let mut result = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let item_path = format!("{}/{}", path, i);
        let resolved = resolve_value(item, options, &item_path)?;
        result.push(resolved);
    }
    Ok(Value::Array(result))
}

fn resolve_composition(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let Some(arr) = value.as_array() else {
        return Ok(value.clone());
    };

    let mut result = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let item_path = format!("{}/{}", path, i);
        let resolved = resolve_value(item, options, &item_path)?;
        result.push(resolved);
    }

    Ok(Value::Array(result))
}

/// allOf-specific resolution with cross-branch annotation propagation.
///
/// Three-phase approach:
/// 1. **Collect**: scan all branches for annotations (last-writer-wins)
/// 2. **Validate**: check for type conflicts across branches
/// 3. **Inject + Resolve**: copy collected annotations into branches that lack them,
///    enforcing monotonicity (extensions cannot weaken required fields), then resolve
///
/// Why last-writer-wins: in UCP's allOf convention, the base schema is allOf[0]
/// and extensions follow. Later branches (extensions) should override earlier ones.
fn resolve_allof(
    value: &Value,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let Some(arr) = value.as_array() else {
        return Ok(value.clone());
    };

    let ann_key = options.direction.annotation_key();
    let merged = collect_allof_annotations(arr, ann_key);
    validate_allof_types(arr, path)?;

    let mut result = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let item_path = format!("{}/{}", path, i);
        let item = if !merged.is_empty() {
            inject_annotations(item, &merged, ann_key, options, &item_path)?
        } else {
            item.clone()
        };
        let resolved = resolve_value(&item, options, &item_path)?;
        result.push(resolved);
    }

    Ok(Value::Array(result))
}

/// Scan allOf branches and collect annotations per property (last-writer-wins).
///
/// Returns a map of property_name → annotation_value for properties that have
/// a UCP annotation in any branch. When multiple branches annotate the same
/// property, the last branch's annotation wins.
fn collect_allof_annotations(branches: &[Value], ann_key: &str) -> Map<String, Value> {
    let mut merged = Map::new();
    for branch in branches {
        let props = branch
            .as_object()
            .and_then(|o| o.get("properties"))
            .and_then(|p| p.as_object());
        if let Some(props) = props {
            for (name, prop) in props {
                if let Some(ann) = prop.as_object().and_then(|p| p.get(ann_key)) {
                    merged.insert(name.clone(), ann.clone());
                }
            }
        }
    }
    merged
}

/// Inject collected annotations into a branch's properties where they're missing.
///
/// Enforces monotonicity: if a field is `required` in a base branch's required
/// array, an extension annotation cannot weaken it to `omit` or `optional`.
///
/// | base required? | extension annotation | result     |
/// |---------------|---------------------|------------|
/// | yes           | required            | OK         |
/// | yes           | optional            | ERROR      |
/// | yes           | omit                | ERROR      |
/// | no            | any                 | OK         |
fn inject_annotations(
    branch: &Value,
    annotations: &Map<String, Value>,
    ann_key: &str,
    options: &ResolveOptions,
    path: &str,
) -> Result<Value, ResolveError> {
    let mut branch = branch.clone();

    let base_required: Vec<String> = branch
        .as_object()
        .and_then(|o| o.get("required"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if let Some(props) = branch
        .as_object_mut()
        .and_then(|o| o.get_mut("properties"))
        .and_then(|p| p.as_object_mut())
    {
        for (name, ann) in annotations {
            if let Some(prop) = props.get_mut(name) {
                if let Some(obj) = prop.as_object_mut() {
                    // Skip if this property already has its own annotation
                    if obj.contains_key(ann_key) {
                        continue;
                    }

                    // Monotonicity check: required fields cannot be weakened
                    if base_required.contains(name) {
                        let (vis, _) = get_visibility_from_annotation(
                            ann,
                            &options.operation,
                            &format!("{}/properties/{}", path, name),
                        )?;
                        if matches!(vis, Visibility::Omit | Visibility::Optional) {
                            return Err(ResolveError::MonotonicityViolation {
                                path: format!("{}/properties/{}", path, name),
                                field: name.clone(),
                                base_status: "required".into(),
                                attempted: match vis {
                                    Visibility::Omit => "omit",
                                    Visibility::Optional => "optional",
                                    Visibility::Required => "required",
                                    Visibility::Include => "include",
                                }
                                .into(),
                            });
                        }
                    }

                    obj.insert(ann_key.to_string(), ann.clone());
                }
            }
        }
    }

    Ok(branch)
}

/// Validate that allOf branches don't declare contradictory types on the same property.
///
/// Only checks string-form `"type"` values. Array-form types (e.g. `["string", "null"]`)
/// are intentionally skipped — they're rare and the semantic comparison is non-trivial.
fn validate_allof_types(branches: &[Value], path: &str) -> Result<(), ResolveError> {
    let mut prop_types: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for branch in branches {
        let props = branch
            .as_object()
            .and_then(|o| o.get("properties"))
            .and_then(|p| p.as_object());
        if let Some(props) = props {
            for (name, prop) in props {
                if let Some(type_val) = prop.as_object().and_then(|p| p.get("type")) {
                    if let Some(type_str) = type_val.as_str() {
                        if let Some(existing) = prop_types.get(name) {
                            if existing != type_str {
                                return Err(ResolveError::TypeConflict {
                                    path: format!("{}/properties/{}", path, name),
                                    base_type: existing.clone(),
                                    ext_type: type_str.to_string(),
                                });
                            }
                        } else {
                            prop_types.insert(name.clone(), type_str.to_string());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn strip_annotations_recursive(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut result = Map::new();
            for (k, v) in map {
                if !UCP_ANNOTATIONS.contains(&k.as_str()) {
                    result.insert(k.clone(), strip_annotations_recursive(v));
                }
            }
            Value::Object(result)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(strip_annotations_recursive).collect()),
        other => other.clone(),
    }
}

fn apply_transition_metadata(value: &mut Value, transition: &Option<SchemaTransitionInfo>) {
    if let (Value::Object(map), Some(info)) = (value, transition) {
        map.insert(
            "x-ucp-schema-transition".to_string(),
            serde_json::json!({
                "from": info.from,
                "to": info.to,
                "description": info.description,
            }),
        );
        if info.to == "omit" {
            map.insert("deprecated".to_string(), Value::Bool(true));
        }
    }
}

fn parse_visibility_string(s: &str, path: &str) -> Result<Visibility, ResolveError> {
    Visibility::parse(s).ok_or_else(|| ResolveError::UnknownVisibility {
        path: path.to_string(),
        value: s.to_string(),
    })
}
