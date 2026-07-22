mod codegen;
mod manifest;
mod schema;

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

    // Resolve valid --fields values for every list operation that has a
    // `field` query param, so codegen can validate --fields client-side
    // instead of silently sending a bad name the server will just as
    // silently ignore.
    let mut field_enum_values: HashMap<String, Vec<String>> = HashMap::new();
    for op in operations.values() {
        let Some(enum_name) = &op.field_enum_name else { continue };
        if field_enum_values.contains_key(enum_name) {
            continue;
        }
        let values = schema::extract_enum_values(&doc, enum_name)
            .with_context(|| format!("resolving --fields values for operation `{}`", op.operation_id))?;
        field_enum_values.insert(enum_name.clone(), values);
    }

    let output = codegen::generate_all(&manifest, &operations, &field_enum_values)
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

    println!(
        "Generated {} resource(s) across {} group(s), {} operation(s) used.",
        output.resources.len(),
        manifest.group.len(),
        operations.len()
    );

    Ok(())
}
