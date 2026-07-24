mod codegen;
mod manifest;
mod schema;
mod schema_codegen;

use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

/// Usage: waldur-cli-generator [SCHEMA_PATH] [WALDUR_CLI_DIR]
///
/// `SCHEMA_PATH` is the OpenAPI schema (YAML) to generate commands from --
/// the sole source of truth for every operation's path, params, and
/// request/response shape. `WALDUR_CLI_DIR` defaults to the conventional CI
/// layout where waldur-cli is checked out as a sibling of this repo.
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let schema_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("waldur-openapi-schema.yaml"));
    let waldur_cli_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../waldur-cli"));

    let manifest_path = Path::new("commands.toml");
    let manifest_text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: manifest::Manifest =
        toml::from_str(&manifest_text).context("parsing commands.toml")?;

    let doc = schema::load(&schema_path)?;

    let wanted: Vec<String> = manifest
        .group
        .iter()
        .flat_map(|g| &g.resource)
        .flat_map(|r| r.commands.values())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut operations: HashMap<String, schema::ExtractedOperation> = HashMap::new();
    for operation_id in &wanted {
        let op = schema::extract_operation(&doc, operation_id)
            .with_context(|| format!("extracting operation `{operation_id}` from OpenAPI schema"))?;
        operations.insert(operation_id.clone(), op);
    }

    // Auto-discover custom actions (start/stop/restart, attach/detach,
    // approve/reject, ...) for every resource with an `[actions]` config --
    // POST/PUT/... operations at `{resource's own uuid-scoped path}{action}/`,
    // Waldur's convention throughout for state-changing operations that
    // aren't a plain REST create/update/delete. Keyed by resource name
    // (a resource can have several). Discovery needs the resource's own
    // `get` to know its base path, so it's required (same as `wait`).
    let mut resource_actions: HashMap<String, Vec<schema::ExtractedAction>> = HashMap::new();
    for resource in manifest.group.iter().flat_map(|g| &g.resource) {
        let Some(actions_config) = &resource.actions else { continue };
        let get_method_name = resource.commands.get("get").with_context(|| {
            format!(
                "resource `{}` has an [actions] config but no `get` -- action discovery \
                 needs get's own path to know the resource's base path to scan beneath",
                resource.name
            )
        })?;
        let get_op = operations.get(get_method_name).with_context(|| {
            format!("internal error: operation `{get_method_name}` was not extracted")
        })?;
        let actions = schema::discover_actions(&doc, &get_op.path, &actions_config.exclude)
            .with_context(|| format!("discovering actions for resource `{}`", resource.name))?;
        resource_actions.insert(resource.name.clone(), actions);
    }
    // Every discovered action's own ExtractedOperation, alongside the
    // manifest-declared ones -- needed below so field_enum_values/
    // request_skeletons/request_json_schemas cover actions' request bodies
    // too, not just create/update/provision's.
    let action_operations: Vec<&schema::ExtractedOperation> =
        resource_actions.values().flatten().map(|a| &a.operation).collect();

    // Resolve valid --fields values for every list operation that has a
    // `field` query param, so codegen can validate --fields client-side
    // instead of silently sending a bad name the server will just as
    // silently ignore.
    let mut field_enum_values: HashMap<String, Vec<String>> = HashMap::new();
    for op in operations.values().chain(action_operations.iter().copied()) {
        let Some(enum_name) = &op.field_enum_name else { continue };
        if field_enum_values.contains_key(enum_name) {
            continue;
        }
        let values = schema::extract_enum_values(&doc, enum_name)
            .with_context(|| format!("resolving --fields values for operation `{}`", op.operation_id))?;
        field_enum_values.insert(enum_name.clone(), values);
    }

    // Build a fillable request-body template for every operation that has a
    // request body, so codegen can embed it for `--generate-skeleton`. Keyed
    // by the request type name, since the same type is reused across
    // operations (e.g. a resource's create and update).
    let mut request_skeletons: HashMap<String, String> = HashMap::new();
    for op in operations.values().chain(action_operations.iter().copied()) {
        let Some(type_name) = &op.request_body_type else { continue };
        if request_skeletons.contains_key(type_name) {
            continue;
        }
        let skeleton = schema::build_request_skeleton(&doc, type_name)
            .with_context(|| format!("building request skeleton for operation `{}`", op.operation_id))?;
        request_skeletons.insert(type_name.clone(), skeleton);
    }

    // Build a self-contained JSON Schema for every request-body type, so
    // codegen can embed it for waldur-cli to validate `--request` bodies
    // against at runtime -- the exact same schema every other part of the
    // CLI (skeletons, --filter/--fields) is already generated from, so a
    // request-body validation can never drift out of sync with it. Keyed
    // the same way as the skeletons.
    let mut request_json_schemas: HashMap<String, String> = HashMap::new();
    for op in operations.values().chain(action_operations.iter().copied()) {
        let Some(type_name) = &op.request_body_type else { continue };
        if request_json_schemas.contains_key(type_name) {
            continue;
        }
        let json_schema = schema::build_request_json_schema(&doc, type_name)
            .with_context(|| format!("building request JSON schema for operation `{}`", op.operation_id))?;
        request_json_schemas.insert(type_name.clone(), json_schema);
    }

    // Build a `provision` (marketplace order) skeleton for every resource
    // that declares an order config, keyed by resource name (so a generic
    // provisioner with no offering_type gets its own entry too).
    let mut order_skeletons: HashMap<String, String> = HashMap::new();
    for resource in manifest.group.iter().flat_map(|g| &g.resource) {
        let Some(order) = &resource.order else { continue };
        let skeleton = schema::build_order_skeleton(&doc, order.offering_type.as_deref())
            .with_context(|| format!("building order skeleton for resource `{}`", resource.name))?;
        order_skeletons.insert(resource.name.clone(), skeleton);
    }

    let output = codegen::generate_all(
        &manifest,
        &operations,
        &field_enum_values,
        &request_skeletons,
        &request_json_schemas,
        &order_skeletons,
        &resource_actions,
    )
    .context("generating CLI source")?;

    let commands_dir = waldur_cli_dir.join("src/commands");
    fs::create_dir_all(&commands_dir)
        .with_context(|| format!("creating {}", commands_dir.display()))?;
    fs::write(commands_dir.join("mod.rs"), &output.commands_mod_decls)?;

    for (group_name, decls) in &output.group_mod_decls {
        let group_dir = commands_dir.join(group_name.replace('-', "_"));
        fs::create_dir_all(&group_dir).with_context(|| format!("creating {}", group_dir.display()))?;
        fs::write(group_dir.join("mod.rs"), decls)?;
    }

    for resource in &output.resources {
        let group_dir = commands_dir.join(resource.group_name.replace('-', "_"));
        let file_path = group_dir.join(format!("{}.rs", resource.resource_name.replace('-', "_")));
        fs::write(&file_path, &resource.source)
            .with_context(|| format!("writing {}", file_path.display()))?;
        println!("wrote {}", file_path.display());
    }

    let cli_path = waldur_cli_dir.join("src/cli.rs");
    fs::write(&cli_path, &output.cli_source).with_context(|| format!("writing {}", cli_path.display()))?;
    println!("wrote {}", cli_path.display());

    // Generate the CLI schema JSON and emit it as src/schema.rs.
    let cli_version = {
        let cargo_toml_path = waldur_cli_dir.join("Cargo.toml");
        let cargo_text = fs::read_to_string(&cargo_toml_path)
            .with_context(|| format!("reading {} to extract CLI version", cargo_toml_path.display()))?;
        let cargo_toml: toml::Value = cargo_text.parse().context("parsing waldur-cli Cargo.toml")?;
        cargo_toml["package"]["version"]
            .as_str()
            .unwrap_or("0.0.0")
            .to_string()
    };
    let schema_json = schema_codegen::build_schema_json(
        &manifest,
        &operations,
        &field_enum_values,
        &request_skeletons,
        &order_skeletons,
        &cli_version,
    )
    .context("building CLI schema JSON")?;
    schema_codegen::write_schema_rs(&schema_json, &waldur_cli_dir.join("src"))
        .context("writing schema.rs")?;

    println!(
        "Generated {} resource(s) across {} group(s), {} operation(s) used.",
        output.resources.len(),
        manifest.group.len(),
        operations.len()
    );

    Ok(())
}
