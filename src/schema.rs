//! Direct OpenAPI-schema extraction. The schema is the actual source of
//! truth for every operation's path, params, and request/response shape --
//! everything waldur-cli's generated commands need is read from it directly,
//! not proxied through a separately-generated intermediate.

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
    /// JSON Schema's `format` keyword (e.g. `date-time`, `date`, `uuid`,
    /// `email`) -- only consumed by request-body JSON Schema generation
    /// (`build_request_json_schema`), which validates it at runtime. Ignored
    /// everywhere else (skeletons/CLI flags don't need it).
    #[serde(default)]
    pub format: Option<String>,
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
    /// the `field`/`o` filters (handled separately via `field_enum_name`/
    /// `order_enum_name`).
    pub query_params: Vec<ExtractedParam>,
    /// Name of the schema this operation's `field` query param's items
    /// resolve to (e.g. `"CustomerFieldEnum"`), if it has one.
    pub field_enum_name: Option<String>,
    /// Whether this operation has an `o` (ordering) query param at all --
    /// not every list endpoint does (confirmed against the live schema:
    /// most OpenStack resources' lists lack one entirely). Drives whether
    /// `--order` is emitted; `order_enum_name` separately drives whether
    /// its values are validated client-side, since some resources declare
    /// `o` as a bare `string` with no enum (e.g. customers) rather than the
    /// more common `array` of an `{Resource}OEnum`.
    pub has_order: bool,
    /// Name of the schema this operation's `o` query param's items resolve
    /// to (e.g. `"OpenStackFlavorOEnum"` -- each orderable field appears
    /// twice, once bare for ascending and once `-`-prefixed for
    /// descending), if it has one.
    pub order_enum_name: Option<String>,
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
    extract_from_found(doc, path, http_verb, op)
}

/// The actual extraction logic, shared by `extract_operation` (which finds
/// its `(path, verb, op)` by searching for a known operationId) and
/// `discover_actions` (which finds them by scanning paths instead, since a
/// custom action's operationId isn't known ahead of time -- only the path
/// convention it lives at is).
fn extract_from_found(
    doc: &OpenApiDoc,
    path: &str,
    http_verb: &str,
    op: &RawOperation,
) -> Result<ExtractedOperation> {
    let operation_id = op.operation_id.as_str();
    let mut path_param: Option<String> = None;
    let mut query_params = Vec::new();
    let mut field_enum_name: Option<String> = None;
    let mut has_order = false;
    let mut order_enum_name: Option<String> = None;

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
                if param.name == "o" {
                    has_order = true;
                    order_enum_name = param
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
        has_order,
        order_enum_name,
        request_body_type,
    })
}

/// A discovered custom action: its own name (the CLI verb) alongside the
/// fully-extracted operation, same shape as any other verb.
#[derive(Debug, Clone)]
pub struct ExtractedAction {
    /// The action's own name -- the path's last segment (e.g. `"start"`,
    /// `"approve_by_consumer"`), becomes the CLI verb.
    pub name: String,
    pub operation: ExtractedOperation,
}

/// Discovers a resource's custom actions: POST/PUT/... operations at
/// `{base_path}{action}/`, one path segment beyond `base_path` (the
/// resource's own uuid-scoped path, e.g. `/api/openstack-instances/{uuid}/`)
/// -- Waldur's convention throughout for state-changing operations that
/// aren't a plain REST create/update/delete (start/stop/restart, attach/
/// detach, approve/reject, ...). Only scans paths beneath `base_path`, never
/// the whole schema, so this can't accidentally pull in some other
/// resource's actions. A path with its own further path parameter (a nested
/// sub-resource, e.g. `{base_path}networks/{network_uuid}/`) is not an
/// action and is skipped, not just one with more than one segment.
pub fn discover_actions(doc: &OpenApiDoc, base_path: &str, exclude: &[String]) -> Result<Vec<ExtractedAction>> {
    let mut actions = Vec::new();
    for (path, methods) in &doc.paths {
        let Some(rest) = path.strip_prefix(base_path) else { continue };
        if rest.is_empty() || rest.contains('{') {
            continue;
        }
        let name = rest.trim_end_matches('/').to_string();
        if name.is_empty() || name.contains('/') || exclude.contains(&name) {
            continue;
        }
        for (verb, op) in methods {
            let operation = extract_from_found(doc, path, verb, op)
                .with_context(|| format!("discovering action `{name}` at `{path}`"))?;
            actions.push(ExtractedAction { name: name.clone(), operation });
        }
    }
    // Deterministic order across regenerations, independent of the source
    // HashMap's iteration order.
    actions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(actions)
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
/// marketplace `OrderCreateRequest` envelope, with its polymorphic
/// `attributes` object replaced by a concrete schema. For a specific
/// `offering_type` (e.g. `OpenStack.Instance`), that's the typed
/// `{OfferingType}CreateOrderAttributes` schema (Waldur's naming convention,
/// so `OpenStack.Instance` -> `OpenStackInstanceCreateOrderAttributes`). For a
/// generic provisioner (`offering_type` is `None`), it's `GenericOrderAttributes`
/// -- the caller supplies the offering-specific attributes themselves.
/// `accepting_terms_of_service` is defaulted to `true` -- a CLI provision is
/// an explicit action, and leaving it unset can leave the order stuck pending
/// consumer approval.
pub fn build_order_skeleton(doc: &OpenApiDoc, offering_type: Option<&str>) -> Result<String> {
    let mut envelope = skeleton_for(doc, "OrderCreateRequest")?;
    let attrs_schema = match offering_type {
        Some(t) => format!("{}CreateOrderAttributes", t.replace('.', "")),
        None => "GenericOrderAttributes".to_string(),
    };
    let attributes = skeleton_for(doc, &attrs_schema).with_context(|| {
        format!("no attributes schema `{attrs_schema}` for offering type `{offering_type:?}`")
    })?;
    let obj = envelope
        .as_object_mut()
        .context("OrderCreateRequest skeleton is not a JSON object")?;
    obj.insert("attributes".to_string(), attributes);
    obj.insert("accepting_terms_of_service".to_string(), serde_json::Value::Bool(true));
    serde_json::to_string_pretty(&envelope)
        .with_context(|| format!("serializing order skeleton for `{offering_type:?}`"))
}

/// Builds a self-contained JSON Schema (every `$ref` inlined, no external
/// lookups needed at runtime) for `schema_name`, for `waldur-cli` to validate
/// `--request` bodies against directly. Embedded as a `const` in generated
/// code, the same way `build_request_skeleton`'s output is.
pub fn build_request_json_schema(doc: &OpenApiDoc, schema_name: &str) -> Result<String> {
    let schema = doc
        .components
        .schemas
        .get(schema_name)
        .with_context(|| format!("schema `{schema_name}` not found"))?;
    let mut seen = std::collections::HashSet::new();
    seen.insert(schema_name.to_string());
    let value = json_schema_value(doc, schema, &mut seen, 0);
    serde_json::to_string(&value).with_context(|| format!("serializing JSON schema for `{schema_name}`"))
}

/// The JSON-Schema-node counterpart of `skeleton_value`: unlike a skeleton
/// (which only needs *one* example value, so a `oneOf`/`allOf` union can just
/// take its first member), a validation schema must preserve every
/// combinator faithfully -- collapsing `allOf` to one member would silently
/// stop enforcing the others. `$ref`s are still inlined (not left as JSON
/// Schema `$ref`s pointing at a `definitions` block), so the emitted schema
/// is fully self-contained and needs no resolver at runtime.
fn json_schema_value(
    doc: &OpenApiDoc,
    schema: &RawSchema,
    seen: &mut std::collections::HashSet<String>,
    depth: usize,
) -> serde_json::Value {
    use serde_json::{json, Value};
    // A cycle or excessive depth: fall back to JSON Schema's `true` (matches
    // anything), rather than skeleton's `null` -- for validation, "don't
    // know how to check this nested shape, so allow it" is the safe
    // direction; rejecting valid data would be worse than under-validating a
    // shape this deep/self-referential in practice.
    if depth > SKELETON_MAX_DEPTH {
        return Value::Bool(true);
    }
    if let Some(reference) = &schema.reference {
        let name = reference.rsplit('/').next().unwrap_or(reference).to_string();
        if seen.contains(&name) {
            return Value::Bool(true);
        }
        if let Some(resolved) = doc.components.schemas.get(&name) {
            seen.insert(name.clone());
            let v = json_schema_value(doc, resolved, seen, depth + 1);
            seen.remove(&name);
            return v;
        }
        return Value::Bool(true);
    }
    // Real combinators, each member fully resolved -- not skeleton's
    // take-the-first-member shortcut. A single-member `allOf` wrapping a
    // `$ref` (drf-spectacular's way of attaching a sibling `description` to
    // a `$ref`) round-trips as an `allOf` of one dereferenced schema, which
    // is semantically identical to the `$ref` alone.
    for (key, union) in [
        ("allOf", &schema.all_of),
        ("oneOf", &schema.one_of),
        ("anyOf", &schema.any_of),
    ] {
        if let Some(members) = union {
            let resolved: Vec<Value> = members
                .iter()
                .map(|m| json_schema_value(doc, m, seen, depth + 1))
                .collect();
            return json!({ key: resolved });
        }
    }
    if let Some(values) = &schema.enum_values {
        return json!({ "enum": values });
    }
    match schema.schema_type.as_deref() {
        Some("string") => {
            let mut obj = serde_json::Map::new();
            obj.insert("type".to_string(), json!("string"));
            if let Some(format) = &schema.format {
                obj.insert("format".to_string(), json!(format));
            }
            Value::Object(obj)
        }
        Some("integer") => json!({ "type": "integer" }),
        Some("number") => json!({ "type": "number" }),
        Some("boolean") => json!({ "type": "boolean" }),
        Some("array") => {
            let items = schema
                .items
                .as_ref()
                .map(|items| json_schema_value(doc, items, seen, depth + 1))
                .unwrap_or(Value::Bool(true));
            json!({ "type": "array", "items": items })
        }
        Some("object") | None => match &schema.properties {
            Some(props) => {
                // Read-only fields are dropped entirely (never part of a
                // *written* request body -- same as skeleton_value), which
                // means `required` has to be filtered to match, or a
                // dropped-but-still-required field would make the schema
                // reject every request.
                let mut properties = serde_json::Map::new();
                for (name, prop) in props {
                    if prop.read_only {
                        continue;
                    }
                    properties.insert(name.clone(), json_schema_value(doc, prop, seen, depth + 1));
                }
                let required: Vec<&str> = schema
                    .required
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(String::as_str)
                    .filter(|name| properties.contains_key(*name))
                    .collect();
                json!({ "type": "object", "properties": properties, "required": required })
            }
            None if schema.schema_type.as_deref() == Some("object") => {
                json!({ "type": "object" })
            }
            None => Value::Bool(true),
        },
        _ => Value::Bool(true),
    }
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
                    // ones are left `null`. A raw skeleton still passes
                    // request-body validation as-is: waldur-cli's load_body
                    // strips null-valued keys before validating, so an
                    // untouched optional field reads as "absent," which the
                    // JSON Schema's `required` list never complains about --
                    // a typed empty placeholder like "" would instead fail
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

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> OpenApiDoc {
        serde_yaml::from_str(yaml).expect("test fixture YAML should parse")
    }

    fn param(schema_type: &str, required: bool) -> RawParameter {
        RawParameter {
            name: "x".to_string(),
            location: "query".to_string(),
            required,
            schema: RawSchema { schema_type: Some(schema_type.to_string()), ..Default::default() },
        }
    }

    // -- classify_param -------------------------------------------------

    #[test]
    fn classify_param_maps_scalar_types() {
        assert!(matches!(classify_param(&param("string", true)), ParamKind::RequiredStr));
        assert!(matches!(classify_param(&param("string", false)), ParamKind::OptionalStr));
        assert!(matches!(classify_param(&param("boolean", true)), ParamKind::RequiredBool));
        assert!(matches!(classify_param(&param("boolean", false)), ParamKind::OptionalBool));
        assert!(matches!(classify_param(&param("integer", true)), ParamKind::RequiredI64));
        assert!(matches!(classify_param(&param("integer", false)), ParamKind::OptionalI64));
    }

    #[test]
    fn classify_param_treats_array_as_str_since_filter_repeats_the_key() {
        assert!(matches!(classify_param(&param("array", true)), ParamKind::RequiredStr));
        assert!(matches!(classify_param(&param("array", false)), ParamKind::OptionalStr));
    }

    #[test]
    fn classify_param_skips_unrecognized_types() {
        assert!(matches!(classify_param(&param("object", true)), ParamKind::SkippedRequired));
        assert!(matches!(classify_param(&param("object", false)), ParamKind::SkippedOptional));
    }

    // -- resolve_param ----------------------------------------------------

    #[test]
    fn resolve_param_finds_a_shared_component_parameter() {
        let d = doc(
            r#"
paths: {}
components:
  parameters:
    Page:
      name: page
      in: query
      schema:
        type: integer
  schemas: {}
"#,
        );
        let resolved = resolve_param(&d, "#/components/parameters/Page").unwrap();
        assert_eq!(resolved.name, "page");
    }

    #[test]
    fn resolve_param_rejects_unsupported_ref_shape() {
        let d = doc("paths: {}\ncomponents: {}\n");
        let err = resolve_param(&d, "#/definitions/Page").unwrap_err();
        assert!(err.to_string().contains("unsupported parameter $ref shape"));
    }

    #[test]
    fn resolve_param_errors_on_unknown_name() {
        let d = doc("paths: {}\ncomponents: {}\n");
        let err = resolve_param(&d, "#/components/parameters/Nope").unwrap_err();
        assert!(err.to_string().contains("does not resolve to a known parameter"));
    }

    // -- extract_operation --------------------------------------------------

    #[test]
    fn extract_operation_full_shape() {
        let d = doc(
            r#"
paths:
  /api/things/{uuid}/:
    get:
      operationId: things_retrieve
      parameters:
        - name: uuid
          in: path
          required: true
          schema: {type: string}
        - $ref: '#/components/parameters/Page'
        - $ref: '#/components/parameters/PageSize'
        - name: name
          in: query
          required: false
          schema: {type: string}
        - name: field
          in: query
          required: false
          schema:
            type: array
            items:
              $ref: '#/components/schemas/ThingFieldEnum'
      requestBody:
        content:
          application/json:
            schema:
              $ref: '#/components/schemas/ThingRequest'
components:
  parameters:
    Page:
      name: page
      in: query
      schema: {type: integer}
    PageSize:
      name: page_size
      in: query
      schema: {type: integer}
  schemas:
    ThingFieldEnum:
      type: string
      enum: [uuid, name]
    ThingRequest:
      type: object
      properties:
        name: {type: string}
"#,
        );
        let op = extract_operation(&d, "things_retrieve").unwrap();
        assert_eq!(op.path, "/api/things/{uuid}/");
        assert_eq!(op.http_verb, "get");
        assert_eq!(op.path_param.as_deref(), Some("uuid"));
        // page/page_size (resolved via $ref) are filtered out; field is
        // pulled into field_enum_name separately -- only "name" remains.
        assert_eq!(op.query_params.len(), 1);
        assert_eq!(op.query_params[0].name, "name");
        assert_eq!(op.field_enum_name.as_deref(), Some("ThingFieldEnum"));
        assert_eq!(op.request_body_type.as_deref(), Some("ThingRequest"));
    }

    #[test]
    fn extract_operation_bails_on_multiple_path_params() {
        let d = doc(
            r#"
paths:
  /api/things/{a}/{b}/:
    get:
      operationId: multi_path
      parameters:
        - {name: a, in: path, required: true, schema: {type: string}}
        - {name: b, in: path, required: true, schema: {type: string}}
components: {}
"#,
        );
        let err = extract_operation(&d, "multi_path").unwrap_err();
        assert!(err.to_string().contains("more than one"));
    }

    #[test]
    fn extract_operation_not_found_is_a_clear_error() {
        let d = doc("paths: {}\ncomponents: {}\n");
        let err = extract_operation(&d, "nope").unwrap_err();
        assert!(err.to_string().contains("not found in OpenAPI schema"));
    }

    #[test]
    fn extract_operation_without_request_body_or_path_param_has_none() {
        let d = doc(
            r#"
paths:
  /api/things/:
    get:
      operationId: things_list
components: {}
"#,
        );
        let op = extract_operation(&d, "things_list").unwrap();
        assert_eq!(op.request_body_type, None);
        assert_eq!(op.path_param, None);
        assert!(op.query_params.is_empty());
    }

    // -- discover_actions -----------------------------------------------------

    fn actions_doc() -> OpenApiDoc {
        doc(
            r#"
paths:
  /api/things/{uuid}/:
    get:
      operationId: things_retrieve
  /api/things/{uuid}/start/:
    post:
      operationId: things_start
  /api/things/{uuid}/pull/:
    post:
      operationId: things_pull
  /api/things/{uuid}/history/at/:
    get:
      operationId: things_history_at
  /api/other/{uuid}/detach/:
    post:
      operationId: other_detach
components: {}
"#,
        )
    }

    #[test]
    fn discover_actions_finds_direct_children_of_the_base_path() {
        let d = actions_doc();
        let actions = discover_actions(&d, "/api/things/{uuid}/", &[]).unwrap();
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["pull", "start"]);
    }

    #[test]
    fn discover_actions_skips_deeper_nested_paths() {
        let d = actions_doc();
        let actions = discover_actions(&d, "/api/things/{uuid}/", &[]).unwrap();
        assert!(actions.iter().all(|a| a.name != "history/at"));
    }

    #[test]
    fn discover_actions_ignores_paths_outside_the_base() {
        let d = actions_doc();
        let actions = discover_actions(&d, "/api/things/{uuid}/", &[]).unwrap();
        assert!(actions.iter().all(|a| a.name != "detach"));
    }

    #[test]
    fn discover_actions_respects_the_exclude_list() {
        let d = actions_doc();
        let actions =
            discover_actions(&d, "/api/things/{uuid}/", &["pull".to_string()]).unwrap();
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["start"]);
    }

    #[test]
    fn discover_actions_sorts_by_name_deterministically() {
        let d = actions_doc();
        let actions = discover_actions(&d, "/api/things/{uuid}/", &[]).unwrap();
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    // -- extract_enum_values ------------------------------------------------

    #[test]
    fn extract_enum_values_returns_the_list() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Color:
      type: string
      enum: [red, green, blue]
"#,
        );
        assert_eq!(extract_enum_values(&d, "Color").unwrap(), vec!["red", "green", "blue"]);
    }

    #[test]
    fn extract_enum_values_errors_when_schema_has_no_enum() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Plain:
      type: string
"#,
        );
        let err = extract_enum_values(&d, "Plain").unwrap_err();
        assert!(err.to_string().contains("has no `enum` values"));
    }

    #[test]
    fn extract_enum_values_errors_when_schema_not_found() {
        let d = doc("paths: {}\ncomponents: {}\n");
        let err = extract_enum_values(&d, "Nope").unwrap_err();
        assert!(err.to_string().contains("not found in OpenAPI schema"));
    }

    // -- skeleton_value / build_request_skeleton -----------------------------

    #[test]
    fn skeleton_required_field_gets_typed_placeholder_optional_gets_null() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [name]
      properties:
        name: {type: string}
        age: {type: integer}
        active: {type: boolean}
"#,
        );
        let json = build_request_skeleton(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], serde_json::json!(""));
        assert_eq!(value["age"], serde_json::Value::Null);
        assert_eq!(value["active"], serde_json::Value::Null);
    }

    #[test]
    fn skeleton_array_field_has_one_sample_element() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [tags]
      properties:
        tags:
          type: array
          items: {type: string}
"#,
        );
        let json = build_request_skeleton(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["tags"], serde_json::json!([""]));
    }

    #[test]
    fn skeleton_enum_field_uses_first_value() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [color]
      properties:
        color:
          type: string
          enum: [red, green]
"#,
        );
        let json = build_request_skeleton(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["color"], serde_json::json!("red"));
    }

    #[test]
    fn skeleton_ref_field_resolves_and_recurses() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [owner]
      properties:
        owner:
          $ref: '#/components/schemas/Owner'
    Owner:
      type: object
      required: [name]
      properties:
        name: {type: string}
"#,
        );
        let json = build_request_skeleton(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["owner"]["name"], serde_json::json!(""));
    }

    #[test]
    fn skeleton_allof_wrapping_a_ref_takes_that_member() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [status]
      properties:
        status:
          allOf:
            - $ref: '#/components/schemas/Status'
          description: "wrapped for a sibling description, drf-spectacular style"
    Status:
      type: string
      enum: [active, inactive]
"#,
        );
        let json = build_request_skeleton(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["status"], serde_json::json!("active"));
    }

    #[test]
    fn skeleton_read_only_field_is_excluded_entirely() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [name]
      properties:
        name: {type: string}
        uuid: {type: string, readOnly: true}
"#,
        );
        let json = build_request_skeleton(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("uuid").is_none());
    }

    #[test]
    fn skeleton_self_referential_ref_stops_at_the_cycle() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Node:
      type: object
      required: [parent]
      properties:
        parent:
          $ref: '#/components/schemas/Node'
"#,
        );
        // The real assertion is that this returns at all (proving the `seen`
        // cycle guard works) rather than recursing until the depth cutoff on
        // every test run.
        let json = build_request_skeleton(&d, "Node").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["parent"], serde_json::Value::Null);
    }

    #[test]
    fn skeleton_depth_cutoff_returns_null() {
        let d = doc("paths: {}\ncomponents: {}\n");
        let schema = RawSchema { schema_type: Some("string".to_string()), ..Default::default() };
        let mut seen = std::collections::HashSet::new();
        let value = skeleton_value(&d, &schema, &mut seen, SKELETON_MAX_DEPTH + 1);
        assert_eq!(value, serde_json::Value::Null);
    }

    // -- build_order_skeleton ------------------------------------------------

    #[test]
    fn order_skeleton_typed_offering_uses_the_naming_convention() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    OrderCreateRequest:
      type: object
      properties:
        offering: {type: string}
    OpenStackInstanceCreateOrderAttributes:
      type: object
      required: [name]
      properties:
        name: {type: string}
"#,
        );
        let json = build_order_skeleton(&d, Some("OpenStack.Instance")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["attributes"]["name"], serde_json::json!(""));
        assert_eq!(value["accepting_terms_of_service"], serde_json::json!(true));
    }

    #[test]
    fn order_skeleton_generic_uses_generic_attributes() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    OrderCreateRequest:
      type: object
      properties: {}
    GenericOrderAttributes:
      type: object
      required: [name]
      properties:
        name: {type: string}
"#,
        );
        let json = build_order_skeleton(&d, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["attributes"]["name"], serde_json::json!(""));
    }

    #[test]
    fn order_skeleton_missing_attrs_schema_is_a_clear_error() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    OrderCreateRequest:
      type: object
      properties: {}
"#,
        );
        let err = build_order_skeleton(&d, Some("Nonexistent.Type")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("attributes schema"));
        assert!(msg.contains("NonexistentType"));
    }

    // -- json_schema_value / build_request_json_schema -----------------------

    #[test]
    fn json_schema_required_field_appears_in_properties_and_required() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [name]
      properties:
        name: {type: string}
        age: {type: integer}
"#,
        );
        let json = build_request_json_schema(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["properties"]["name"], serde_json::json!({"type": "string"}));
        assert_eq!(value["properties"]["age"], serde_json::json!({"type": "integer"}));
        assert_eq!(value["required"], serde_json::json!(["name"]));
    }

    #[test]
    fn json_schema_format_is_preserved() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      properties:
        created:
          type: string
          format: date-time
"#,
        );
        let json = build_request_json_schema(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["properties"]["created"],
            serde_json::json!({"type": "string", "format": "date-time"})
        );
    }

    #[test]
    fn json_schema_read_only_field_excluded_from_properties_and_required() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      required: [uuid, name]
      properties:
        uuid: {type: string, readOnly: true}
        name: {type: string}
"#,
        );
        let json = build_request_json_schema(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["properties"].get("uuid").is_none());
        // uuid was in the schema's own `required` list -- dropping the
        // read-only property without also filtering `required` would leave
        // the schema demanding a field it no longer describes at all,
        // rejecting every request.
        assert_eq!(value["required"], serde_json::json!(["name"]));
    }

    #[test]
    fn json_schema_preserves_every_allof_member_not_just_the_first() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      allOf:
        - $ref: '#/components/schemas/Base'
        - $ref: '#/components/schemas/Extra'
    Base:
      type: object
      properties:
        name: {type: string}
    Extra:
      type: object
      properties:
        age: {type: integer}
"#,
        );
        let json = build_request_json_schema(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let members = value["allOf"].as_array().unwrap();
        assert_eq!(members.len(), 2, "skeleton_value would take only the first member -- json schema must keep both");
        assert_eq!(members[0]["properties"]["name"], serde_json::json!({"type": "string"}));
        assert_eq!(members[1]["properties"]["age"], serde_json::json!({"type": "integer"}));
    }

    #[test]
    fn json_schema_enum_field() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      properties:
        color:
          type: string
          enum: [red, green]
"#,
        );
        let json = build_request_json_schema(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["properties"]["color"], serde_json::json!({"enum": ["red", "green"]}));
    }

    #[test]
    fn json_schema_self_referential_ref_becomes_permissive_true() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Node:
      type: object
      properties:
        parent:
          $ref: '#/components/schemas/Node'
"#,
        );
        let json = build_request_json_schema(&d, "Node").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Unlike skeleton_value's Null, a cycle here must stay a *valid*
        // schema node -- JSON Schema's `true` means "anything passes," so
        // validation doesn't reject legitimately recursive data it can't
        // fully describe.
        assert_eq!(value["properties"]["parent"], serde_json::json!(true));
    }

    #[test]
    fn json_schema_depth_cutoff_is_permissive_true_not_null() {
        let d = doc("paths: {}\ncomponents: {}\n");
        let schema = RawSchema { schema_type: Some("string".to_string()), ..Default::default() };
        let mut seen = std::collections::HashSet::new();
        let value = json_schema_value(&d, &schema, &mut seen, SKELETON_MAX_DEPTH + 1);
        assert_eq!(value, serde_json::Value::Bool(true));
    }

    #[test]
    fn json_schema_array_field() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      properties:
        tags:
          type: array
          items: {type: string}
"#,
        );
        let json = build_request_json_schema(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["properties"]["tags"],
            serde_json::json!({"type": "array", "items": {"type": "string"}})
        );
    }

    #[test]
    fn json_schema_object_without_properties_is_a_permissive_object() {
        let d = doc(
            r#"
paths: {}
components:
  schemas:
    Thing:
      type: object
      properties:
        metadata:
          type: object
"#,
        );
        let json = build_request_json_schema(&d, "Thing").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["properties"]["metadata"], serde_json::json!({"type": "object"}));
    }
}
