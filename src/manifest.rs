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
