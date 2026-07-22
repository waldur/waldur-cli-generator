//! Direct OpenAPI-schema extraction, replacing the old syn-based reading of
//! rs-client's generated Rust source. The schema is the actual source of
//! truth for every operation's path, params, and request/response shape --
//! rs-client's generated code was always just an indirect (and sometimes
//! lossy) proxy for it.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct OpenApiDoc {
    pub paths: HashMap<String, HashMap<String, RawOperation>>,
    pub components: Components,
}

#[derive(Debug, Default, Deserialize)]
pub struct Components {
    #[serde(default)]
    pub parameters: HashMap<String, RawParameter>,
    #[serde(default)]
    pub schemas: HashMap<String, RawSchema>,
}

#[derive(Debug, Deserialize)]
pub struct RawOperation {
    #[serde(rename = "operationId")]
    pub operation_id: String,
    #[serde(default)]
    pub parameters: Vec<RawParamOrRef>,
    #[serde(rename = "requestBody", default)]
    pub request_body: Option<RawRequestBody>,
}

/// A parameter entry is either inline or a bare `{"$ref": "..."}` (Waldur's
/// schema uses this for the shared `page`/`page_size` params).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RawParamOrRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Inline(RawParameter),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawParameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: String,
    #[serde(default)]
    pub required: bool,
    pub schema: RawSchema,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawSchema {
    #[serde(rename = "type", default)]
    pub schema_type: Option<String>,
    #[serde(default)]
    pub items: Option<Box<RawSchema>>,
    #[serde(rename = "$ref", default)]
    pub reference: Option<String>,
    #[serde(rename = "enum", default)]
    pub enum_values: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct RawRequestBody {
    pub content: HashMap<String, RawMediaType>,
}

#[derive(Debug, Deserialize)]
pub struct RawMediaType {
    pub schema: RawSchema,
}

/// Loads and parses an OpenAPI schema file (YAML or JSON -- serde_yaml
/// accepts both).
pub fn load(path: &std::path::Path) -> Result<OpenApiDoc> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading OpenAPI schema at {}", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("parsing OpenAPI schema at {}", path.display()))
}

/// How a query/path parameter should be represented as a CLI flag. Mirrors
/// the shapes waldur-cli's generated Args structs actually need -- kept
/// intentionally narrow (a handful of scalar kinds) rather than modeling the
/// full JSON Schema type system.
#[derive(Debug, Clone)]
pub enum ParamKind {
    RequiredStr,
    OptionalStr,
    RequiredBool,
    OptionalBool,
    RequiredI64,
    OptionalI64,
    /// A required/optional parameter whose schema type this generator
    /// doesn't know how to map to a CLI flag (e.g. an array of enums that
    /// isn't the `field` filter, which is handled separately).
    SkippedOptional,
    SkippedRequired,
}

#[derive(Debug, Clone)]
pub struct ExtractedParam {
    pub name: String,
    pub kind: ParamKind,
}

#[derive(Debug, Clone)]
pub struct ExtractedOperation {
    pub operation_id: String,
    /// The literal REST path, e.g. `/api/customers/{uuid}/`.
    pub path: String,
    /// `"get"` / `"post"` / `"put"` / `"delete"`.
    pub http_verb: String,
    /// Name of the `in: path` parameter, if any (e.g. `"uuid"`). At most one
    /// per operation -- asserted at extraction time.
    pub path_param: Option<String>,
    /// Query parameters, in schema order, excluding `page`/`page_size` and
    /// the `field` filter (handled separately via `field_enum_name`).
    pub query_params: Vec<ExtractedParam>,
    /// Name of the schema this operation's `field` query param's items
    /// resolve to (e.g. `"CustomerFieldEnum"`), if it has one.
    pub field_enum_name: Option<String>,
    /// Name of the request body schema (e.g. `"CustomerRequest"`), resolved
    /// per-operation -- never guessed from the resource name, since it
    /// genuinely diverges (see e.g. `role.rs`'s `RoleModifyRequest`).
    pub request_body_type: Option<String>,
}

fn schema_type_name(schema: &RawSchema) -> Option<&str> {
    schema.schema_type.as_deref()
}

fn classify_param(param: &RawParameter) -> ParamKind {
    let required = param.required;
    match schema_type_name(&param.schema) {
        Some("string") => {
            if required {
                ParamKind::RequiredStr
            } else {
                ParamKind::OptionalStr
            }
        }
        Some("boolean") => {
            if required {
                ParamKind::RequiredBool
            } else {
                ParamKind::OptionalBool
            }
        }
        Some("integer") => {
            if required {
                ParamKind::RequiredI64
            } else {
                ParamKind::OptionalI64
            }
        }
        _ => {
            if required {
                ParamKind::SkippedRequired
            } else {
                ParamKind::SkippedOptional
            }
        }
    }
}

/// Resolves a `$ref` like `#/components/parameters/Page` against
/// `components.parameters`.
fn resolve_param<'a>(doc: &'a OpenApiDoc, reference: &str) -> Result<&'a RawParameter> {
    let name = reference
        .strip_prefix("#/components/parameters/")
        .with_context(|| format!("unsupported parameter $ref shape: `{reference}`"))?;
    doc.components
        .parameters
        .get(name)
        .with_context(|| format!("$ref `{reference}` does not resolve to a known parameter"))
}

/// Strips a `#/components/schemas/` prefix off a `$ref`, returning the bare
/// schema name (e.g. `CustomerRequest`).
fn schema_ref_name(reference: &str) -> Option<&str> {
    reference.strip_prefix("#/components/schemas/")
}

/// Finds the operation with the given `operationId` anywhere in the schema's
/// paths, and extracts everything waldur-cli's generator needs from it.
pub fn extract_operation(doc: &OpenApiDoc, operation_id: &str) -> Result<ExtractedOperation> {
    let mut found: Option<(&str, &str, &RawOperation)> = None;
    for (path, methods) in &doc.paths {
        for (verb, op) in methods {
            if op.operation_id == operation_id {
                found = Some((path.as_str(), verb.as_str(), op));
            }
        }
    }
    let (path, http_verb, op) = found
        .with_context(|| format!("operationId `{operation_id}` not found in OpenAPI schema"))?;

    let mut path_param: Option<String> = None;
    let mut query_params = Vec::new();
    let mut field_enum_name: Option<String> = None;

    for entry in &op.parameters {
        let param: std::borrow::Cow<RawParameter> = match entry {
            RawParamOrRef::Inline(p) => std::borrow::Cow::Borrowed(p),
            RawParamOrRef::Ref { reference } => {
                std::borrow::Cow::Owned(resolve_param(doc, reference)?.clone())
            }
        };

        match param.location.as_str() {
            "path" => {
                if path_param.is_some() {
                    bail!(
                        "operation `{operation_id}` has more than one `in: path` parameter -- \
                         this generator only supports a single path parameter per operation"
                    );
                }
                path_param = Some(param.name.clone());
            }
            "query" => {
                if param.name == "page" || param.name == "page_size" {
                    continue;
                }
                if param.name == "field" {
                    field_enum_name = param
                        .schema
                        .items
                        .as_ref()
                        .and_then(|items| items.reference.as_deref())
                        .and_then(schema_ref_name)
                        .map(|s| s.to_string());
                    continue;
                }
                query_params.push(ExtractedParam {
                    name: param.name.clone(),
                    kind: classify_param(&param),
                });
            }
            _ => {}
        }
    }

    let request_body_type = op
        .request_body
        .as_ref()
        .and_then(|body| body.content.get("application/json"))
        .and_then(|media| media.schema.reference.as_deref())
        .and_then(schema_ref_name)
        .map(|s| s.to_string());

    Ok(ExtractedOperation {
        operation_id: operation_id.to_string(),
        path: path.to_string(),
        http_verb: http_verb.to_string(),
        path_param,
        query_params,
        field_enum_name,
        request_body_type,
    })
}

/// Resolves a flat `{type: string, enum: [...]}` schema's values by name
/// (e.g. `"CustomerFieldEnum"`).
pub fn extract_enum_values(doc: &OpenApiDoc, schema_name: &str) -> Result<Vec<String>> {
    let schema = doc
        .components
        .schemas
        .get(schema_name)
        .with_context(|| format!("schema `{schema_name}` not found in OpenAPI schema"))?;
    schema
        .enum_values
        .clone()
        .with_context(|| format!("schema `{schema_name}` has no `enum` values"))
}
