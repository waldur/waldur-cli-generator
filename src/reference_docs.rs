//! Generates `docs/reference/` in waldur-cli-target: one markdown page per
//! full command (`waldur-cli <group> <resource> <verb>`), plus a README
//! index per group and at the top level. Derived from the same schema data
//! `codegen.rs` builds the actual command surface from, so it can't drift
//! out of sync with it the way hand-written reference docs inevitably would
//! every time a resource or verb changes -- this is regenerated wholesale
//! on every run, the same as `src/commands/`.

use crate::manifest::{Manifest, Resource, KNOWN_VERBS};
use crate::schema::{ExtractedAction, ExtractedOperation, ParamKind};
use anyhow::{Context, Result};
use std::collections::HashMap;

/// One generated page, path relative to `docs/reference/`.
pub struct ReferencePage {
    pub relative_path: String,
    pub content: String,
}

/// Shared by every page: the global flags common to (almost) every command,
/// kept as one short pointer rather than repeating the same ~10-flag
/// description block on every one of ~90 pages.
const GLOBAL_OPTIONS: &str = "\n## Global options\n\n\
Every command also accepts `--api-url`, `--token`, `--profile`, `--format`, and `--debug`; \
mutating commands additionally accept `--dry-run`. See \
[Getting started](../../1-getting-started.md) for what each does.\n";

fn filter_kind_str(kind: &ParamKind) -> Option<&'static str> {
    match kind {
        ParamKind::RequiredStr | ParamKind::OptionalStr => Some("string"),
        ParamKind::RequiredBool | ParamKind::OptionalBool => Some("boolean"),
        ParamKind::RequiredI64 | ParamKind::OptionalI64 => Some("integer"),
        ParamKind::SkippedOptional | ParamKind::SkippedRequired => None,
    }
}

/// The CLI verb as typed: clap kebab-cases derive(Subcommand) variant names,
/// so a discovered action like `change_flavor` is actually invoked as
/// `change-flavor`.
fn cli_verb(name: &str) -> String {
    name.replace('_', "-")
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Renders a `| Flag | Type | Description |` table. Empty input renders
/// nothing (some verbs, e.g. bodyless actions, only have positional args).
fn options_table(rows: &[(String, String, String)]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n| Flag | Type | Description |\n| --- | --- | --- |\n");
    for (flag, ty, desc) in rows {
        s.push_str(&format!("| `{flag}` | {ty} | {desc} |\n"));
    }
    s
}

/// Picks a filter key likely to make a believable example (a real value is
/// easy to guess for `state=OK`/`name_exact=...`, much less so for an
/// arbitrary numeric range key) -- falls back to the first filter key at
/// all, so every resource with any filter still gets a concrete example.
fn pick_example_filter(op: &ExtractedOperation) -> Option<(&str, &'static str)> {
    let candidates: &[(&str, &str)] =
        &[("state", "OK"), ("name_exact", "example"), ("is_active", "true")];
    for (key, value) in candidates {
        if op.query_params.iter().any(|p| p.name == *key) {
            return Some((key, value));
        }
    }
    op.query_params.first().and_then(|p| match filter_kind_str(&p.kind) {
        Some("boolean") => Some((p.name.as_str(), "true")),
        Some("integer") => Some((p.name.as_str(), "1")),
        Some(_) => Some((p.name.as_str(), "example")),
        None => None,
    })
}

fn page_path(group: &str, resource: &str, verb: &str) -> String {
    format!("{group}/{resource}-{verb}.md")
}

fn page(group: &str, resource: &str, verb: &str, body: String) -> ReferencePage {
    ReferencePage { relative_path: page_path(group, resource, &cli_verb(verb)), content: body }
}

#[allow(clippy::too_many_arguments)]
fn list_page(
    group: &str,
    resource: &Resource,
    op: &ExtractedOperation,
    field_enum_values: &HashMap<String, Vec<String>>,
    order_enum_values: &HashMap<String, Vec<String>>,
) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} list", resource.name);
    let mut md = format!(
        "# `{cmd}`\n\nList {}.\n\n## Usage\n\n```bash\n{cmd} [OPTIONS]\n```\n",
        resource.about.to_lowercase()
    );

    let mut rows = Vec::new();
    let filter_keys: Vec<String> = op
        .query_params
        .iter()
        .filter_map(|p| filter_kind_str(&p.kind).map(|t| format!("`{}` ({t})", p.name)))
        .collect();
    if !filter_keys.is_empty() {
        rows.push((
            "--filter KEY=VALUE".to_string(),
            "repeatable".to_string(),
            format!("Server-side filter. Valid keys: {}.", filter_keys.join(", ")),
        ));
    }
    let fields_desc = match op.field_enum_name.as_ref().and_then(|n| field_enum_values.get(n)) {
        Some(values) => format!(
            "Fetch only these fields from the server (comma-separated). Valid: {}.",
            values.join(", ")
        ),
        None => "Fetch only these fields from the server (comma-separated).".to_string(),
    };
    rows.push(("--fields FIELDS".into(), "string".into(), fields_desc));
    if op.has_order {
        let order_desc = match op.order_enum_name.as_ref().and_then(|n| order_enum_values.get(n)) {
            Some(values) => format!(
                "Sort server-side (comma-separated, `-` prefix for descending). Valid: {}.",
                values.join(", ")
            ),
            None => "Sort server-side (comma-separated, `-` prefix for descending).".to_string(),
        };
        rows.push(("--order FIELDS".into(), "string".into(), order_desc));
    }
    rows.push((
        "--jmespath EXPR".into(),
        "string".into(),
        "Reshape the already-fetched result client-side (https://jmespath.org).".into(),
    ));
    rows.push(("--limit N".into(), "integer".into(), "Stop after this many items.".into()));
    rows.push(("--format FORMAT".into(), "string".into(), "table, json, tsv, toon, or ndjson.".into()));
    md += &options_table(&rows);

    md += "\n## Examples\n\n";
    let (col1, col2) = (
        resource.columns.first().map(String::as_str).unwrap_or("uuid"),
        resource.columns.get(1).map(String::as_str).unwrap_or("name"),
    );
    match pick_example_filter(op) {
        Some((key, value)) => {
            md += &format!(
                "```bash\n{cmd} --filter {key}={value} --fields {col1},{col2} --format json\n```\n"
            );
        }
        None => {
            md += &format!("```bash\n{cmd} --fields {col1},{col2} --format json\n```\n");
        }
    }
    md += &format!(
        "\nProject just the columns you need, client-side:\n\n```bash\n{cmd} --jmespath '[].[{col1}, {col2}]'\n```\n"
    );
    if op.has_order {
        // Use an actual valid ascending value when one's resolvable (a
        // column name is not necessarily an orderable field -- e.g.
        // flavor's own `uuid` column isn't one of its valid --order
        // values), falling back to col1 only when there's no enum to
        // check against at all.
        let order_field = op
            .order_enum_name
            .as_ref()
            .and_then(|n| order_enum_values.get(n))
            .and_then(|values| values.iter().find(|v| !v.starts_with('-')))
            .map(String::as_str)
            .unwrap_or(col1);
        md += &format!(
            "\nSmallest/first result matching a filter, sorted server-side:\n\n```bash\n{cmd} --order {order_field} --limit 1\n```\n"
        );
    }
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "list", md)
}

fn get_page(group: &str, resource: &Resource, op: &ExtractedOperation) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} get", resource.name);
    let mut md = format!(
        "# `{cmd}`\n\nGet {}.\n\n## Usage\n\n```bash\n{cmd} <UUID> [OPTIONS]\n```\n",
        resource.about.to_lowercase()
    );

    let mut rows = vec![(
        "<UUID>".to_string(),
        "positional, required".to_string(),
        format!("{} of the resource.", op.path_param.as_deref().unwrap_or("uuid")),
    )];
    if let Some(web) = &resource.web {
        let _ = web;
        rows.push((
            "--web".into(),
            "flag".into(),
            "Open this resource's page in Waldur's web UI (HomePort) instead of printing it."
                .into(),
        ));
    }
    rows.push(("--format FORMAT".into(), "string".into(), "table, json, tsv, toon, or ndjson.".into()));
    md += &options_table(&rows);

    md += &format!("\n## Examples\n\n```bash\n{cmd} <uuid> --format json\n```\n");
    if resource.web.is_some() {
        md += &format!("\nOpen it in the browser instead:\n\n```bash\n{cmd} <uuid> --web\n```\n");
    }
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "get", md)
}

/// Renders a compact-JSON one-liner from a pretty-printed skeleton, for the
/// `--request` example -- falls back to the raw (pretty) skeleton text if it
/// somehow doesn't parse as JSON, so a page is never left without an
/// example over a formatting edge case.
fn compact_json(skeleton: &str) -> String {
    serde_json::from_str::<serde_json::Value>(skeleton)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_else(|| skeleton.to_string())
}

fn body_verb_examples(cmd: &str, skeleton: Option<&str>) -> String {
    let mut md = String::from("\n## Examples\n\nSee the fillable template first:\n\n```bash\n");
    md += &format!("{cmd} --generate-skeleton\n```\n");
    if let Some(skeleton) = skeleton {
        md += &format!(
            "\nThen fill it in and submit:\n\n```bash\n{cmd} --request '{}'\n```\n",
            compact_json(skeleton)
        );
    }
    md
}

fn create_page(
    group: &str,
    resource: &Resource,
    op: &ExtractedOperation,
    request_skeletons: &HashMap<String, String>,
) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} create", resource.name);
    let mut md = format!(
        "# `{cmd}`\n\nCreate {}.\n\n## Usage\n\n```bash\n{cmd} (--request JSON | --request-file PATH | --generate-skeleton)\n```\n",
        resource.about.to_lowercase()
    );
    md += &options_table(&[
        ("--request JSON".into(), "string".into(), "Request body as inline JSON.".into()),
        (
            "--request-file PATH".into(),
            "path".into(),
            "Read the request body from a JSON or YAML file.".into(),
        ),
        (
            "--generate-skeleton [FORMAT]".into(),
            "json|yaml".into(),
            "Print a fillable template and exit, instead of sending a request.".into(),
        ),
    ]);
    let skeleton = op.request_body_type.as_ref().and_then(|t| request_skeletons.get(t)).map(String::as_str);
    md += &body_verb_examples(&cmd, skeleton);
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "create", md)
}

fn update_page(
    group: &str,
    resource: &Resource,
    op: &ExtractedOperation,
    request_skeletons: &HashMap<String, String>,
) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} update", resource.name);
    let mut md = format!(
        "# `{cmd}`\n\nUpdate {}.\n\n## Usage\n\n```bash\n{cmd} <UUID> (--request JSON | --request-file PATH | --generate-skeleton)\n```\n",
        resource.about.to_lowercase()
    );
    md += &options_table(&[
        (
            "<UUID>".into(),
            "positional".into(),
            "Required unless --generate-skeleton (the template doesn't need a specific resource).".into(),
        ),
        ("--request JSON".into(), "string".into(), "Request body as inline JSON.".into()),
        (
            "--request-file PATH".into(),
            "path".into(),
            "Read the request body from a JSON or YAML file.".into(),
        ),
        (
            "--generate-skeleton [FORMAT]".into(),
            "json|yaml".into(),
            "Print a fillable template and exit, instead of sending a request.".into(),
        ),
    ]);
    let skeleton = op.request_body_type.as_ref().and_then(|t| request_skeletons.get(t)).map(String::as_str);
    let mut examples = String::from("\n## Examples\n\nSee the fillable template first:\n\n```bash\n");
    examples += &format!("{cmd} --generate-skeleton\n```\n");
    if let Some(skeleton) = skeleton {
        examples += &format!(
            "\nThen fill it in and submit (only the fields you're changing need a value -- \
             `null` fields in the template are omitted, not sent literally):\n\n```bash\n{cmd} <uuid> --request '{}'\n```\n",
            compact_json(skeleton)
        );
    }
    md += &examples;
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "update", md)
}

fn delete_page(group: &str, resource: &Resource, op: &ExtractedOperation) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} delete", resource.name);
    let mut md = format!(
        "# `{cmd}`\n\nDelete {}.\n\n## Usage\n\n```bash\n{cmd} <UUID>\n```\n",
        resource.about.to_lowercase()
    );
    md += &options_table(&[(
        "<UUID>".to_string(),
        "positional, required".to_string(),
        format!("{} of the resource.", op.path_param.as_deref().unwrap_or("uuid")),
    )]);
    md += &format!("\n## Examples\n\n```bash\n{cmd} <uuid>\n```\n\nPreview without deleting:\n\n```bash\n{cmd} <uuid> --dry-run\n```\n");
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "delete", md)
}

fn provision_page(
    group: &str,
    resource: &Resource,
    order_skeletons: &HashMap<String, String>,
) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} provision", resource.name);
    let mut md = format!(
        "# `{cmd}`\n\nProvision {} via a marketplace order.\n\n## Usage\n\n```bash\n{cmd} (--request JSON | --request-file PATH | --generate-skeleton)\n```\n",
        resource.about.to_lowercase()
    );
    md += &options_table(&[
        ("--request JSON".into(), "string".into(), "The order body as inline JSON.".into()),
        ("--request-file PATH".into(), "path".into(), "Read the order body from a JSON or YAML file.".into()),
        (
            "--generate-skeleton [FORMAT]".into(),
            "json|yaml".into(),
            "Print a fillable order template and exit.".into(),
        ),
        (
            "--no-wait".into(),
            "flag".into(),
            "Submit and return immediately, without polling the order to completion.".into(),
        ),
        (
            "--timeout N".into(),
            "integer".into(),
            "Seconds to wait for the order to complete before giving up (default 600).".into(),
        ),
        ("--interval N".into(), "integer".into(), "Seconds between polls (default 3).".into()),
    ]);
    let skeleton = order_skeletons.get(&resource.name).map(String::as_str);
    md += &body_verb_examples(&cmd, skeleton);
    md += &format!(
        "\nFire-and-forget, checking on it later from the printed order UUID:\n\n```bash\n{cmd} --request-file order.yaml --no-wait --format json\n```\n"
    );
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "provision", md)
}

fn terminate_page(group: &str, resource: &Resource) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} terminate", resource.name);
    let mut md = format!(
        "# `{cmd}`\n\nTerminate {} via a marketplace order.\n\n## Usage\n\n```bash\n{cmd} <MARKETPLACE_RESOURCE_UUID> [OPTIONS]\n```\n",
        resource.about.to_lowercase()
    );
    md += &options_table(&[
        (
            "<MARKETPLACE_RESOURCE_UUID>".into(),
            "positional, required".into(),
            "The resource's `marketplace_resource_uuid` field (from get/list) -- not its own uuid.".into(),
        ),
        ("--request JSON".into(), "string".into(), "Optional termination attributes as inline JSON.".into()),
        (
            "--no-wait".into(),
            "flag".into(),
            "Submit and return immediately, without polling the order to completion.".into(),
        ),
        (
            "--timeout N".into(),
            "integer".into(),
            "Seconds to wait for termination before giving up (default 600).".into(),
        ),
        ("--interval N".into(), "integer".into(), "Seconds between polls (default 3).".into()),
    ]);
    md += &format!(
        "\n## Examples\n\n```bash\n{cmd} <marketplace-resource-uuid>\n```\n\nWith termination attributes:\n\n```bash\n{cmd} <marketplace-resource-uuid> --request '{{\"delete_volumes\": true}}'\n```\n"
    );
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "terminate", md)
}

fn wait_page(group: &str, resource: &Resource) -> ReferencePage {
    let cmd = format!("waldur-cli {group} {} wait", resource.name);
    // "state" makes by far the most plausible wait-condition example when
    // it's a real column; falling back to the resource's own uuid column
    // (usually first) would produce a nonsensical `uuid=='OK'` example, so
    // prefer any other column over that specifically.
    let condition_field = resource
        .columns
        .iter()
        .find(|c| c.as_str() == "state")
        .or_else(|| resource.columns.get(1))
        .or_else(|| resource.columns.first())
        .map(String::as_str)
        .unwrap_or("state");
    let mut md = format!(
        "# `{cmd}`\n\nPoll {} until a `--jmespath` condition against it stops evaluating to \
         `false`/`null`, or error on timeout. Client-side polling -- Waldur's API has no \
         server-side watch/push mechanism.\n\n## Usage\n\n```bash\n{cmd} <UUID> --jmespath EXPR [OPTIONS]\n```\n",
        resource.about.to_lowercase()
    );
    md += &options_table(&[
        ("<UUID>".into(), "positional, required".into(), "uuid of the resource.".into()),
        (
            "--jmespath EXPR".into(),
            "string, required".into(),
            "Condition to poll for, evaluated against the fetched object on every poll.".into(),
        ),
        (
            "--timeout N".into(),
            "integer".into(),
            "Seconds to wait for the condition before giving up (default 600).".into(),
        ),
        ("--interval N".into(), "integer".into(), "Seconds between polls (default 3).".into()),
    ]);
    md += &format!("\n## Examples\n\n```bash\n{cmd} <uuid> --jmespath \"{condition_field}=='OK'\"\n```\n");
    md += GLOBAL_OPTIONS;
    page(group, &resource.name, "wait", md)
}

fn action_page(
    group: &str,
    resource: &Resource,
    action: &ExtractedAction,
    request_json_schemas_unused: &HashMap<String, String>,
    request_skeletons: &HashMap<String, String>,
) -> ReferencePage {
    let _ = request_json_schemas_unused;
    let verb = cli_verb(&action.name);
    let cmd = format!("waldur-cli {group} {} {verb}", resource.name);
    let about = format!("{} {}", capitalize(&action.name.replace('_', " ")), resource.about.to_lowercase());
    let op = &action.operation;

    match &op.request_body_type {
        Some(type_name) => {
            let mut md = format!(
                "# `{cmd}`\n\n{about}.\n\n## Usage\n\n```bash\n{cmd} <UUID> (--request JSON | --request-file PATH | --generate-skeleton)\n```\n"
            );
            md += &options_table(&[
                ("<UUID>".into(), "positional, required".into(), "uuid of the resource.".into()),
                ("--request JSON".into(), "string".into(), "Request body as inline JSON.".into()),
                (
                    "--request-file PATH".into(),
                    "path".into(),
                    "Read the request body from a JSON or YAML file.".into(),
                ),
                (
                    "--generate-skeleton [FORMAT]".into(),
                    "json|yaml".into(),
                    "Print a fillable template and exit.".into(),
                ),
            ]);
            let skeleton = request_skeletons.get(type_name).map(String::as_str);
            md += &format!(
                "\n## Examples\n\n```bash\n{cmd} <uuid> --generate-skeleton\n```\n"
            );
            if let Some(skeleton) = skeleton {
                md += &format!(
                    "\n```bash\n{cmd} <uuid> --request '{}'\n```\n",
                    compact_json(skeleton)
                );
            }
            md += GLOBAL_OPTIONS;
            page(group, &resource.name, &action.name, md)
        }
        None => {
            let method = op.http_verb.to_uppercase();
            let mut md = format!("# `{cmd}`\n\n{about}.\n\n## Usage\n\n```bash\n{cmd} <UUID>\n```\n");
            md += &options_table(&[(
                "<UUID>".into(),
                "positional, required".into(),
                "uuid of the resource.".into(),
            )]);
            md += &format!(
                "\n## Examples\n\n```bash\n{cmd} <uuid>\n```\n\n(sends a bodyless {method} to `{}`)\n",
                op.path
            );
            md += GLOBAL_OPTIONS;
            page(group, &resource.name, &action.name, md)
        }
    }
}

/// Top-level `docs/reference/README.md` and one `docs/reference/{group}/
/// README.md` per group, both plain link lists -- GitHub renders a
/// directory's README.md automatically, so this is the natural landing
/// page whether browsing on GitHub or through the mkdocs site this syncs
/// into.
fn index_pages(entries: &[(String, String, Vec<String>)]) -> Vec<ReferencePage> {
    let mut pages = Vec::new();
    let mut groups: Vec<&str> = entries.iter().map(|(g, _, _)| g.as_str()).collect();
    groups.sort();
    groups.dedup();

    let mut top = String::from("# waldur-cli command reference\n\nOne page per command. Generated from the same schema data the CLI itself is built from -- see [Development](../development.md) for how.\n\n");
    for g in &groups {
        top += &format!("- [{g}](./{g}/)\n");
    }
    pages.push(ReferencePage { relative_path: "README.md".to_string(), content: top });

    for g in &groups {
        let mut body = format!("# `waldur-cli {g}`\n\n");
        let mut resources: Vec<&(String, String, Vec<String>)> =
            entries.iter().filter(|(grp, _, _)| grp == g).collect();
        resources.sort_by(|a, b| a.1.cmp(&b.1));
        for (_, resource, verbs) in resources.drain(..) {
            body += &format!("## {resource}\n\n");
            for v in verbs {
                let cli = cli_verb(v);
                body += &format!("- [{cli}](./{resource}-{cli}.md)\n");
            }
            body += "\n";
        }
        pages.push(ReferencePage { relative_path: format!("{g}/README.md"), content: body });
    }
    pages
}

/// Generates every reference page: one per full command, plus the README
/// indexes. Mirrors `codegen::generate_resource_module`'s own verb-by-verb
/// structure (known verbs, then provision/terminate, then wait, then
/// discovered actions), so the reference stays a faithful description of
/// exactly what that function actually emits.
#[allow(clippy::too_many_arguments)]
pub fn generate(
    manifest: &Manifest,
    operations: &HashMap<String, ExtractedOperation>,
    field_enum_values: &HashMap<String, Vec<String>>,
    order_enum_values: &HashMap<String, Vec<String>>,
    request_skeletons: &HashMap<String, String>,
    request_json_schemas: &HashMap<String, String>,
    order_skeletons: &HashMap<String, String>,
    resource_actions: &HashMap<String, Vec<ExtractedAction>>,
) -> Result<Vec<ReferencePage>> {
    let mut pages = Vec::new();
    let mut index_entries: Vec<(String, String, Vec<String>)> = Vec::new();

    for group in &manifest.group {
        for resource in &group.resource {
            let mut verbs_for_index = Vec::new();

            for verb in KNOWN_VERBS {
                let Some(method_name) = resource.commands.get(*verb) else { continue };
                let op = operations.get(method_name).with_context(|| {
                    format!(
                        "reference_docs: operation `{method_name}` (group `{}`, resource `{}`, verb `{verb}`) not extracted",
                        group.name, resource.name
                    )
                })?;
                verbs_for_index.push((*verb).to_string());
                pages.push(match *verb {
                    "list" => list_page(&group.name, resource, op, field_enum_values, order_enum_values),
                    "get" => get_page(&group.name, resource, op),
                    "create" => create_page(&group.name, resource, op, request_skeletons),
                    "update" => update_page(&group.name, resource, op, request_skeletons),
                    "delete" => delete_page(&group.name, resource, op),
                    _ => continue,
                });
            }

            if resource.order.is_some() {
                verbs_for_index.push("provision".to_string());
                verbs_for_index.push("terminate".to_string());
                pages.push(provision_page(&group.name, resource, order_skeletons));
                pages.push(terminate_page(&group.name, resource));
            }

            if resource.commands.contains_key("get") {
                verbs_for_index.push("wait".to_string());
                pages.push(wait_page(&group.name, resource));
            }

            if let Some(actions) = resource_actions.get(&resource.name) {
                for action in actions {
                    verbs_for_index.push(action.name.clone());
                    pages.push(action_page(&group.name, resource, action, request_json_schemas, request_skeletons));
                }
            }

            if !verbs_for_index.is_empty() {
                index_entries.push((group.name.clone(), resource.name.clone(), verbs_for_index));
            }
        }
    }

    pages.extend(index_pages(&index_entries));
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ExtractedParam;
    use std::collections::BTreeMap;

    fn resource(name: &str, columns: &[&str]) -> Resource {
        Resource {
            name: name.to_string(),
            about: format!("{name}s"),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            commands: BTreeMap::new(),
            order: None,
            actions: None,
            web: None,
        }
    }

    fn param(name: &str, kind: ParamKind) -> ExtractedParam {
        ExtractedParam { name: name.to_string(), kind }
    }

    fn op_with_params(params: Vec<ExtractedParam>) -> ExtractedOperation {
        ExtractedOperation {
            operation_id: "thing_list".to_string(),
            path: "/api/things/".to_string(),
            http_verb: "get".to_string(),
            path_param: None,
            query_params: params,
            field_enum_name: None,
            has_order: false,
            order_enum_name: None,
            request_body_type: None,
        }
    }

    // -- pick_example_filter -----------------------------------------------

    #[test]
    fn pick_example_filter_prefers_state_when_present() {
        let op = op_with_params(vec![
            param("cores__gte", ParamKind::RequiredI64),
            param("state", ParamKind::OptionalStr),
            param("name_exact", ParamKind::OptionalStr),
        ]);
        assert_eq!(pick_example_filter(&op), Some(("state", "OK")));
    }

    #[test]
    fn pick_example_filter_falls_back_to_name_exact() {
        let op = op_with_params(vec![
            param("cores__gte", ParamKind::RequiredI64),
            param("name_exact", ParamKind::OptionalStr),
        ]);
        assert_eq!(pick_example_filter(&op), Some(("name_exact", "example")));
    }

    #[test]
    fn pick_example_filter_falls_back_to_first_param_when_no_preferred_key_matches() {
        let op = op_with_params(vec![param("cores__gte", ParamKind::RequiredI64)]);
        assert_eq!(pick_example_filter(&op), Some(("cores__gte", "1")));
    }

    #[test]
    fn pick_example_filter_none_when_no_filters_at_all() {
        let op = op_with_params(vec![]);
        assert_eq!(pick_example_filter(&op), None);
    }

    // -- wait_page: never conditions the example on the resource's own uuid

    #[test]
    fn wait_page_example_prefers_state_column() {
        let resource = resource("thing", &["uuid", "name", "state", "project_name"]);
        let rendered = wait_page("openstack", &resource).content;
        assert!(rendered.contains("--jmespath \"state=='OK'\""));
    }

    #[test]
    fn wait_page_example_never_conditions_on_uuid_even_without_a_state_column() {
        // A resource like `flavor` has no "state" column at all -- the
        // example must still not become the nonsensical `uuid=='OK'`.
        let resource = resource("flavor", &["uuid", "name", "cores"]);
        let rendered = wait_page("openstack", &resource).content;
        assert!(!rendered.contains("uuid=='OK'"));
        assert!(rendered.contains("--jmespath \"name=='OK'\""));
    }

    // -- list_page: --order example must be an actual valid order value ----

    #[test]
    fn list_page_order_example_uses_a_real_enum_value_not_an_arbitrary_column() {
        let resource = resource("flavor", &["uuid", "name", "cores"]);
        let mut op = op_with_params(vec![]);
        op.has_order = true;
        op.order_enum_name = Some("FlavorOEnum".to_string());
        let order_enum_values: HashMap<String, Vec<String>> = [(
            "FlavorOEnum".to_string(),
            vec!["-cores".to_string(), "cores".to_string()],
        )]
        .into();
        let empty: HashMap<String, Vec<String>> = HashMap::new();

        let rendered = list_page("openstack", &resource, &op, &empty, &order_enum_values).content;

        // "uuid" (the resource's own first column) is not a valid --order
        // value for flavor -- the example must use "cores" instead.
        assert!(rendered.contains("--order cores --limit 1"));
        assert!(!rendered.contains("--order uuid"));
    }

    #[test]
    fn list_page_order_example_falls_back_to_first_column_without_a_resolvable_enum() {
        let resource = resource("customer", &["uuid", "name"]);
        let mut op = op_with_params(vec![]);
        op.has_order = true;
        op.order_enum_name = None; // e.g. customers' bare `string` "o" param
        let empty: HashMap<String, Vec<String>> = HashMap::new();

        let rendered = list_page("team", &resource, &op, &empty, &empty).content;

        assert!(rendered.contains("--order uuid --limit 1"));
    }

    // -- compact_json --------------------------------------------------

    #[test]
    fn compact_json_minifies_pretty_printed_input() {
        let pretty = "{\n  \"name\": \"\",\n  \"count\": 0\n}";
        assert_eq!(compact_json(pretty), r#"{"count":0,"name":""}"#);
    }

    #[test]
    fn compact_json_falls_back_to_raw_text_on_invalid_json() {
        assert_eq!(compact_json("not json"), "not json");
    }
}
