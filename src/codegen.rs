use crate::manifest::{Manifest, Resource, KNOWN_VERBS};
use crate::schema::{ExtractedAction, ExtractedOperation, ParamKind};
use anyhow::{bail, Context, Result};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::HashMap;

fn pascal_case(kebab_or_snake: &str) -> String {
    kebab_or_snake
        .split(|c| c == '-' || c == '_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn snake_ident(kebab_or_snake: &str) -> proc_macro2::Ident {
    format_ident!("{}", kebab_or_snake.replace('-', "_"))
}

/// Builds a Rust identifier for a schema-supplied name, raw-escaping it
/// (`r#type`) if it collides with a keyword (e.g. Waldur's `type` filter
/// param) -- unlike an identifier, the wire-level query param name must stay
/// the plain schema string, never raw-escaped.
fn field_ident(name: &str) -> proc_macro2::Ident {
    if syn::parse_str::<syn::Ident>(name).is_ok() {
        format_ident!("{}", name)
    } else {
        format_ident!("r#{}", name)
    }
}

/// Maps a query param's schema-derived kind to a `crate::filter::FilterKind`
/// expression for the resource's `FILTER_SPEC`, so `--filter key=value` can
/// validate the value's type client-side. `None` for params that can't be
/// exposed as a filter at all (an unrecognized required shape is still a
/// hard failure -- generation can't silently drop a required filter).
fn filter_kind_expr(param: &crate::schema::ExtractedParam) -> Result<Option<TokenStream>> {
    Ok(match &param.kind {
        ParamKind::RequiredStr | ParamKind::OptionalStr => Some(quote! { crate::filter::FilterKind::Str }),
        ParamKind::RequiredBool | ParamKind::OptionalBool => Some(quote! { crate::filter::FilterKind::Bool }),
        ParamKind::RequiredI64 | ParamKind::OptionalI64 => Some(quote! { crate::filter::FilterKind::I64 }),
        ParamKind::SkippedOptional => None,
        ParamKind::SkippedRequired => {
            bail!(
                "parameter `{}` has a type this generator can't map to a --filter value \
                 (not a string/bool/i64 shape) -- either extend classify_param() \
                 in schema.rs, or drop this method from commands.toml",
                param.name
            );
        }
    })
}

/// Maps the schema's HTTP verb for an operation to a `reqwest::Method`
/// expression -- driven by the schema rather than the CLI verb name, so a
/// resource that genuinely uses e.g. PATCH instead of PUT for `update`
/// generates correctly instead of silently sending the wrong method.
fn http_method_expr(op: &ExtractedOperation) -> Result<TokenStream> {
    Ok(match op.http_verb.as_str() {
        "get" => quote! { reqwest::Method::GET },
        "post" => quote! { reqwest::Method::POST },
        "put" => quote! { reqwest::Method::PUT },
        "patch" => quote! { reqwest::Method::PATCH },
        "delete" => quote! { reqwest::Method::DELETE },
        other => bail!(
            "operation `{}` uses HTTP verb `{other}`, which this generator doesn't know how to map",
            op.operation_id
        ),
    })
}

/// Builds the expression for the request path at generation time, splitting
/// the schema's literal path template (e.g. `/api/customers/{uuid}/`) around
/// its path parameter, if it has one -- resolved once here rather than
/// re-parsing the template at runtime on every call.
fn build_path_expr(op: &ExtractedOperation) -> Result<TokenStream> {
    match &op.path_param {
        Some(param_name) => {
            let placeholder = format!("{{{param_name}}}");
            let (prefix, suffix) = op.path.split_once(&placeholder).with_context(|| {
                format!(
                    "operation `{}`: path `{}` doesn't contain its own path param `{{{param_name}}}`",
                    op.operation_id, op.path
                )
            })?;
            let ident = field_ident(param_name);
            Ok(quote! { format!("{}{}{}", #prefix, args.#ident, #suffix) })
        }
        None => {
            let path = &op.path;
            Ok(quote! { #path.to_string() })
        }
    }
}

/// Statements binding `let path = ...;` for a create/update operation, where
/// the path param (if any) is an `Option` positional -- enforced present here
/// at runtime rather than by clap, so `--generate-skeleton` stays reachable
/// without it. Assumes `anyhow::Context` is in scope (create/update import it).
fn body_path_stmts(op: &ExtractedOperation) -> Result<TokenStream> {
    match &op.path_param {
        None => {
            let path = &op.path;
            Ok(quote! { let path = #path.to_string(); })
        }
        Some(param_name) => {
            let placeholder = format!("{{{param_name}}}");
            let (prefix, suffix) = op.path.split_once(&placeholder).with_context(|| {
                format!(
                    "operation `{}`: path `{}` doesn't contain its own path param `{{{param_name}}}`",
                    op.operation_id, op.path
                )
            })?;
            let ident = field_ident(param_name);
            let msg = format!("this command requires a <{param_name}> argument");
            Ok(quote! {
                let #ident = args.#ident.as_deref().context(#msg)?;
                let path = format!("{}{}{}", #prefix, #ident, #suffix);
            })
        }
    }
}

/// What emitting one verb (or a small, tightly-coupled group of verbs, like
/// provision+terminate) contributes to a resource's generated module: its
/// Command enum variant(s), match arm(s), Args struct(s), and any extra
/// top-level `const`(s) it needs (a skeleton, a filter spec, ...). Kept
/// together in one struct rather than several parallel `Vec`s/flags, since
/// every emitter below produces all of these in lockstep.
#[derive(Debug, Default)]
struct EmittedVerb {
    variant: TokenStream,
    arm: TokenStream,
    args_struct: TokenStream,
    consts: TokenStream,
    /// Whether this verb's arm needs `anyhow::Context` in scope. Only
    /// `update`'s required-uuid unwrap does, currently (see
    /// `body_path_stmts`) -- everything else either doesn't `.context(...)`
    /// at all, or (as of the schema-validated-request-body change) returns
    /// its own fully-formed error instead of needing a wrapper.
    needs_context: bool,
}

/// The positional path-param field every non-collection-level verb's Args
/// struct starts with (e.g. `pub uuid: String`), or `None` for a verb with
/// no path param (`list`, `create`). `optional` makes it `Option<String>`
/// instead -- only `update` does this, so `--generate-skeleton` stays
/// reachable without one (enforced present at runtime instead, see
/// `body_path_stmts`).
fn path_param_field(op: &ExtractedOperation, optional: bool) -> Option<TokenStream> {
    let path_param = op.path_param.as_ref()?;
    let ident = field_ident(path_param);
    Some(if optional {
        quote! { pub #ident: Option<String>, }
    } else {
        quote! { pub #ident: String, }
    })
}

/// Bails if a non-`list` operation somehow has query parameters -- this
/// generator only supports query filters on `list`.
fn assert_no_query_params(resource: &Resource, verb: &str, method_name: &str, op: &ExtractedOperation) -> Result<()> {
    if !op.query_params.is_empty() {
        bail!(
            "resource `{}`, verb `{verb}` ({method_name}) has query parameter(s) \
             {:?} -- this generator only supports query filters on `list`, extend \
             codegen.rs if a non-list verb genuinely needs one",
            resource.name,
            op.query_params.iter().map(|p| &p.name).collect::<Vec<_>>()
        );
    }
    Ok(())
}

fn emit_list_verb(
    resource: &Resource,
    resource_pascal: &str,
    resource_enum_ident: &proc_macro2::Ident,
    op: &ExtractedOperation,
    method_name: &str,
    list_has_project: bool,
    field_enum_values: &HashMap<String, Vec<String>>,
) -> Result<EmittedVerb> {
    let mut field_defs = Vec::new();
    if let Some(field) = path_param_field(op, false) {
        field_defs.push(field);
    }

    // Every real query filter goes through one generic --filter KEY=VALUE
    // flag (AWS --filters / kubectl --field-selector style) instead of a
    // dedicated flag per field -- some resources have 20+ filterable
    // fields, and a single uniform pattern is both a smaller --help and one
    // thing to learn across every resource (mirrors --request's own move
    // away from a flag per request-body field). FILTER_SPEC (built from the
    // same op.query_params used below) is what makes this still
    // client-side-validated rather than a blind passthrough: a bad key or
    // wrongly-typed value is rejected locally.
    let spec_entries: Vec<TokenStream> = op
        .query_params
        .iter()
        .map(|param| {
            let name = &param.name;
            filter_kind_expr(param)
                .with_context(|| format!("in resource `{}`, verb `list` ({method_name})", resource.name))
                .map(|kind| kind.map(|k| quote! { (#name, #k) }))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let consts = quote! {
        const FILTER_SPEC: &[(&str, crate::filter::FilterKind)] = &[#(#spec_entries),*];
    };
    field_defs.push(quote! {
        /// Filter results server-side, KEY=VALUE (repeatable). See
        /// --help's error on an unknown key for the valid keys.
        #[arg(long = "filter", value_name = "KEY=VALUE")]
        pub filter: Vec<String>,
    });
    // Named jmespath, not query: several resources have a real `query`
    // filter field of their own (e.g. customers' full-text search) --
    // `--filter query=...` reaches that. A `--query` flag here would
    // silently shadow it (a bare word is itself valid JMESPath, a field
    // projection, so `--query foo` wouldn't even error, just silently do
    // something other than what a user migrating from `--query` on other
    // CLIs would expect).
    field_defs.push(quote! {
        /// Reshape/narrow the already-fetched result with a JMESPath
        /// expression (https://jmespath.org), client-side -- e.g.
        /// [].name or [?blocked==`true`]. Applied after fetching,
        /// before rendering in --format. (Named distinctly from
        /// --filter's own `query` key, several resources' own
        /// full-text search field.)
        #[arg(long)]
        pub jmespath: Option<String>,
    });
    // Client-side only -- not part of the schema (list has no "limit"
    // concept), added here so `list` can bound a huge auto-paginated fetch
    // instead of always fetching everything.
    field_defs.push(quote! {
        /// Stop after this many items (across however many pages that
        /// takes), instead of fetching the complete result
        #[arg(long)]
        pub limit: Option<i64>,
    });
    let doc = "Only fetch these fields from the server (comma-separated), instead of \
               the complete object -- avoids over-fetching. Table output always does \
               this already (using its own display columns); for json/toon/tsv, which \
               fetch the complete object by default, this narrows what they get too.";
    // Waldur's RestrictedSerializerMixin silently ignores unknown field
    // names rather than rejecting them (confirmed against mastermind
    // source) -- an all-invalid --fields list falls back to returning the
    // complete object with no error at all. Validate against the
    // resource's own FieldEnum values client-side instead of letting that
    // happen silently, when we know what they are.
    let valid_values = op.field_enum_name.as_ref().and_then(|name| field_enum_values.get(name));
    field_defs.push(match valid_values {
        Some(values) => quote! {
            #[doc = #doc]
            #[arg(
                long,
                value_delimiter = ',',
                value_parser = clap::builder::PossibleValuesParser::new([#(#values),*]),
            )]
            pub fields: Option<Vec<String>>,
        },
        None => quote! {
            #[doc = #doc]
            #[arg(long, value_delimiter = ',')]
            pub fields: Option<Vec<String>>,
        },
    });

    let args_ident = format_ident!("{}ListArgs", resource_pascal);
    let about = format!("List {}", resource.about.to_lowercase());
    let variant = quote! {
        #[doc = #about]
        List(#args_ident),
    };

    let path = &op.path;
    // Apply the ambient --project scope, unless the user already filtered
    // by project_uuid explicitly (theirs wins). Only for resources whose
    // list actually supports the filter.
    let project_inject = if list_has_project {
        quote! {
            if let Some(project) = project {
                if !query_params.iter().any(|(k, _)| k == "project_uuid") {
                    query_params.push(("project_uuid".to_string(), project.to_string()));
                }
            }
        }
    } else {
        quote! {}
    };
    let output_stmt = quote! {
        let mut query_params: Vec<(String, String)> = crate::filter::parse_filters(&args.filter, FILTER_SPEC)?;
        #project_inject
        // Table always narrows the server fetch to its own display columns
        // (there's never a reason to fetch more than what it shows);
        // json/toon/tsv fetch the complete object by default, but --fields
        // narrows any format that asks for it.
        match &args.fields {
            Some(fields) => {
                for f in fields {
                    query_params.push(("field".to_string(), f.clone()));
                }
            }
            None => {
                if matches!(format, crate::output::OutputFormat::Table) {
                    for f in COLUMNS {
                        query_params.push(("field".to_string(), (*f).to_string()));
                    }
                }
            }
        }
        // ndjson prints as each page arrives instead of buffering the
        // complete result set first -- lower memory, faster first output.
        // Only when there's no --jmespath: a JMESPath expression can
        // reshape/aggregate across the whole array (sort, slice, count,
        // ...), so it still needs the complete result fetched first, same
        // as json/toon/table/tsv.
        if matches!(format, crate::output::OutputFormat::Ndjson) && args.jmespath.is_none() {
            crate::pagination::fetch_all_streaming(
                base_url,
                token,
                #path,
                &query_params,
                args.limit,
                |item| crate::output::print_ndjson_line(&item),
            )
            .await?;
        } else {
            let result = crate::pagination::fetch_all(base_url, token, #path, &query_params, args.limit).await?;
            // table/tsv render exactly these columns (json/toon/ndjson
            // ignore them, showing the complete fetched object regardless)
            // -- when --fields narrowed what was actually fetched, the
            // display columns have to follow the same override, or
            // table/tsv would show a column for every field COLUMNS
            // expects but --fields didn't ask for, which the server
            // response then doesn't have at all (rendering as blank).
            let display_columns: Vec<&str> = match &args.fields {
                Some(fields) => fields.iter().map(String::as_str).collect(),
                None => COLUMNS.to_vec(),
            };
            // --query reshapes the already-fetched result client-side (AWS
            // CLI's --query) -- distinct from --filter, which narrows
            // what's fetched in the first place.
            let result: serde_json::Value = serde_json::Value::Array(result);
            let result = match &args.jmespath {
                Some(expr) => crate::query::apply(result, expr)?,
                None => result,
            };
            crate::output::print_result(&result, &display_columns, format)?;
        }
    };

    let arm = quote! {
        #resource_enum_ident::List(args) => {
            #output_stmt
        }
    };
    let args_struct = quote! {
        #[derive(clap::Args, Debug)]
        pub struct #args_ident {
            #(#field_defs)*
        }
    };

    Ok(EmittedVerb { variant, arm, args_struct, consts, needs_context: false })
}

fn emit_get_verb(
    resource: &Resource,
    resource_pascal: &str,
    resource_enum_ident: &proc_macro2::Ident,
    op: &ExtractedOperation,
    method_name: &str,
) -> Result<EmittedVerb> {
    assert_no_query_params(resource, "get", method_name, op)?;

    let mut field_defs = Vec::new();
    if let Some(field) = path_param_field(op, false) {
        field_defs.push(field);
    }

    let args_ident = format_ident!("{}GetArgs", resource_pascal);
    let about = format!("Get {}", resource.about.to_lowercase());
    let variant = quote! {
        #[doc = #about]
        Get(#args_ident),
    };

    let path_expr = build_path_expr(op)?;
    let method_expr = http_method_expr(op)?;
    let arm = quote! {
        #resource_enum_ident::Get(args) => {
            let path = #path_expr;
            let result = crate::http::call_one(base_url, token, #method_expr, &path, None).await?;
            crate::output::print_result(&result, COLUMNS, format)?;
        }
    };
    let args_struct = quote! {
        #[derive(clap::Args, Debug)]
        pub struct #args_ident {
            #(#field_defs)*
        }
    };

    Ok(EmittedVerb { variant, arm, args_struct, ..Default::default() })
}

/// Shared by `create` and `update` -- their Args structs, skeleton/schema
/// embedding, and request-body handling are identical apart from the HTTP
/// verb and the presence (update) or absence (create) of a path param.
#[allow(clippy::too_many_arguments)]
fn emit_body_verb(
    verb: &str,
    resource: &Resource,
    resource_pascal: &str,
    resource_enum_ident: &proc_macro2::Ident,
    op: &ExtractedOperation,
    method_name: &str,
    request_skeletons: &HashMap<String, String>,
    request_json_schemas: &HashMap<String, String>,
) -> Result<EmittedVerb> {
    assert_no_query_params(resource, verb, method_name, op)?;

    let mut field_defs = Vec::new();
    // update's uuid is `Option<String>` (see `path_param_field`'s doc) --
    // body_path_stmts emits `.context(...)` unwrapping it, the only place
    // left that needs `Context` in scope now that request-body validation
    // (crate::request::validate_request_body) returns its own fully-formed
    // error rather than needing a `.with_context` wrapper.
    if let Some(field) = path_param_field(op, true) {
        field_defs.push(field);
    }
    let needs_context = verb == "update" && op.path_param.is_some();

    // AWS-style body input: inline JSON, a JSON/YAML file, or print a
    // fillable template -- exactly one, enforced by a required arg group.
    // Discoverable in --help without a flag per schema field.
    field_defs.push(quote! {
        /// Request body as inline JSON. Use --generate-skeleton for a
        /// template, or --request-file to read it from a file.
        #[arg(long)]
        pub request: Option<String>,
    });
    field_defs.push(quote! {
        /// Read the request body from a JSON or YAML file (e.g. a
        /// filled-in --generate-skeleton template).
        #[arg(long, value_name = "PATH")]
        pub request_file: Option<std::path::PathBuf>,
    });
    field_defs.push(quote! {
        /// Print a fillable request-body template and exit, instead of
        /// sending a request (json or yaml; default json).
        #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "json", value_name = "FORMAT")]
        pub generate_skeleton: Option<crate::request::SkeletonFormat>,
    });
    let group_name = format!("{}_{verb}_body", resource.name.replace('-', "_"));
    let struct_attr = quote! {
        #[command(group(
            clap::ArgGroup::new(#group_name)
                .required(true)
                .args(["request", "request_file", "generate_skeleton"])
        ))]
    };

    // Embed the fillable template (built from the schema at generation
    // time) so --generate-skeleton needs no runtime schema access.
    let type_name = op.request_body_type.as_deref().with_context(|| {
        format!(
            "resource `{}`, verb `{verb}` ({method_name}): no request body schema \
             to build a --generate-skeleton template from",
            resource.name
        )
    })?;
    let skeleton = request_skeletons.get(type_name).with_context(|| {
        format!("internal error: no skeleton built for request type `{type_name}`")
    })?;
    let const_ident = format_ident!("{}_SKELETON", verb.to_uppercase());
    // Embed the JSON Schema `--request` is validated against at runtime
    // (crate::request::validate_request_body).
    let json_schema = request_json_schemas.get(type_name).with_context(|| {
        format!("internal error: no JSON schema built for request type `{type_name}`")
    })?;
    let schema_const_ident = format_ident!("{}_REQUEST_SCHEMA", verb.to_uppercase());
    let consts = quote! {
        const #const_ident: &str = #skeleton;
        const #schema_const_ident: &str = #json_schema;
    };

    let verb_pascal = pascal_case(verb);
    let args_ident = format_ident!("{}{}Args", resource_pascal, verb_pascal);
    let variant_ident = format_ident!("{}", verb_pascal);
    let about = format!("{verb_pascal} {}", resource.about.to_lowercase());
    let variant = quote! {
        #[doc = #about]
        #variant_ident(#args_ident),
    };

    let path_stmts = body_path_stmts(op)?;
    let method_expr = http_method_expr(op)?;
    let method_str = op.http_verb.to_uppercase();
    let output_stmt = quote! {
        if let Some(fmt) = args.generate_skeleton {
            crate::request::print_skeleton(#const_ident, fmt)?;
            return Ok(());
        }
        let body = crate::request::load_body(args.request.as_deref(), args.request_file.as_deref())?;
        // Validate the body even under --dry-run, so a dry run still
        // catches a malformed request rather than only previewing it.
        crate::request::validate_request_body(#schema_const_ident, &body)?;
        #path_stmts
        if dry_run {
            return crate::output::print_dry_run(#method_str, &path, Some(&body), format);
        }
        let result = crate::http::call_one(base_url, token, #method_expr, &path, Some(&body)).await?;
        crate::output::print_result(&result, COLUMNS, format)?;
    };
    let arm = quote! {
        #resource_enum_ident::#variant_ident(args) => {
            #output_stmt
        }
    };
    let args_struct = quote! {
        #[derive(clap::Args, Debug)]
        #struct_attr
        pub struct #args_ident {
            #(#field_defs)*
        }
    };

    Ok(EmittedVerb { variant, arm, args_struct, consts, needs_context })
}

fn emit_delete_verb(
    resource: &Resource,
    resource_pascal: &str,
    resource_enum_ident: &proc_macro2::Ident,
    op: &ExtractedOperation,
    method_name: &str,
) -> Result<EmittedVerb> {
    assert_no_query_params(resource, "delete", method_name, op)?;

    let mut field_defs = Vec::new();
    if let Some(field) = path_param_field(op, false) {
        field_defs.push(field);
    }

    let args_ident = format_ident!("{}DeleteArgs", resource_pascal);
    let about = format!("Delete {}", resource.about.to_lowercase());
    let variant = quote! {
        #[doc = #about]
        Delete(#args_ident),
    };

    let path_expr = build_path_expr(op)?;
    let method_expr = http_method_expr(op)?;
    let method_str = op.http_verb.to_uppercase();
    let uuid_ident = op
        .path_param
        .as_ref()
        .map(|p| field_ident(p))
        .unwrap_or_else(|| format_ident!("uuid"));
    let arm = quote! {
        #resource_enum_ident::Delete(args) => {
            let path = #path_expr;
            if dry_run {
                return crate::output::print_dry_run(#method_str, &path, None, format);
            }
            let _ = crate::http::call_one(base_url, token, #method_expr, &path, None).await?;
            match format {
                crate::output::OutputFormat::Json | crate::output::OutputFormat::Ndjson => {
                    println!("{}", serde_json::json!({"deleted": true, "uuid": args.#uuid_ident}));
                }
                crate::output::OutputFormat::Table => {
                    println!("Deleted {}", args.#uuid_ident);
                }
                crate::output::OutputFormat::Tsv => {
                    println!("true\t{}", args.#uuid_ident);
                }
                crate::output::OutputFormat::Toon => {
                    println!(
                        "{}",
                        serde_toon::to_string(
                            &serde_json::json!({"deleted": true, "uuid": args.#uuid_ident}),
                        )?
                    );
                }
            }
        }
    };
    let args_struct = quote! {
        #[derive(clap::Args, Debug)]
        pub struct #args_ident {
            #(#field_defs)*
        }
    };

    Ok(EmittedVerb { variant, arm, args_struct, ..Default::default() })
}

/// Marketplace-order provisioning: resources with an `[order]` config get
/// `provision` (submit a marketplace order + poll to completion) and
/// `terminate` (terminate the marketplace resource + poll) subcommands, for
/// the async order flow that has no direct REST create/delete. Emitted
/// together (not through the per-verb KNOWN_VERBS loop, which only knows
/// about verbs tied to a manifest-declared operationId) since they share
/// the same `--request`/`--request-file`/`--generate-skeleton` shape and
/// gating condition.
fn emit_order_verbs(
    resource: &Resource,
    resource_pascal: &str,
    resource_enum_ident: &proc_macro2::Ident,
    order_skeletons: &HashMap<String, String>,
) -> Result<EmittedVerb> {
    let skeleton = order_skeletons.get(&resource.name).with_context(|| {
        format!("internal error: no order skeleton built for resource `{}`", resource.name)
    })?;
    let consts = quote! { const PROVISION_SKELETON: &str = #skeleton; };

    let provision_args = format_ident!("{}ProvisionArgs", resource_pascal);
    let terminate_args = format_ident!("{}TerminateArgs", resource_pascal);
    let provision_about = format!("Provision {} via a marketplace order", resource.about.to_lowercase());
    let terminate_about = format!("Terminate {} via a marketplace order", resource.about.to_lowercase());
    let body_group = format!("{}_provision_body", resource.name.replace('-', "_"));

    let variant = quote! {
        #[doc = #provision_about]
        Provision(#provision_args),
        #[doc = #terminate_about]
        Terminate(#terminate_args),
    };

    let arm = quote! {
        #resource_enum_ident::Provision(args) => {
            if let Some(fmt) = args.generate_skeleton {
                crate::request::print_skeleton(PROVISION_SKELETON, fmt)?;
                return Ok(());
            }
            let body = crate::request::load_body(args.request.as_deref(), args.request_file.as_deref())?;
            crate::order::provision(base_url, token, &body, project, dry_run, !args.no_wait, args.timeout, args.interval, format).await?;
        }
        #resource_enum_ident::Terminate(args) => {
            crate::order::terminate(base_url, token, &args.uuid, args.request.as_deref(), dry_run, !args.no_wait, args.timeout, args.interval, format).await?;
        }
    };

    let args_struct = quote! {
        #[derive(clap::Args, Debug)]
        #[command(group(
            clap::ArgGroup::new(#body_group)
                .required(true)
                .args(["request", "request_file", "generate_skeleton"])
        ))]
        pub struct #provision_args {
            /// The marketplace order body as inline JSON. Use
            /// --generate-skeleton for a template (offering/project plus
            /// this resource's typed attributes), or --request-file to
            /// read it from a file.
            #[arg(long)]
            pub request: Option<String>,
            /// Read the order body from a JSON or YAML file.
            #[arg(long, value_name = "PATH")]
            pub request_file: Option<std::path::PathBuf>,
            /// Print a fillable order template and exit, instead of
            /// submitting (json or yaml; default json).
            #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "json", value_name = "FORMAT")]
            pub generate_skeleton: Option<crate::request::SkeletonFormat>,
            /// Submit the order and return immediately, without polling
            /// it to completion.
            #[arg(long)]
            pub no_wait: bool,
            /// Seconds to wait for the order to reach a terminal state
            /// before giving up (ignored with --no-wait).
            #[arg(long, default_value_t = 600)]
            pub timeout: u64,
            /// Seconds between polls (ignored with --no-wait).
            #[arg(long, default_value_t = 3)]
            pub interval: u64,
        }
        #[derive(clap::Args, Debug)]
        pub struct #terminate_args {
            /// The marketplace resource UUID (a resource's
            /// `marketplace_resource_uuid` field, from get/list) -- not
            /// the plugin resource's own UUID.
            pub uuid: String,
            /// Optional termination attributes as inline JSON, e.g.
            /// '{"delete_volumes": true}'.
            #[arg(long)]
            pub request: Option<String>,
            /// Submit the termination and return immediately, without
            /// polling the order to completion.
            #[arg(long)]
            pub no_wait: bool,
            /// Seconds to wait for the termination order before giving
            /// up (ignored with --no-wait).
            #[arg(long, default_value_t = 600)]
            pub timeout: u64,
            /// Seconds between polls (ignored with --no-wait).
            #[arg(long, default_value_t = 3)]
            pub interval: u64,
        }
    };

    Ok(EmittedVerb { variant, arm, args_struct, consts, needs_context: false })
}

/// `wait`: generic polling for any resource with a `get` -- Waldur's API
/// has no server-side watch/push mechanism, so this is a client-side poll
/// that stops as soon as a --jmespath condition against the fetched object
/// is met, or errors on timeout (mirrors AWS's named waiters / Azure's `az
/// resource wait --custom` / kubectl's `--for=jsonpath=`, generalized via
/// the JMESPath engine already embedded for --jmespath). Not tied to a
/// distinct operationId -- it reuses `get`'s own path -- so it's emitted
/// separately from the per-verb KNOWN_VERBS loop, keyed off `get`'s
/// presence rather than a manifest-declared verb.
fn emit_wait_verb(
    resource: &Resource,
    resource_pascal: &str,
    resource_enum_ident: &proc_macro2::Ident,
    operations: &HashMap<String, ExtractedOperation>,
    get_method_name: &str,
) -> Result<EmittedVerb> {
    let get_op = operations.get(get_method_name).with_context(|| {
        format!(
            "internal error: operation `{get_method_name}` (resource `{}`, verb `get`) \
             was not extracted",
            resource.name
        )
    })?;
    let path_param = get_op.path_param.as_deref().with_context(|| {
        format!(
            "resource `{}`: `get` has no path parameter -- `wait` needs one to know \
             which resource to poll",
            resource.name
        )
    })?;
    let uuid_ident = field_ident(path_param);
    let path_expr = build_path_expr(get_op)?;
    let wait_args = format_ident!("{}WaitArgs", resource_pascal);
    let wait_about = format!("Wait for a --jmespath condition on {}", resource.about.to_lowercase());

    let variant = quote! {
        #[doc = #wait_about]
        Wait(#wait_args),
    };

    let arm = quote! {
        #resource_enum_ident::Wait(args) => {
            let path = #path_expr;
            crate::wait::wait_for(base_url, token, &path, &args.jmespath, args.timeout, args.interval, COLUMNS, format).await?;
        }
    };

    let args_struct = quote! {
        #[derive(clap::Args, Debug)]
        pub struct #wait_args {
            pub #uuid_ident: String,
            /// JMESPath condition to poll for, evaluated against the
            /// fetched object on every poll (e.g. "state=='OK'").
            /// Waiting stops as soon as this evaluates to anything
            /// other than false or null.
            #[arg(long)]
            pub jmespath: String,
            /// Seconds to wait for the condition before giving up.
            #[arg(long, default_value_t = 600)]
            pub timeout: u64,
            /// Seconds between polls.
            #[arg(long, default_value_t = 3)]
            pub interval: u64,
        }
    };

    Ok(EmittedVerb { variant, arm, args_struct, consts: quote! {}, needs_context: false })
}

/// Turns a schema action name like `approve_by_consumer` into a natural
/// sentence-style phrase (`Approve by consumer`) for doc comments -- unlike
/// `pascal_case`, which produces the Rust identifier (`ApproveByConsumer`).
fn capitalize_words(name: &str) -> String {
    let spaced = name.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A custom action discovered by `schema::discover_actions` -- Waldur's
/// convention for state-changing operations that aren't a plain REST
/// create/update/delete (start/stop/restart, attach/detach, approve/reject,
/// ...). Mirrors `emit_body_verb`'s request/request-file/generate-skeleton
/// shape when the action takes a body, or a bare no-body POST (`get`'s
/// shape, but mutating) when it doesn't -- both real cases in practice
/// (`openstack_instances_start` takes nothing; `openstack_volumes_attach`
/// takes a device path).
fn emit_action_verb(
    resource: &Resource,
    resource_pascal: &str,
    resource_enum_ident: &proc_macro2::Ident,
    action: &ExtractedAction,
    request_skeletons: &HashMap<String, String>,
    request_json_schemas: &HashMap<String, String>,
) -> Result<EmittedVerb> {
    let op = &action.operation;
    let action_pascal = pascal_case(&action.name);
    let args_ident = format_ident!("{}{}Args", resource_pascal, action_pascal);
    let variant_ident = format_ident!("{}", action_pascal);
    let about = format!("{} {}", capitalize_words(&action.name), resource.about.to_lowercase());
    let variant = quote! {
        #[doc = #about]
        #variant_ident(#args_ident),
    };

    let mut field_defs = Vec::new();
    if let Some(field) = path_param_field(op, false) {
        field_defs.push(field);
    }

    let path_expr = build_path_expr(op)?;
    let method_expr = http_method_expr(op)?;
    let method_str = op.http_verb.to_uppercase();

    let (output_stmt, struct_attr, consts) = match op.request_body_type.as_deref() {
        Some(type_name) => {
            field_defs.push(quote! {
                /// Request body as inline JSON. Use --generate-skeleton for
                /// a template, or --request-file to read it from a file.
                #[arg(long)]
                pub request: Option<String>,
            });
            field_defs.push(quote! {
                /// Read the request body from a JSON or YAML file (e.g. a
                /// filled-in --generate-skeleton template).
                #[arg(long, value_name = "PATH")]
                pub request_file: Option<std::path::PathBuf>,
            });
            field_defs.push(quote! {
                /// Print a fillable request-body template and exit, instead
                /// of sending a request (json or yaml; default json).
                #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "json", value_name = "FORMAT")]
                pub generate_skeleton: Option<crate::request::SkeletonFormat>,
            });
            let group_name = format!("{}_{}_body", resource.name.replace('-', "_"), action.name);
            let struct_attr = quote! {
                #[command(group(
                    clap::ArgGroup::new(#group_name)
                        .required(true)
                        .args(["request", "request_file", "generate_skeleton"])
                ))]
            };

            let skeleton = request_skeletons.get(type_name).with_context(|| {
                format!("internal error: no skeleton built for request type `{type_name}`")
            })?;
            let const_ident = format_ident!("{}_SKELETON", action.name.to_uppercase());
            let json_schema = request_json_schemas.get(type_name).with_context(|| {
                format!("internal error: no JSON schema built for request type `{type_name}`")
            })?;
            let schema_const_ident = format_ident!("{}_REQUEST_SCHEMA", action.name.to_uppercase());
            let consts = quote! {
                const #const_ident: &str = #skeleton;
                const #schema_const_ident: &str = #json_schema;
            };

            let stmt = quote! {
                if let Some(fmt) = args.generate_skeleton {
                    crate::request::print_skeleton(#const_ident, fmt)?;
                    return Ok(());
                }
                let body = crate::request::load_body(args.request.as_deref(), args.request_file.as_deref())?;
                crate::request::validate_request_body(#schema_const_ident, &body)?;
                let path = #path_expr;
                if dry_run {
                    return crate::output::print_dry_run(#method_str, &path, Some(&body), format);
                }
                let result = crate::http::call_one(base_url, token, #method_expr, &path, Some(&body)).await?;
                crate::output::print_result(&result, COLUMNS, format)?;
            };
            (stmt, struct_attr, consts)
        }
        None => {
            let stmt = quote! {
                let path = #path_expr;
                if dry_run {
                    return crate::output::print_dry_run(#method_str, &path, None, format);
                }
                let result = crate::http::call_one(base_url, token, #method_expr, &path, None).await?;
                crate::output::print_result(&result, COLUMNS, format)?;
            };
            (stmt, quote! {}, quote! {})
        }
    };

    let arm = quote! {
        #resource_enum_ident::#variant_ident(args) => {
            #output_stmt
        }
    };
    let args_struct = quote! {
        #[derive(clap::Args, Debug)]
        #struct_attr
        pub struct #args_ident {
            #(#field_defs)*
        }
    };

    Ok(EmittedVerb { variant, arm, args_struct, consts, needs_context: false })
}

/// One resource's generated file: Args structs + Command enum + run().
#[allow(clippy::too_many_arguments)]
fn generate_resource_module(
    resource: &Resource,
    operations: &HashMap<String, ExtractedOperation>,
    field_enum_values: &HashMap<String, Vec<String>>,
    request_skeletons: &HashMap<String, String>,
    request_json_schemas: &HashMap<String, String>,
    order_skeletons: &HashMap<String, String>,
    resource_actions: &HashMap<String, Vec<ExtractedAction>>,
) -> Result<TokenStream> {
    let resource_pascal = pascal_case(&resource.name);
    let resource_enum_ident = format_ident!("{}Command", resource_pascal);
    let columns = &resource.columns;

    // Whether the ambient `--project` scope applies to this resource: its
    // `list` supports a `project_uuid` filter, or it can be provisioned (every
    // marketplace order needs a project). Drives whether `run` uses the
    // `project` argument or takes it as `_project`.
    let list_has_project = resource
        .commands
        .get("list")
        .and_then(|m| operations.get(m))
        .map(|op| op.query_params.iter().any(|p| p.name == "project_uuid"))
        .unwrap_or(false);
    let uses_project = list_has_project || resource.order.is_some();
    let project_param = if uses_project {
        quote! { project }
    } else {
        quote! { _project }
    };

    // `--dry-run` is honored only by mutating verbs (including every
    // discovered action, which all emit their own `if dry_run` check); a
    // read-only resource takes the flag as `_dry_run` so there's no
    // unused-argument warning.
    let has_mutating_verb = ["create", "update", "delete"]
        .iter()
        .any(|v| resource.commands.contains_key(*v))
        || resource.order.is_some()
        || resource_actions.get(&resource.name).is_some_and(|actions| !actions.is_empty());
    let dry_run_param = if has_mutating_verb {
        quote! { dry_run }
    } else {
        quote! { _dry_run }
    };

    let mut emitted: Vec<EmittedVerb> = Vec::new();

    for verb in KNOWN_VERBS {
        let Some(method_name) = resource.commands.get(*verb) else {
            continue;
        };
        let op = operations.get(method_name).with_context(|| {
            format!(
                "internal error: operation `{method_name}` (resource `{}`, verb `{verb}`) \
                 was not extracted",
                resource.name
            )
        })?;
        emitted.push(match *verb {
            "list" => emit_list_verb(
                resource,
                &resource_pascal,
                &resource_enum_ident,
                op,
                method_name,
                list_has_project,
                field_enum_values,
            )?,
            "get" => emit_get_verb(resource, &resource_pascal, &resource_enum_ident, op, method_name)?,
            "create" | "update" => emit_body_verb(
                verb,
                resource,
                &resource_pascal,
                &resource_enum_ident,
                op,
                method_name,
                request_skeletons,
                request_json_schemas,
            )?,
            "delete" => emit_delete_verb(resource, &resource_pascal, &resource_enum_ident, op, method_name)?,
            other => bail!("internal error: unknown verb `{other}` in KNOWN_VERBS"),
        });
    }

    if resource.order.is_some() {
        emitted.push(emit_order_verbs(resource, &resource_pascal, &resource_enum_ident, order_skeletons)?);
    }

    if let Some(get_method_name) = resource.commands.get("get") {
        emitted.push(emit_wait_verb(resource, &resource_pascal, &resource_enum_ident, operations, get_method_name)?);
    }

    // Auto-discovered custom actions (start/stop/restart, attach/detach,
    // approve/reject, ...), if this resource opted in via [actions]. Guard
    // against a discovered action colliding with one of the fixed verb
    // names above -- Waldur's own naming has never produced one in
    // practice, but silently emitting two enum variants with the same
    // identifier would be a confusing generated-code compile error rather
    // than a clear one here.
    if let Some(actions) = resource_actions.get(&resource.name) {
        let mut used_names: std::collections::HashSet<&str> = resource.commands.keys().map(String::as_str).collect();
        if resource.order.is_some() {
            used_names.insert("provision");
            used_names.insert("terminate");
        }
        if resource.commands.contains_key("get") {
            used_names.insert("wait");
        }
        for action in actions {
            if used_names.contains(action.name.as_str()) {
                bail!(
                    "resource `{}`: discovered action `{}` collides with an existing verb name -- \
                     add it to this resource's [actions] `exclude` list",
                    resource.name,
                    action.name
                );
            }
            emitted.push(emit_action_verb(
                resource,
                &resource_pascal,
                &resource_enum_ident,
                action,
                request_skeletons,
                request_json_schemas,
            )?);
        }
    }

    let verb_variants = emitted.iter().map(|e| &e.variant);
    let verb_arms = emitted.iter().map(|e| &e.arm);
    let args_structs = emitted.iter().map(|e| &e.args_struct);
    let extra_consts = emitted.iter().map(|e| &e.consts);
    let uses_context = emitted.iter().any(|e| e.needs_context);

    let about = &resource.about;
    let columns_len = columns.len();
    let context_import = if uses_context {
        quote! { use anyhow::Context; }
    } else {
        quote! {}
    };

    Ok(quote! {
        //! Generated by waldur-cli-generator from `commands.toml`. Do not edit by hand;
        //! see that repo's README for how to regenerate.
        #![allow(clippy::too_many_arguments)]

        #context_import

        const COLUMNS: &[&str; #columns_len] = &[#(#columns),*];

        #(#extra_consts)*

        #[doc = #about]
        #[derive(clap::Subcommand, Debug)]
        pub enum #resource_enum_ident {
            #(#verb_variants)*
        }

        #(#args_structs)*

        pub async fn run(
            base_url: &str,
            token: Option<&str>,
            #project_param: Option<&str>,
            #dry_run_param: bool,
            command: #resource_enum_ident,
            format: crate::output::OutputFormat,
        ) -> anyhow::Result<()> {
            match command {
                #(#verb_arms)*
            }
            Ok(())
        }
    })
}

thread_local! {
    static ARGS_STRUCTS: std::cell::RefCell<Vec<TokenStream>> = std::cell::RefCell::new(Vec::new());
}

pub struct GeneratedResource {
    pub group_name: String,
    pub resource_name: String,
    pub source: String,
}

/// Everything the generator produces, ready to be written to disk by main.rs.
pub struct GeneratedOutput {
    /// One entry per manifest resource: its rendered `src/commands/<group>/<resource>.rs`.
    pub resources: Vec<GeneratedResource>,
    /// group name -> contents of `src/commands/<group>/mod.rs` (module declarations only).
    pub group_mod_decls: HashMap<String, String>,
    /// Contents of `src/commands/mod.rs`.
    pub commands_mod_decls: String,
    /// Contents of `src/cli.rs`.
    pub cli_source: String,
}

#[allow(clippy::too_many_arguments)]
pub fn generate_all(
    manifest: &Manifest,
    operations: &HashMap<String, ExtractedOperation>,
    field_enum_values: &HashMap<String, Vec<String>>,
    request_skeletons: &HashMap<String, String>,
    request_json_schemas: &HashMap<String, String>,
    order_skeletons: &HashMap<String, String>,
    resource_actions: &HashMap<String, Vec<ExtractedAction>>,
) -> Result<GeneratedOutput> {
    let mut resources = Vec::new();
    let mut group_mod_decls: HashMap<String, String> = HashMap::new();

    for group in &manifest.group {
        let mut resource_mod_decls = Vec::new();
        for resource in &group.resource {
            let tokens = generate_resource_module(
                resource,
                operations,
                field_enum_values,
                request_skeletons,
                request_json_schemas,
                order_skeletons,
                resource_actions,
            )
            .with_context(|| format!("generating group `{}` resource `{}`", group.name, resource.name))?;
            let file: syn::File = syn::parse2(tokens.clone()).with_context(|| {
                format!(
                    "generated code for group `{}` resource `{}` is not valid Rust:\n{}",
                    group.name, resource.name, tokens
                )
            })?;
            let source = prettyplease::unparse(&file);
            resources.push(GeneratedResource {
                group_name: group.name.clone(),
                resource_name: resource.name.clone(),
                source,
            });
            let mod_ident = snake_ident(&resource.name);
            resource_mod_decls.push(format!("pub mod {mod_ident};"));
        }
        group_mod_decls.insert(group.name.clone(), resource_mod_decls.join("\n"));
    }

    let commands_mod_decls: String = manifest
        .group
        .iter()
        .map(|g| format!("pub mod {};", snake_ident(&g.name)))
        .collect::<Vec<_>>()
        .join("\n");

    let cli_source = generate_cli_file(manifest)?;

    Ok(GeneratedOutput {
        resources,
        group_mod_decls,
        commands_mod_decls,
        cli_source,
    })
}

fn generate_cli_file(manifest: &Manifest) -> Result<String> {
    let mut group_variants = Vec::new();
    let mut group_arms = Vec::new();

    for group in &manifest.group {
        let group_pascal = pascal_case(&group.name);
        let group_mod = snake_ident(&group.name);
        let group_enum_ident = format_ident!("{}Command", group_pascal);
        let group_variant_ident = format_ident!("{}", group_pascal);
        let about = &group.about;

        let mut resource_variants = Vec::new();
        let mut resource_arms = Vec::new();
        for resource in &group.resource {
            let resource_pascal = pascal_case(&resource.name);
            let resource_mod = snake_ident(&resource.name);
            let resource_variant_ident = format_ident!("{}", resource_pascal);
            let resource_command_ty = format_ident!("{}Command", resource_pascal);
            let resource_about = &resource.about;
            resource_variants.push(quote! {
                #[doc = #resource_about]
                #[command(subcommand)]
                #resource_variant_ident(
                    crate::commands::#group_mod::#resource_mod::#resource_command_ty,
                ),
            });
            resource_arms.push(quote! {
                #group_enum_ident::#resource_variant_ident(cmd) => {
                    crate::commands::#group_mod::#resource_mod::run(base_url, token, project, dry_run, cmd, format).await
                }
            });
        }

        group_variants.push(quote! {
            #[doc = #about]
            #[command(subcommand)]
            #group_variant_ident(
                #group_enum_ident,
            ),
        });
        group_arms.push(quote! {
            GroupCommand::#group_variant_ident(cmd) => match cmd {
                #(#resource_arms)*
            }
        });

        // Emit each group's Command enum as its own top-level item too.
        ARGS_STRUCTS.with(|cell| {
            cell.borrow_mut().push(quote! {
                #[doc = #about]
                #[derive(clap::Subcommand, Debug)]
                pub enum #group_enum_ident {
                    #(#resource_variants)*
                }
            });
        });
    }

    let group_enums = ARGS_STRUCTS.with(|cell| {
        let v = cell.borrow().clone();
        cell.borrow_mut().clear();
        v
    });

    let tokens = quote! {
        //! Generated by waldur-cli-generator from `commands.toml`. Do not edit by hand.

        #[derive(clap::Subcommand, Debug)]
        pub enum GroupCommand {
            #(#group_variants)*
        }

        #(#group_enums)*

        pub async fn dispatch(
            base_url: &str,
            token: Option<&str>,
            project: Option<&str>,
            dry_run: bool,
            command: GroupCommand,
            format: crate::output::OutputFormat,
        ) -> anyhow::Result<()> {
            match command {
                #(#group_arms)*
            }
        }
    };

    let file: syn::File = syn::parse2(tokens.clone())
        .with_context(|| format!("generated cli.rs is not valid Rust:\n{tokens}"))?;
    Ok(prettyplease::unparse(&file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::OrderConfig;
    use crate::schema::ExtractedParam;
    use std::collections::BTreeMap;

    fn resource(name: &str, commands: &[(&str, &str)], order: Option<OrderConfig>) -> Resource {
        Resource {
            name: name.to_string(),
            about: format!("{name}s"),
            columns: vec!["uuid".to_string()],
            commands: commands.iter().map(|(v, m)| (v.to_string(), m.to_string())).collect::<BTreeMap<_, _>>(),
            order,
            actions: None,
        }
    }

    fn op(path: &str, verb: &str, path_param: Option<&str>) -> ExtractedOperation {
        ExtractedOperation {
            operation_id: format!("{verb}_op"),
            path: path.to_string(),
            http_verb: verb.to_string(),
            path_param: path_param.map(str::to_string),
            query_params: Vec::new(),
            field_enum_name: None,
            request_body_type: Some("SomeRequest".to_string()),
        }
    }

    /// Renders a TokenStream's items in a whitespace-normalized form for
    /// substring assertions -- exact formatting doesn't matter for these
    /// tests, only that the right identifiers/shapes are present.
    fn rendered(ts: &TokenStream) -> String {
        ts.to_string()
    }

    #[test]
    fn path_param_field_none_when_operation_has_no_path_param() {
        let op = op("/api/things/", "get", None);
        assert!(path_param_field(&op, false).is_none());
    }

    #[test]
    fn path_param_field_required_vs_optional() {
        let op = op("/api/things/{uuid}/", "get", Some("uuid"));
        let required = rendered(&path_param_field(&op, false).unwrap());
        assert!(required.contains("pub uuid : String"));
        assert!(!required.contains("Option"));

        let optional = rendered(&path_param_field(&op, true).unwrap());
        assert!(optional.contains("pub uuid : Option < String >"));
    }

    #[test]
    fn assert_no_query_params_passes_when_empty() {
        let resource = resource("thing", &[], None);
        let op = op("/api/things/{uuid}/", "get", Some("uuid"));
        assert_no_query_params(&resource, "get", "things_get", &op).unwrap();
    }

    #[test]
    fn assert_no_query_params_bails_with_the_offending_verb() {
        let resource = resource("thing", &[], None);
        let mut op = op("/api/things/{uuid}/", "get", Some("uuid"));
        op.query_params.push(ExtractedParam { name: "bogus".to_string(), kind: ParamKind::RequiredStr });
        let err = assert_no_query_params(&resource, "get", "things_get", &op).unwrap_err();
        assert!(err.to_string().contains("verb `get`"));
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn emit_get_verb_produces_a_required_uuid_field() {
        let resource = resource("thing", &[("get", "things_get")], None);
        let op = op("/api/things/{uuid}/", "get", Some("uuid"));
        let ident = format_ident!("ThingCommand");
        let emitted = emit_get_verb(&resource, "Thing", &ident, &op, "things_get").unwrap();

        assert!(rendered(&emitted.variant).contains("Get (ThingGetArgs)"));
        assert!(rendered(&emitted.args_struct).contains("pub uuid : String"));
        assert!(rendered(&emitted.arm).contains("print_result"));
        assert!(!emitted.needs_context);
    }

    #[test]
    fn emit_get_verb_rejects_query_params() {
        let resource = resource("thing", &[], None);
        let mut op = op("/api/things/{uuid}/", "get", Some("uuid"));
        op.query_params.push(ExtractedParam { name: "bogus".to_string(), kind: ParamKind::RequiredStr });
        let ident = format_ident!("ThingCommand");
        let err = emit_get_verb(&resource, "Thing", &ident, &op, "things_get").unwrap_err();
        assert!(err.to_string().contains("query parameter"));
    }

    #[test]
    fn emit_body_verb_update_needs_context_create_does_not() {
        let resource = resource("thing", &[], None);
        let ident = format_ident!("ThingCommand");
        let skeletons: HashMap<String, String> = [("SomeRequest".to_string(), "{}".to_string())].into();

        let update_op = op("/api/things/{uuid}/", "put", Some("uuid"));
        let update = emit_body_verb(
            "update", &resource, "Thing", &ident, &update_op, "things_update", &skeletons, &skeletons,
        )
        .unwrap();
        assert!(update.needs_context, "update's required-uuid unwrap needs Context in scope");
        assert!(rendered(&update.args_struct).contains("pub uuid : Option < String >"));

        let create_op = op("/api/things/", "post", None);
        let create = emit_body_verb(
            "create", &resource, "Thing", &ident, &create_op, "things_create", &skeletons, &skeletons,
        )
        .unwrap();
        assert!(!create.needs_context);
        assert!(!rendered(&create.args_struct).contains("uuid"));
    }

    #[test]
    fn emit_body_verb_missing_request_schema_is_a_clear_error() {
        let resource = resource("thing", &[], None);
        let ident = format_ident!("ThingCommand");
        let mut create_op = op("/api/things/", "post", None);
        create_op.request_body_type = None;
        let empty: HashMap<String, String> = HashMap::new();
        let err = emit_body_verb(
            "create", &resource, "Thing", &ident, &create_op, "things_create", &empty, &empty,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no request body schema"));
    }

    #[test]
    fn emit_wait_verb_requires_get_to_have_a_path_param() {
        let resource = resource("thing", &[("get", "things_get")], None);
        let ident = format_ident!("ThingCommand");
        let mut operations = HashMap::new();
        operations.insert("things_get".to_string(), op("/api/things/", "get", None));

        let err = emit_wait_verb(&resource, "Thing", &ident, &operations, "things_get").unwrap_err();
        assert!(err.to_string().contains("needs one to know"));
    }

    #[test]
    fn emit_wait_verb_reuses_gets_path_and_columns() {
        let resource = resource("thing", &[("get", "things_get")], None);
        let ident = format_ident!("ThingCommand");
        let mut operations = HashMap::new();
        operations.insert("things_get".to_string(), op("/api/things/{uuid}/", "get", Some("uuid")));

        let emitted = emit_wait_verb(&resource, "Thing", &ident, &operations, "things_get").unwrap();
        assert!(rendered(&emitted.arm).contains("wait_for"));
        assert!(rendered(&emitted.arm).contains("COLUMNS"));
        assert!(rendered(&emitted.args_struct).contains("pub jmespath : String"));
        assert!(rendered(&emitted.args_struct).contains("default_value_t = 3"));
    }

    #[test]
    fn emit_order_verbs_shares_provision_and_terminate_timeout_default() {
        let resource = resource("thing", &[], Some(OrderConfig { offering_type: None }));
        let ident = format_ident!("ThingCommand");
        let skeletons: HashMap<String, String> = [("thing".to_string(), "{}".to_string())].into();

        let emitted = emit_order_verbs(&resource, "Thing", &ident, &skeletons).unwrap();
        let variant = rendered(&emitted.variant);
        assert!(variant.contains("Provision (ThingProvisionArgs)"));
        assert!(variant.contains("Terminate (ThingTerminateArgs)"));
        assert!(rendered(&emitted.arm).contains("crate :: order :: provision"));
        assert!(rendered(&emitted.arm).contains("crate :: order :: terminate"));
    }

    // -- emit_action_verb -------------------------------------------------

    fn action(name: &str, path: &str, verb: &str, request_body_type: Option<&str>) -> ExtractedAction {
        let mut operation = op(path, verb, Some("uuid"));
        operation.request_body_type = request_body_type.map(str::to_string);
        ExtractedAction { name: name.to_string(), operation }
    }

    #[test]
    fn emit_action_verb_bodyless_action_is_a_bare_call() {
        let resource = resource("thing", &[], None);
        let ident = format_ident!("ThingCommand");
        let empty: HashMap<String, String> = HashMap::new();
        let start = action("start", "/api/things/{uuid}/start/", "post", None);

        let emitted = emit_action_verb(&resource, "Thing", &ident, &start, &empty, &empty).unwrap();
        assert!(rendered(&emitted.variant).contains("Start (ThingStartArgs)"));
        assert!(rendered(&emitted.args_struct).contains("pub uuid : String"));
        assert!(!rendered(&emitted.args_struct).contains("request"));
        assert!(rendered(&emitted.arm).contains("call_one"));
        assert!(!emitted.needs_context);
    }

    #[test]
    fn emit_action_verb_body_having_action_gets_request_args_and_consts() {
        let resource = resource("thing", &[], None);
        let ident = format_ident!("ThingCommand");
        let skeletons: HashMap<String, String> = [("SomeRequest".to_string(), "{}".to_string())].into();
        let change = action("change_flavor", "/api/things/{uuid}/change_flavor/", "post", Some("SomeRequest"));

        let emitted =
            emit_action_verb(&resource, "Thing", &ident, &change, &skeletons, &skeletons).unwrap();
        assert!(rendered(&emitted.variant).contains("ChangeFlavor (ThingChangeFlavorArgs)"));
        assert!(rendered(&emitted.args_struct).contains("pub request : Option < String >"));
        assert!(rendered(&emitted.args_struct).contains("pub generate_skeleton"));
        assert!(rendered(&emitted.consts).contains("CHANGE_FLAVOR_SKELETON"));
        assert!(rendered(&emitted.consts).contains("CHANGE_FLAVOR_REQUEST_SCHEMA"));
        assert!(rendered(&emitted.arm).contains("validate_request_body"));
    }

    #[test]
    fn emit_action_verb_missing_request_schema_is_a_clear_error() {
        let resource = resource("thing", &[], None);
        let ident = format_ident!("ThingCommand");
        let empty: HashMap<String, String> = HashMap::new();
        let change = action("change_flavor", "/api/things/{uuid}/change_flavor/", "post", Some("SomeRequest"));

        let err = emit_action_verb(&resource, "Thing", &ident, &change, &empty, &empty).unwrap_err();
        assert!(err.to_string().contains("no skeleton built"));
    }

    #[test]
    fn generate_resource_module_bails_when_an_action_collides_with_an_existing_verb() {
        let resource = resource("thing", &[("get", "things_get")], None);
        let mut operations = HashMap::new();
        operations.insert("things_get".to_string(), op("/api/things/{uuid}/", "get", Some("uuid")));
        let empty_map: HashMap<String, Vec<String>> = HashMap::new();
        let empty_skeletons: HashMap<String, String> = HashMap::new();
        let order_skeletons: HashMap<String, String> = HashMap::new();
        let mut resource_actions: HashMap<String, Vec<ExtractedAction>> = HashMap::new();
        resource_actions.insert(
            "thing".to_string(),
            vec![action("wait", "/api/things/{uuid}/wait/", "post", None)],
        );

        let err = generate_resource_module(
            &resource,
            &operations,
            &empty_map,
            &empty_skeletons,
            &empty_skeletons,
            &order_skeletons,
            &resource_actions,
        )
        .unwrap_err();
        assert!(err.to_string().contains("collides with an existing verb name"));
    }

    // -- small free functions -------------------------------------------

    #[test]
    fn pascal_case_handles_hyphen_underscore_and_plain_words() {
        assert_eq!(pascal_case("customer"), "Customer");
        assert_eq!(pascal_case("user-invitation"), "UserInvitation");
        assert_eq!(pascal_case("organization_group"), "OrganizationGroup");
        assert_eq!(pascal_case("get"), "Get");
    }

    #[test]
    fn snake_ident_replaces_hyphens() {
        assert_eq!(snake_ident("user-invitation").to_string(), "user_invitation");
        assert_eq!(snake_ident("customer").to_string(), "customer");
    }

    #[test]
    fn field_ident_passes_through_plain_names() {
        assert_eq!(field_ident("uuid").to_string(), "uuid");
        assert_eq!(field_ident("name_exact").to_string(), "name_exact");
    }

    #[test]
    fn field_ident_raw_escapes_keyword_collisions() {
        // Waldur's own `type` filter param is exactly why this exists.
        assert_eq!(field_ident("type").to_string(), "r#type");
    }

    #[test]
    fn filter_kind_expr_maps_scalar_kinds() {
        let str_param = ExtractedParam { name: "name".to_string(), kind: ParamKind::OptionalStr };
        assert!(filter_kind_expr(&str_param).unwrap().unwrap().to_string().contains("Str"));

        let bool_param = ExtractedParam { name: "archived".to_string(), kind: ParamKind::RequiredBool };
        assert!(filter_kind_expr(&bool_param).unwrap().unwrap().to_string().contains("Bool"));

        let int_param = ExtractedParam { name: "age".to_string(), kind: ParamKind::OptionalI64 };
        assert!(filter_kind_expr(&int_param).unwrap().unwrap().to_string().contains("I64"));
    }

    #[test]
    fn filter_kind_expr_skipped_optional_is_silently_dropped() {
        let param = ExtractedParam { name: "weird".to_string(), kind: ParamKind::SkippedOptional };
        assert!(filter_kind_expr(&param).unwrap().is_none());
    }

    #[test]
    fn filter_kind_expr_skipped_required_is_a_hard_error() {
        // Silently dropping a *required* filter would mean --help never
        // shows it, but the server still expects it -- must fail loudly at
        // generation time instead.
        let param = ExtractedParam { name: "weird".to_string(), kind: ParamKind::SkippedRequired };
        let err = filter_kind_expr(&param).unwrap_err();
        assert!(err.to_string().contains("weird"));
    }

    #[test]
    fn http_method_expr_maps_known_verbs() {
        for (verb, expected) in [
            ("get", "GET"),
            ("post", "POST"),
            ("put", "PUT"),
            ("patch", "PATCH"),
            ("delete", "DELETE"),
        ] {
            let op = op("/api/things/", verb, None);
            assert!(rendered(&http_method_expr(&op).unwrap()).contains(expected));
        }
    }

    #[test]
    fn http_method_expr_rejects_unknown_verbs() {
        let op = op("/api/things/", "options", None);
        let err = http_method_expr(&op).unwrap_err();
        assert!(err.to_string().contains("options"));
    }

    #[test]
    fn build_path_expr_with_and_without_a_path_param() {
        let with_param = op("/api/things/{uuid}/", "get", Some("uuid"));
        let rendered_with = rendered(&build_path_expr(&with_param).unwrap());
        assert!(rendered_with.contains("args . uuid"));
        assert!(rendered_with.contains("\"/api/things/\""));
        assert!(rendered_with.contains("\"/\""));

        let without_param = op("/api/things/", "get", None);
        let rendered_without = rendered(&build_path_expr(&without_param).unwrap());
        assert!(rendered_without.contains("\"/api/things/\" . to_string ()"));
    }

    #[test]
    fn build_path_expr_bails_when_path_lacks_its_own_placeholder() {
        // Path param name doesn't match the `{...}` in the literal path --
        // an internal inconsistency in the schema this generator can't
        // silently paper over.
        let mut op = op("/api/things/{uuid}/", "get", Some("id"));
        op.path = "/api/things/{uuid}/".to_string();
        let err = build_path_expr(&op).unwrap_err();
        assert!(err.to_string().contains("doesn't contain its own path param"));
    }

    #[test]
    fn body_path_stmts_update_unwraps_the_optional_uuid_with_context() {
        let op = op("/api/things/{uuid}/", "put", Some("uuid"));
        let rendered = rendered(&body_path_stmts(&op).unwrap());
        assert!(rendered.contains("as_deref () . context"));
        assert!(rendered.contains("requires a <uuid> argument"));
    }

    #[test]
    fn body_path_stmts_create_has_no_uuid_unwrap() {
        let op = op("/api/things/", "post", None);
        let rendered = rendered(&body_path_stmts(&op).unwrap());
        assert!(!rendered.contains("context"));
        assert!(rendered.contains("\"/api/things/\" . to_string ()"));
    }
}
