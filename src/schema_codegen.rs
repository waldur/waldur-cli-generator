//! Generates `src/schema.rs` in waldur-cli-target: a single `pub const`
//! containing the full CLI schema as pretty-printed JSON, so the hand-written
//! `schema` subcommand can serve it without any runtime schema access.
//!
//! The JSON shape matches the spec proposed in the agent-friendly enhancements
//! doc: an array of command descriptors with path, description, type,
//! parameters (including filter valid_keys, field valid_values, format
//! valid_values), output column info, and request skeletons/field metadata.

use crate::manifest::{Manifest, KNOWN_VERBS};
use crate::schema::{ExtractedOperation, ParamKind};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Filter kind as a human-readable string for the schema JSON.
fn filter_kind_str(kind: &ParamKind) -> Option<&'static str> {
    match kind {
        ParamKind::RequiredStr | ParamKind::OptionalStr => Some("string"),
        ParamKind::RequiredBool | ParamKind::OptionalBool => Some("boolean"),
        ParamKind::RequiredI64 | ParamKind::OptionalI64 => Some("integer"),
        ParamKind::SkippedOptional | ParamKind::SkippedRequired => None,
    }
}

/// Builds the complete CLI schema JSON from the same data the rest of the
/// codegen pipeline works with.
pub fn build_schema_json(
    manifest: &Manifest,
    operations: &HashMap<String, ExtractedOperation>,
    field_enum_values: &HashMap<String, Vec<String>>,
    request_skeletons: &HashMap<String, String>,
    order_skeletons: &HashMap<String, String>,
    cli_version: &str,
) -> Result<Value> {
    let mut commands = Vec::new();

    for group in &manifest.group {
        for resource in &group.resource {
            // Standard CRUD verbs.
            for verb in KNOWN_VERBS {
                let Some(operation_id) = resource.commands.get(*verb) else {
                    continue;
                };
                let op = operations.get(operation_id).with_context(|| {
                    format!(
                        "schema_codegen: operation `{operation_id}` \
                         (group `{}`, resource `{}`, verb `{verb}`) not extracted",
                        group.name, resource.name
                    )
                })?;

                let path = vec![
                    group.name.clone(),
                    resource.name.clone(),
                    verb.to_string(),
                ];

                let mut params = Vec::new();

                match *verb {
                    "list" => {
                        // --filter
                        let valid_keys: Vec<Value> = op
                            .query_params
                            .iter()
                            .filter_map(|p| {
                                filter_kind_str(&p.kind).map(|t| {
                                    json!({ "key": p.name, "type": t })
                                })
                            })
                            .collect();
                        if !valid_keys.is_empty() {
                            params.push(json!({
                                "name": "--filter",
                                "type": "KEY=VALUE",
                                "repeatable": true,
                                "description": "Server-side filter",
                                "valid_keys": valid_keys
                            }));
                        }

                        // --fields
                        let valid_values = op
                            .field_enum_name
                            .as_ref()
                            .and_then(|n| field_enum_values.get(n));
                        if let Some(values) = valid_values {
                            params.push(json!({
                                "name": "--fields",
                                "type": "string",
                                "repeatable": true,
                                "description": "Fetch only these fields from the API",
                                "valid_values": values
                            }));
                        } else {
                            params.push(json!({
                                "name": "--fields",
                                "type": "string",
                                "repeatable": true,
                                "description": "Fetch only these fields from the API"
                            }));
                        }

                        // --jmespath
                        params.push(json!({
                            "name": "--jmespath",
                            "type": "string",
                            "description": "JMESPath expression for client-side output reshaping"
                        }));

                        // --limit
                        params.push(json!({
                            "name": "--limit",
                            "type": "integer",
                            "description": "Maximum number of items to return"
                        }));
                    }
                    "get" => {
                        if let Some(param_name) = &op.path_param {
                            params.push(json!({
                                "name": param_name,
                                "type": "string",
                                "positional": true,
                                "required": true,
                                "description": format!("{} of the resource", param_name)
                            }));
                        }
                    }
                    "create" | "update" => {
                        if *verb == "update" {
                            if let Some(param_name) = &op.path_param {
                                params.push(json!({
                                    "name": param_name,
                                    "type": "string",
                                    "positional": true,
                                    "required": false,
                                    "description": format!("{} of the resource (required unless --generate-skeleton)", param_name)
                                }));
                            }
                        }
                        params.push(json!({
                            "name": "--request",
                            "type": "json",
                            "description": "Request body as inline JSON"
                        }));
                        params.push(json!({
                            "name": "--request-file",
                            "type": "path",
                            "description": "Read the request body from a JSON or YAML file"
                        }));
                        params.push(json!({
                            "name": "--generate-skeleton",
                            "type": "enum",
                            "description": "Print a fillable request-body template and exit",
                            "valid_values": ["json", "yaml"]
                        }));
                    }
                    "delete" => {
                        if let Some(param_name) = &op.path_param {
                            params.push(json!({
                                "name": param_name,
                                "type": "string",
                                "positional": true,
                                "required": true,
                                "description": format!("{} of the resource", param_name)
                            }));
                        }
                    }
                    _ => {}
                }

                // --format is available on all non-delete verbs, and on
                // delete too (confirmation output is formatted).
                params.push(json!({
                    "name": "--format",
                    "type": "string",
                    "global": true,
                    "description": "Output format",
                    "valid_values": ["table", "json", "tsv", "toon", "ndjson"]
                }));

                let mut cmd_json = json!({
                    "path": path,
                    "description": format!("{} {}", capitalize(verb), resource.about.to_lowercase()),
                    "type": verb,
                    "api_endpoint": op.path,
                    "http_method": op.http_verb.to_uppercase(),
                    "parameters": params
                });

                // Output info for list commands.
                if *verb == "list" && !resource.columns.is_empty() {
                    cmd_json["output"] = json!({
                        "type": "array",
                        "default_columns": resource.columns
                    });
                }

                // Request skeleton for create/update.
                if *verb == "create" || *verb == "update" {
                    if let Some(type_name) = &op.request_body_type {
                        if let Some(skeleton_str) = request_skeletons.get(type_name) {
                            if let Ok(skeleton_val) = serde_json::from_str::<Value>(skeleton_str) {
                                cmd_json["request_skeleton"] = skeleton_val;
                            }
                        }
                    }
                }

                commands.push(cmd_json);
            }

            // Marketplace-order verbs: provision + terminate.
            if resource.order.is_some() {
                // provision
                {
                    let path = vec![
                        group.name.clone(),
                        resource.name.clone(),
                        "provision".to_string(),
                    ];
                    let params = vec![
                        json!({
                            "name": "--request",
                            "type": "json",
                            "description": "The marketplace order body as inline JSON"
                        }),
                        json!({
                            "name": "--request-file",
                            "type": "path",
                            "description": "Read the order body from a JSON or YAML file"
                        }),
                        json!({
                            "name": "--generate-skeleton",
                            "type": "enum",
                            "description": "Print a fillable order template and exit",
                            "valid_values": ["json", "yaml"]
                        }),
                        json!({
                            "name": "--no-wait",
                            "type": "boolean",
                            "description": "Submit the order and return immediately"
                        }),
                        json!({
                            "name": "--timeout",
                            "type": "integer",
                            "description": "Seconds to wait for the order to complete (default: 600)"
                        }),
                        json!({
                            "name": "--format",
                            "type": "string",
                            "global": true,
                            "description": "Output format",
                            "valid_values": ["table", "json", "tsv", "toon", "ndjson"]
                        }),
                    ];

                    let mut cmd_json = json!({
                        "path": path,
                        "description": format!("Provision {} via a marketplace order", resource.about.to_lowercase()),
                        "type": "provision",
                        "parameters": params
                    });

                    // Embed the order skeleton.
                    if let Some(skeleton_str) = order_skeletons.get(&resource.name) {
                        if let Ok(skeleton_val) = serde_json::from_str::<Value>(skeleton_str) {
                            cmd_json["request_skeleton"] = skeleton_val;
                        }
                    }

                    commands.push(cmd_json);
                }

                // terminate
                {
                    let path = vec![
                        group.name.clone(),
                        resource.name.clone(),
                        "terminate".to_string(),
                    ];
                    let params = vec![
                        json!({
                            "name": "uuid",
                            "type": "string",
                            "positional": true,
                            "required": true,
                            "description": "Marketplace resource UUID"
                        }),
                        json!({
                            "name": "--request",
                            "type": "json",
                            "description": "Optional termination attributes as inline JSON"
                        }),
                        json!({
                            "name": "--no-wait",
                            "type": "boolean",
                            "description": "Submit and return immediately"
                        }),
                        json!({
                            "name": "--timeout",
                            "type": "integer",
                            "description": "Seconds to wait for termination (default: 600)"
                        }),
                        json!({
                            "name": "--format",
                            "type": "string",
                            "global": true,
                            "description": "Output format",
                            "valid_values": ["table", "json", "tsv", "toon", "ndjson"]
                        }),
                    ];

                    commands.push(json!({
                        "path": path,
                        "description": format!("Terminate {} via a marketplace order", resource.about.to_lowercase()),
                        "type": "terminate",
                        "parameters": params
                    }));
                }
            }
        }
    }

    // Add hand-written meta-commands that exist in main.rs but aren't
    // generated. These don't need API credentials and are useful for an
    // agent to know about.
    commands.push(json!({
        "path": ["schema"],
        "description": "Print the CLI command schema as JSON",
        "type": "meta",
        "parameters": [
            {
                "name": "--group",
                "type": "string",
                "description": "Only include commands from this group"
            },
            {
                "name": "--compact",
                "type": "boolean",
                "description": "Print only command paths and descriptions (minimal output for context budgets)"
            }
        ]
    }));
    commands.push(json!({
        "path": ["completions"],
        "description": "Generate a shell completion script",
        "type": "meta",
        "parameters": [{
            "name": "shell",
            "type": "string",
            "positional": true,
            "required": true,
            "description": "Shell to generate completions for",
            "valid_values": ["bash", "zsh", "fish", "powershell", "elvish"]
        }]
    }));
    commands.push(json!({
        "path": ["login"],
        "description": "Log in and save API credentials to a local config file",
        "type": "meta",
        "parameters": []
    }));
    commands.push(json!({
        "path": ["logout"],
        "description": "Remove saved credentials for the selected profile",
        "type": "meta",
        "parameters": []
    }));
    commands.push(json!({
        "path": ["whoami"],
        "description": "Show the current user, verifying the active credentials",
        "type": "meta",
        "parameters": []
    }));
    commands.push(json!({
        "path": ["set-project"],
        "description": "Save a default project (UUID) for the selected profile",
        "type": "meta",
        "parameters": [{
            "name": "uuid",
            "type": "string",
            "positional": true,
            "required": true,
            "description": "Project UUID"
        }]
    }));
    commands.push(json!({
        "path": ["unset-project"],
        "description": "Clear the selected profile's saved default project",
        "type": "meta",
        "parameters": []
    }));

    // Build the groups summary for compact mode and general introspection.
    let groups: Vec<Value> = manifest
        .group
        .iter()
        .map(|g| {
            let resources: Vec<Value> = g
                .resource
                .iter()
                .map(|r| {
                    let verbs: Vec<String> = KNOWN_VERBS
                        .iter()
                        .filter(|v| r.commands.contains_key(**v))
                        .map(|v| v.to_string())
                        .chain(
                            r.order
                                .as_ref()
                                .map(|_| vec!["provision".to_string(), "terminate".to_string()])
                                .unwrap_or_default(),
                        )
                        .collect();
                    json!({
                        "name": r.name,
                        "description": r.about,
                        "verbs": verbs,
                        "default_columns": r.columns
                    })
                })
                .collect();
            json!({
                "name": g.name,
                "description": g.about,
                "resources": resources
            })
        })
        .collect();

    Ok(json!({
        "version": cli_version,
        "groups": groups,
        "commands": commands
    }))
}

/// Writes the schema.rs source file into the waldur-cli-target source tree.
pub fn write_schema_rs(
    schema_json: &Value,
    output_dir: &std::path::Path,
) -> Result<()> {
    let pretty = serde_json::to_string_pretty(schema_json)
        .context("serializing CLI schema JSON")?;

    // Use a raw string literal with enough # marks to avoid conflicts with
    // any sequence the JSON might contain.
    let code = format!(
        r####"//! Generated by waldur-cli-generator from `commands.toml`. Do not edit by hand.
//!
//! Contains the full CLI command schema as an embedded JSON string, used by the
//! `schema` subcommand to serve a machine-readable tool specification.

/// The full CLI schema as a JSON string — command paths, descriptions,
/// parameters, valid values, filter keys, request skeletons, and group
/// metadata. Parsed at runtime by the hand-written `schema` subcommand
/// in `main.rs`.
pub const CLI_SCHEMA_JSON: &str = r###"{pretty}"###;
"####,
        pretty = pretty,
    );

    let path = output_dir.join("schema.rs");
    std::fs::write(&path, code)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());

    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Group, OrderConfig, Resource};
    use crate::schema::ExtractedParam;

    fn op(path: &str, verb: &str, path_param: Option<&str>, request_body_type: Option<&str>) -> ExtractedOperation {
        ExtractedOperation {
            operation_id: format!("{verb}_op"),
            path: path.to_string(),
            http_verb: verb.to_string(),
            path_param: path_param.map(str::to_string),
            query_params: vec![ExtractedParam { name: "name".to_string(), kind: ParamKind::OptionalStr }],
            field_enum_name: None,
            request_body_type: request_body_type.map(str::to_string),
        }
    }

    fn resource(name: &str, commands: &[(&str, &str)], order: Option<OrderConfig>) -> Resource {
        Resource {
            name: name.to_string(),
            about: format!("{name}s"),
            columns: vec!["uuid".to_string(), "name".to_string()],
            commands: commands.iter().map(|(v, m)| (v.to_string(), m.to_string())).collect(),
            order,
            actions: None,
            web: None,
        }
    }

    // -- filter_kind_str / capitalize --------------------------------------

    #[test]
    fn filter_kind_str_maps_scalar_kinds_and_drops_skipped() {
        assert_eq!(filter_kind_str(&ParamKind::RequiredStr), Some("string"));
        assert_eq!(filter_kind_str(&ParamKind::OptionalBool), Some("boolean"));
        assert_eq!(filter_kind_str(&ParamKind::RequiredI64), Some("integer"));
        assert_eq!(filter_kind_str(&ParamKind::SkippedOptional), None);
        assert_eq!(filter_kind_str(&ParamKind::SkippedRequired), None);
    }

    #[test]
    fn capitalize_first_letter_only() {
        assert_eq!(capitalize("list"), "List");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("Already"), "Already");
    }

    // -- build_schema_json --------------------------------------------------

    #[test]
    fn build_schema_json_shape() {
        let manifest = Manifest {
            group: vec![Group {
                name: "team".to_string(),
                about: "Team management".to_string(),
                resource: vec![
                    resource("customer", &[("list", "customers_list"), ("get", "customers_get"), ("create", "customers_create")], None),
                    resource("tenant", &[("get", "tenants_get")], Some(OrderConfig { offering_type: None })),
                ],
            }],
        };

        let mut operations = HashMap::new();
        operations.insert("customers_list".to_string(), op("/api/customers/", "get", None, None));
        operations.insert("customers_get".to_string(), op("/api/customers/{uuid}/", "get", Some("uuid"), None));
        operations.insert(
            "customers_create".to_string(),
            op("/api/customers/", "post", None, Some("CustomerRequest")),
        );
        operations.insert("tenants_get".to_string(), op("/api/tenants/{uuid}/", "get", Some("uuid"), None));

        let field_enum_values = HashMap::new();
        let mut request_skeletons = HashMap::new();
        request_skeletons.insert("CustomerRequest".to_string(), r#"{"name": ""}"#.to_string());
        let mut order_skeletons = HashMap::new();
        order_skeletons.insert("tenant".to_string(), r#"{"offering": ""}"#.to_string());

        let schema = build_schema_json(&manifest, &operations, &field_enum_values, &request_skeletons, &order_skeletons, "1.2.3").unwrap();

        assert_eq!(schema["version"], json!("1.2.3"));

        let commands = schema["commands"].as_array().unwrap();
        let find = |path: &[&str]| {
            commands.iter().find(|c| {
                let p: Vec<&str> = c["path"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                p == path
            })
        };

        // list: --filter (from the query param), --fields, --jmespath, --limit, --format.
        let list = find(&["team", "customer", "list"]).expect("list command present");
        let param_names: Vec<&str> = list["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(param_names.contains(&"--filter"));
        assert!(param_names.contains(&"--fields"));
        assert!(param_names.contains(&"--jmespath"));
        assert!(param_names.contains(&"--limit"));
        assert!(param_names.contains(&"--format"));
        assert_eq!(list["output"]["default_columns"], json!(["uuid", "name"]));

        // get: positional uuid, required.
        let get = find(&["team", "customer", "get"]).expect("get command present");
        assert_eq!(get["parameters"][0]["name"], json!("uuid"));
        assert_eq!(get["parameters"][0]["required"], json!(true));

        // create: request skeleton embedded from the schema-name lookup.
        let create = find(&["team", "customer", "create"]).expect("create command present");
        assert_eq!(create["request_skeleton"], json!({"name": ""}));

        // Order-enabled resource gets provision/terminate, embedding the
        // order skeleton keyed by resource name (not operation id).
        let provision = find(&["team", "tenant", "provision"]).expect("provision command present");
        assert_eq!(provision["request_skeleton"], json!({"offering": ""}));
        find(&["team", "tenant", "terminate"]).expect("terminate command present");

        // Hand-written meta-commands are always present.
        for path in [["schema"].as_slice(), &["completions"], &["login"], &["logout"], &["whoami"]] {
            assert!(
                commands.iter().any(|c| c["path"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).eq(path.iter().copied())),
                "missing meta-command {path:?}"
            );
        }

        // Groups summary: the order-enabled resource's verb list includes
        // provision/terminate alongside its declared `get`.
        let groups = schema["groups"].as_array().unwrap();
        let team_group = groups.iter().find(|g| g["name"] == "team").unwrap();
        let tenant_resource = team_group["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "tenant")
            .unwrap();
        let verbs: Vec<&str> = tenant_resource["verbs"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(verbs.contains(&"get"));
        assert!(verbs.contains(&"provision"));
        assert!(verbs.contains(&"terminate"));
    }

    #[test]
    fn build_schema_json_errors_when_an_operation_is_missing() {
        let manifest = Manifest {
            group: vec![Group {
                name: "team".to_string(),
                about: "Team management".to_string(),
                resource: vec![resource("customer", &[("get", "customers_get")], None)],
            }],
        };
        let operations = HashMap::new(); // customers_get was never extracted
        let err = build_schema_json(&manifest, &operations, &HashMap::new(), &HashMap::new(), &HashMap::new(), "0.0.0").unwrap_err();
        assert!(err.to_string().contains("customers_get"));
    }
}
