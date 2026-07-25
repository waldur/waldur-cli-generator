use serde::Deserialize;
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
