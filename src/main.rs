mod codegen;
mod extract;
mod manifest;

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Usage: waldur-cli-generator [RS_CLIENT_DIR] [WALDUR_CLI_DIR]
///
/// Defaults assume the conventional CI layout where rs-client and waldur-cli
/// are checked out as sibling directories of this repo.
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let rs_client_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../rs-client"));
    let waldur_cli_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../waldur-cli"));

    let manifest_path = Path::new("commands.toml");
    let manifest_text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: manifest::Manifest =
        toml::from_str(&manifest_text).context("parsing commands.toml")?;

    let client_rs_path = rs_client_dir.join("src/generated/client.rs");
    let client_source = fs::read_to_string(&client_rs_path)
        .with_context(|| format!("reading {} (pass the rs-client checkout path as the first argument if it's not at ../rs-client)", client_rs_path.display()))?;

    let wanted: Vec<String> = manifest
        .group
        .iter()
        .flat_map(|g| &g.resource)
        .flat_map(|r| r.commands.values())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let methods = extract::extract_methods(&client_source, &wanted)
        .context("extracting method signatures from rs-client")?;

    let output = codegen::generate_all(&manifest, &methods).context("generating CLI source")?;

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
        "Generated {} resource(s) across {} group(s), {} rs-client method(s) used.",
        output.resources.len(),
        manifest.group.len(),
        methods.len()
    );

    Ok(())
}
