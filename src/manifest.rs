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
}

/// Verbs we know how to generate CLI handling for. Order here is also the
/// order subcommands are emitted in.
pub const KNOWN_VERBS: &[&str] = &["list", "get", "create", "update", "delete"];
