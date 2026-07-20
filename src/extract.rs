use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use syn::{File, FnArg, ImplItem, Item, Pat, Type};

/// How a single rs-client method parameter maps onto the generated CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamKind {
    /// `impl AsRef<str>` -- always the required path param (uuid).
    RequiredStr,
    /// `Option<impl AsRef<str>>` -- optional string query filter.
    OptionalStr,
    RequiredBool,
    OptionalBool,
    RequiredI64,
    OptionalI64,
    /// A bare named type (e.g. `CustomerRequest`) -- the JSON request body.
    JsonBody(String),
    /// `Option<SomeNamedType>` -- an optional JSON body (rare; treated like
    /// JsonBody but the flag isn't required).
    OptionalJsonBody(String),
    /// Recognized as "just a filter we don't expose," safe to hardcode to
    /// `None` when calling the method (e.g. `Option<Vec<SomeEnum>>`).
    SkippedOptional,
    /// A required parameter of a shape we don't know how to produce a value
    /// for. Generation must fail loudly for the owning method rather than
    /// silently emit code that can't compile or is missing a required call.
    SkippedRequired,
}

#[derive(Debug, Clone)]
pub struct ExtractedParam {
    pub name: String,
    pub kind: ParamKind,
}

#[derive(Debug, Clone)]
pub struct ExtractedMethod {
    pub name: String,
    pub params: Vec<ExtractedParam>,
}

/// Parse `client.rs` and pull out the signatures of exactly the methods in
/// `wanted` (the manifest's referenced method names), from `impl HttpClient`
/// blocks. Errors if any wanted method isn't found at all -- a typo'd or
/// renamed method name in commands.toml should fail generation, not silently
/// produce a CLI missing a command.
pub fn extract_methods(source: &str, wanted: &[String]) -> Result<HashMap<String, ExtractedMethod>> {
    let file: File = syn::parse_file(source).context("failed to parse rs-client's client.rs")?;
    let wanted_set: HashSet<&str> = wanted.iter().map(String::as_str).collect();
    let mut found = HashMap::new();

    for item in &file.items {
        let Item::Impl(item_impl) = item else { continue };
        for impl_item in &item_impl.items {
            let ImplItem::Fn(method) = impl_item else { continue };
            let name = method.sig.ident.to_string();
            if !wanted_set.contains(name.as_str()) {
                continue;
            }
            let mut params = Vec::new();
            for arg in &method.sig.inputs {
                let FnArg::Typed(pat_type) = arg else { continue }; // skip &self
                let Pat::Ident(pat_ident) = &*pat_type.pat else { continue };
                params.push(ExtractedParam {
                    name: pat_ident.ident.to_string(),
                    kind: classify_type(&pat_type.ty),
                });
            }
            found.insert(name.clone(), ExtractedMethod { name, params });
        }
    }

    let missing: Vec<&str> = wanted
        .iter()
        .map(String::as_str)
        .filter(|n| !found.contains_key(*n))
        .collect();
    if !missing.is_empty() {
        bail!(
            "commands.toml references method(s) not found on HttpClient in rs-client's \
             client.rs (typo, or rs-client's API surface changed): {}",
            missing.join(", ")
        );
    }

    Ok(found)
}

fn classify_type(ty: &Type) -> ParamKind {
    let raw = quote::quote!(#ty).to_string();
    let s = raw.replace(' ', "");
    match s.as_str() {
        "implAsRef<str>" => ParamKind::RequiredStr,
        "Option<implAsRef<str>>" => ParamKind::OptionalStr,
        "bool" => ParamKind::RequiredBool,
        "Option<bool>" => ParamKind::OptionalBool,
        "i64" => ParamKind::RequiredI64,
        "Option<i64>" => ParamKind::OptionalI64,
        _ => {
            if let Some(inner) = s.strip_prefix("Option<").and_then(|s| s.strip_suffix('>')) {
                if is_simple_type_name(inner) {
                    ParamKind::OptionalJsonBody(inner.to_string())
                } else {
                    ParamKind::SkippedOptional
                }
            } else if is_simple_type_name(&s) {
                ParamKind::JsonBody(s)
            } else {
                ParamKind::SkippedRequired
            }
        }
    }
}

/// True for a bare CamelCase type name like `CustomerRequest`, false for
/// anything with generics/paths in it (`Vec<...>`, `Option<...>`, etc.) --
/// those are the cases we don't have a CLI mapping for.
fn is_simple_type_name(s: &str) -> bool {
    matches!(s.chars().next(), Some(c) if c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
