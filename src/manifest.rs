use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub group: Vec<Group>,
}

#[derive(Debug, Deserialize)]
pub struct Group {
    pub name: String,
    pub about: String,
    pub resource: Vec<Resource>,
}

#[derive(Debug, Deserialize)]
pub struct Resource {
    pub name: String,
    pub about: String,
    pub columns: Vec<String>,
    #[serde(deserialize_with = "deserialize_commands")]
    pub commands: BTreeMap<String, String>,
    /// Set for resources provisioned through Waldur's marketplace order flow
    /// (OpenStack tenant/instance/volume) rather than a direct REST create --
    /// adds `provision`/`terminate` subcommands. See `OrderConfig`.
    #[serde(default)]
    pub order: Option<OrderConfig>,
    /// Set to auto-discover this resource's custom actions -- Waldur's
    /// convention for state-changing operations that aren't a plain REST
    /// create/update/delete (start/stop/restart, attach/detach, approve/
    /// reject, ...), each a POST at `{this resource's own uuid-scoped
    /// path}{action}/`. Absent means no discovery at all (opt-in per
    /// resource, so a regen doesn't suddenly sprout new commands for every
    /// resource at once). See `ActionsConfig`.
    #[serde(default)]
    pub actions: Option<ActionsConfig>,
    /// Set to give `get` a `--web` flag that opens this resource in Waldur's
    /// web UI (HomePort) instead of printing it. HomePort's routing isn't
    /// part of the OpenAPI schema -- both `path` and `uuid_field` are read
    /// from HomePort's own source (`src/*/routes.ts`), so this is opt-in per
    /// resource and needs re-checking there if HomePort's routes change.
    /// See `WebConfig`.
    #[serde(default)]
    pub web: Option<WebConfig>,
}

/// HomePort-view config for a resource, used by `get --web`. `path` is a
/// HomePort route template containing a literal `{uuid}` placeholder (e.g.
/// `/projects/{uuid}/`). `uuid_field` overrides what gets substituted into
/// it: by default the CLI's own `--uuid` argument, but some resources
/// (OpenStack instance/volume/tenant) are shown in HomePort keyed by their
/// *marketplace* resource uuid rather than their own -- for those, set
/// `uuid_field` to the response field to read it from instead (e.g.
/// `"marketplace_resource_uuid"`).
#[derive(Debug, Deserialize)]
pub struct WebConfig {
    pub path: String,
    #[serde(default)]
    pub uuid_field: Option<String>,
}

/// Custom-action discovery config for a resource. Discovery itself is
/// unconditional once this is present (every `{uuid}/{action}/` path found
/// under the resource's own base path gets a CLI verb) -- `exclude` is an
/// opt-out list for the ones that aren't meant for direct CLI use (Waldur's
/// own `pull`/`sync_*` housekeeping actions, mostly), not an opt-in
/// allow-list, so newly-added actions on the Waldur side show up on the
/// next regen without needing a commands.toml change to notice them.
#[derive(Debug, Deserialize, Default)]
pub struct ActionsConfig {
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Marketplace-order provisioning config for a resource. `offering_type`
/// (e.g. `OpenStack.Instance`), when set, both pins the offering kind and, by
/// Waldur's schema-naming convention (`OpenStackInstanceCreateOrderAttributes`),
/// locates the typed attributes schema used for `provision`'s
/// `--generate-skeleton`. Omit it for a generic provisioner that works against
/// any offering: the skeleton then uses `GenericOrderAttributes` and the
/// caller supplies the offering-specific attributes themselves.
#[derive(Debug, Deserialize)]
pub struct OrderConfig {
    #[serde(default)]
    pub offering_type: Option<String>,
}

/// Verbs we know how to generate CLI handling for. Order here is also the
/// order subcommands are emitted in.
pub const KNOWN_VERBS: &[&str] = &["list", "get", "create", "update", "delete"];

/// operationId suffixes for each of `KNOWN_VERBS`, matched by index --
/// Waldur/DRF's standard ViewSet action names (`{prefix}_list`,
/// `{prefix}_retrieve`, ...).
const VERB_OPERATION_SUFFIXES: &[&str] = &["list", "retrieve", "create", "update", "destroy"];

/// A resource's `[group.resource.commands]` in `commands.toml` is a compact
/// `operation_prefix` (+ opt-out `exclude`) rather than the five `{prefix}_
/// {verb}` operationIds spelled out individually -- every resource in this
/// manifest follows that naming convention, so writing it out five times per
/// resource was pure repetition, not meaningful configuration. `operation_
/// prefix` is the resource's real operationId prefix, which isn't always
/// identical to its own `name` (e.g. the `offering` resource's is
/// `marketplace_public_offerings`, not `marketplace_offerings`) -- so this
/// isn't guessing/auto-discovery, just not repeating a value that's given
/// explicitly once. A resource whose operationIds don't fit this pattern at
/// all would need this deserializer extended with an explicit per-verb
/// override; none currently do.
fn deserialize_commands<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Raw {
        operation_prefix: String,
        #[serde(default)]
        exclude: Vec<String>,
    }
    let raw = Raw::deserialize(deserializer)?;
    Ok(KNOWN_VERBS
        .iter()
        .copied()
        .zip(VERB_OPERATION_SUFFIXES.iter().copied())
        .filter(|(verb, _)| !raw.exclude.iter().any(|e| e.as_str() == *verb))
        .map(|(verb, suffix)| (verb.to_string(), format!("{}_{}", raw.operation_prefix, suffix)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_resource(commands_toml: &str) -> Resource {
        toml::from_str(&format!(
            r#"
name = "thing"
about = "Things"
columns = ["uuid"]
{commands_toml}
"#
        ))
        .expect("test fixture TOML should parse")
    }

    #[test]
    fn deserialize_commands_derives_all_five_verbs_by_default() {
        let resource = parse_resource(
            r#"
[commands]
operation_prefix = "things"
"#,
        );
        assert_eq!(
            resource.commands,
            BTreeMap::from([
                ("list".to_string(), "things_list".to_string()),
                ("get".to_string(), "things_retrieve".to_string()),
                ("create".to_string(), "things_create".to_string()),
                ("update".to_string(), "things_update".to_string()),
                ("delete".to_string(), "things_destroy".to_string()),
            ])
        );
    }

    #[test]
    fn deserialize_commands_respects_exclude() {
        let resource = parse_resource(
            r#"
[commands]
operation_prefix = "things"
exclude = ["create", "delete"]
"#,
        );
        assert_eq!(
            resource.commands,
            BTreeMap::from([
                ("list".to_string(), "things_list".to_string()),
                ("get".to_string(), "things_retrieve".to_string()),
                ("update".to_string(), "things_update".to_string()),
            ])
        );
    }

    #[test]
    fn deserialize_commands_supports_a_prefix_that_differs_from_the_resource_name() {
        // Mirrors the real `offering` resource: its operationIds are
        // `marketplace_public_offerings_*`, not derived from its own `name`.
        let resource = parse_resource(
            r#"
[commands]
operation_prefix = "marketplace_public_offerings"
exclude = ["create", "update", "delete"]
"#,
        );
        assert_eq!(resource.commands.get("list").map(String::as_str), Some("marketplace_public_offerings_list"));
        assert_eq!(resource.commands.get("get").map(String::as_str), Some("marketplace_public_offerings_retrieve"));
    }

    #[test]
    fn deserialize_commands_missing_operation_prefix_is_a_clear_error() {
        let err = toml::from_str::<Resource>(
            r#"
name = "thing"
about = "Things"
columns = ["uuid"]
[commands]
exclude = ["create"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("operation_prefix"));
    }
}
