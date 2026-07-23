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
    #[serde(default)]
    pub properties: Option<HashMap<String, RawSchema>>,
    #[serde(default)]
    pub required: Option<Vec<String>>,
    #[serde(rename = "readOnly", default)]
    pub read_only: bool,
    #[serde(rename = "allOf", default)]
    pub all_of: Option<Vec<RawSchema>>,
    #[serde(rename = "oneOf", default)]
    pub one_of: Option<Vec<RawSchema>>,
    #[serde(rename = "anyOf", default)]
    pub any_of: Option<Vec<RawSchema>>,
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
        // An array query param (e.g. offerings' `type`, `state`) is filtered
        // on the wire by repeating the key: `?type=A&type=B`. `--filter` is
        // already repeatable and pushes one query param per occurrence, so
        // exposing these as a plain string filter (`--filter type=A --filter
        // type=B`) maps exactly onto that -- no special array handling needed.
        Some("array") => {
            if required {
                ParamKind::RequiredStr
            } else {
                ParamKind::OptionalStr
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

/// Recursion cap for skeleton building -- guards against a self-referential
/// schema (`$ref` cycle the `seen` set doesn't cover, e.g. via an array of
/// the same type) producing an unbounded template.
const SKELETON_MAX_DEPTH: usize = 12;

/// Builds a fillable request-body template (AWS `--generate-cli-skeleton`
/// style): every writable field of `schema_name` with a type-appropriate
/// placeholder, as pretty-printed JSON. Emitted into the generated command
/// so `--generate-skeleton` can print it without any runtime schema access.
pub fn build_request_skeleton(doc: &OpenApiDoc, schema_name: &str) -> Result<String> {
    let value = skeleton_for(doc, schema_name)?;
    serde_json::to_string_pretty(&value)
        .with_context(|| format!("serializing skeleton for `{schema_name}`"))
}

/// The skeleton for a named schema as a `serde_json::Value` (rather than a
/// pretty string) -- lets callers compose skeletons, e.g. splice a resource's
/// typed attributes into the free-form `attributes` slot of an order body.
fn skeleton_for(doc: &OpenApiDoc, schema_name: &str) -> Result<serde_json::Value> {
    let schema = doc
        .components
        .schemas
        .get(schema_name)
        .with_context(|| format!("schema `{schema_name}` not found"))?;
    let mut seen = std::collections::HashSet::new();
    seen.insert(schema_name.to_string());
    Ok(skeleton_value(doc, schema, &mut seen, 0))
}

/// Builds the `--generate-skeleton` template for a `provision` command: the
/// marketplace `OrderCreateRequest` envelope, but with its free-form
/// `attributes` object replaced by the typed skeleton for this offering's
/// `{OfferingType}CreateOrderAttributes` schema (Waldur's naming convention,
/// e.g. `OpenStack.Instance` -> `OpenStackInstanceCreateOrderAttributes`).
/// `accepting_terms_of_service` is defaulted to `true` -- a CLI provision is
/// an explicit action, and leaving it unset can leave the order stuck pending
/// consumer approval.
pub fn build_order_skeleton(doc: &OpenApiDoc, offering_type: &str) -> Result<String> {
    let mut envelope = skeleton_for(doc, "OrderCreateRequest")?;
    let attrs_schema = format!("{}CreateOrderAttributes", offering_type.replace('.', ""));
    let attributes = skeleton_for(doc, &attrs_schema).with_context(|| {
        format!("no attributes schema `{attrs_schema}` for offering type `{offering_type}`")
    })?;
    let obj = envelope
        .as_object_mut()
        .context("OrderCreateRequest skeleton is not a JSON object")?;
    obj.insert("attributes".to_string(), attributes);
    obj.insert("accepting_terms_of_service".to_string(), serde_json::Value::Bool(true));
    serde_json::to_string_pretty(&envelope)
        .with_context(|| format!("serializing order skeleton for `{offering_type}`"))
}

/// A type-appropriate placeholder for one schema node. Mirrors AWS's skeleton
/// convention: empty typed values (`""`, `0`, `false`), a single sample array
/// element, nested objects recursed into. Enums use their first value (a
/// valid example rather than an empty string the server would reject).
fn skeleton_value(
    doc: &OpenApiDoc,
    schema: &RawSchema,
    seen: &mut std::collections::HashSet<String>,
    depth: usize,
) -> serde_json::Value {
    use serde_json::Value;
    if depth > SKELETON_MAX_DEPTH {
        return Value::Null;
    }
    // Resolve $ref, guarding against cycles.
    if let Some(reference) = &schema.reference {
        let name = reference.rsplit('/').next().unwrap_or(reference).to_string();
        if seen.contains(&name) {
            return Value::Null;
        }
        if let Some(resolved) = doc.components.schemas.get(&name) {
            seen.insert(name.clone());
            let v = skeleton_value(doc, resolved, seen, depth + 1);
            seen.remove(&name);
            return v;
        }
        return Value::Null;
    }
    // drf-spectacular wraps a single $ref (typically an enum) in allOf, and
    // models nullable-enum / union fields as oneOf/anyOf whose first member
    // is the "real" type (the rest are Blank/Null placeholders) -- take it.
    for union in [&schema.all_of, &schema.one_of, &schema.any_of] {
        if let Some(first) = union.as_ref().and_then(|list| list.first()) {
            return skeleton_value(doc, first, seen, depth + 1);
        }
    }
    if let Some(first) = schema.enum_values.as_ref().and_then(|v| v.first()) {
        return Value::String(first.clone());
    }
    match schema.schema_type.as_deref() {
        Some("string") => Value::String(String::new()),
        Some("integer") | Some("number") => Value::from(0),
        Some("boolean") => Value::Bool(false),
        Some("array") => {
            let elem = schema
                .items
                .as_ref()
                .map(|items| skeleton_value(doc, items, seen, depth + 1));
            Value::Array(elem.into_iter().collect())
        }
        Some("object") | None => match &schema.properties {
            Some(props) => {
                // serde_json::Map is BTreeMap-backed here, so keys land
                // sorted -- deterministic across regenerations regardless of
                // the source HashMap's iteration order.
                let required: std::collections::HashSet<&str> = schema
                    .required
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .collect();
                let mut map = serde_json::Map::new();
                for (name, prop) in props {
                    if prop.read_only {
                        continue;
                    }
                    // Required fields get a real typed placeholder; optional
                    // ones are left `null`. Every optional field in these
                    // request schemas is an `Option<T>` in rs-client, so a raw
                    // skeleton deserializes cleanly (null -> None) and passes
                    // the create/update `--request` type check as-is -- a
                    // typed empty placeholder like "" would instead fail
                    // against strict field types (dates, numbers). The user
                    // fills in whichever optional fields they actually want.
                    let value = if required.contains(name.as_str()) {
                        skeleton_value(doc, prop, seen, depth + 1)
                    } else {
                        Value::Null
                    };
                    map.insert(name.clone(), value);
                }
                Value::Object(map)
            }
            None if schema.schema_type.as_deref() == Some("object") => {
                Value::Object(serde_json::Map::new())
            }
            None => Value::Null,
        },
        _ => Value::Null,
    }
}
