use crate::manifest::{Manifest, Resource, KNOWN_VERBS};
use crate::schema::{ExtractedOperation, ParamKind};
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
/// its path parameter, if it has one -- mirrors rs-client's own generated
/// style rather than substituting at runtime.
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

/// One resource's generated file: Args structs + Command enum + run().
fn generate_resource_module(
    resource: &Resource,
    operations: &HashMap<String, ExtractedOperation>,
    field_enum_values: &HashMap<String, Vec<String>>,
    request_skeletons: &HashMap<String, String>,
    order_skeletons: &HashMap<String, String>,
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

    // `--dry-run` is honored only by mutating verbs; a read-only resource
    // takes the flag as `_dry_run` so there's no unused-argument warning.
    let has_mutating_verb = ["create", "update", "delete"]
        .iter()
        .any(|v| resource.commands.contains_key(*v))
        || resource.order.is_some();
    let dry_run_param = if has_mutating_verb {
        quote! { dry_run }
    } else {
        quote! { _dry_run }
    };

    let mut verb_variants = Vec::new();
    let mut verb_arms = Vec::new();
    let mut skeleton_consts = Vec::new();
    let mut filter_spec_consts = Vec::new();
    let mut uses_context = false;

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

        if *verb != "list" && !op.query_params.is_empty() {
            bail!(
                "resource `{}`, verb `{verb}` ({method_name}) has query parameter(s) \
                 {:?} -- this generator only supports query filters on `list`, extend \
                 codegen.rs if a non-list verb genuinely needs one",
                resource.name,
                op.query_params.iter().map(|p| &p.name).collect::<Vec<_>>()
            );
        }

        let mut field_defs = Vec::new();
        // Struct-level attribute (e.g. the request-body arg group on
        // create/update); empty for verbs that don't need one.
        let mut struct_attr = quote! {};

        let has_body = *verb == "create" || *verb == "update";
        // A body verb with a path param (i.e. `update`) makes its uuid an
        // optional positional so `--generate-skeleton` can run without it
        // (mirrors AWS's skeleton bypassing required args); presence is then
        // enforced at runtime for an actual update.
        let uuid_optional = has_body && op.path_param.is_some();

        if let Some(path_param) = &op.path_param {
            let ident = field_ident(path_param);
            if uuid_optional {
                field_defs.push(quote! { pub #ident: Option<String>, });
            } else {
                field_defs.push(quote! { pub #ident: String, });
            }
        }
        if *verb == "list" {
            // Every real query filter goes through one generic --filter
            // KEY=VALUE flag (AWS --filters / kubectl --field-selector
            // style) instead of a dedicated flag per field -- some resources
            // have 20+ filterable fields, and a single uniform pattern is
            // both a smaller --help and one thing to learn across every
            // resource (mirrors --request's own move away from a flag per
            // request-body field). FILTER_SPEC (built from the same
            // op.query_params used before) is what makes this still
            // client-side-validated rather than a blind passthrough: a
            // bad key or wrongly-typed value is rejected locally.
            let spec_entries: Vec<TokenStream> = op
                .query_params
                .iter()
                .map(|param| {
                    let name = &param.name;
                    filter_kind_expr(param)
                        .with_context(|| format!("in resource `{}`, verb `{verb}` ({method_name})", resource.name))
                        .map(|kind| kind.map(|k| quote! { (#name, #k) }))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            filter_spec_consts.push(quote! {
                const FILTER_SPEC: &[(&str, crate::filter::FilterKind)] = &[#(#spec_entries),*];
            });
            field_defs.push(quote! {
                /// Filter results server-side, KEY=VALUE (repeatable). See
                /// --help's error on an unknown key for the valid keys.
                #[arg(long = "filter", value_name = "KEY=VALUE")]
                pub filter: Vec<String>,
            });
            // Named jmespath, not query: several resources have a real
            // `query` filter field of their own (e.g. customers' full-text
            // search) -- `--filter query=...` reaches that. A `--query` flag
            // here would silently shadow it (a bare word is itself valid
            // JMESPath, a field projection, so `--query foo` wouldn't even
            // error, just silently do something other than what a user
            // migrating from `--query` on other CLIs would expect).
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
        }
        if *verb == "create" || *verb == "update" {
            uses_context = true;
            // AWS-style body input: inline JSON, a JSON/YAML file, or print a
            // fillable template -- exactly one, enforced by a required arg
            // group. Discoverable in --help without a flag per schema field.
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
            struct_attr = quote! {
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
            skeleton_consts.push(quote! { const #const_ident: &str = #skeleton; });
        }

        // Client-side only -- not part of the schema (list has no "limit"
        // concept), added here so `list` can bound a huge auto-paginated
        // fetch instead of always fetching everything.
        if *verb == "list" {
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
            // Waldur's RestrictedSerializerMixin silently ignores unknown
            // field names rather than rejecting them (confirmed against
            // mastermind source) -- an all-invalid --fields list falls back
            // to returning the complete object with no error at all. Validate
            // against the resource's own FieldEnum values client-side instead
            // of letting that happen silently, when we know what they are.
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
        }

        let verb_pascal = pascal_case(verb);
        let args_ident = format_ident!("{}{}Args", resource_pascal, verb_pascal);
        let variant_ident = format_ident!("{}", verb_pascal);

        let about = format!("{} {}", pascal_case(verb), resource.about.to_lowercase());
        verb_variants.push(quote! {
            #[doc = #about]
            #variant_ident(#args_ident),
        });

        let path_expr = build_path_expr(op)?;

        let output_stmt = if *verb == "list" {
            let path = &op.path;
            // Apply the ambient --project scope, unless the user already
            // filtered by project_uuid explicitly (theirs wins). Only for
            // resources whose list actually supports the filter.
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
            quote! {
                let mut query_params: Vec<(String, String)> = crate::filter::parse_filters(&args.filter, FILTER_SPEC)?;
                #project_inject
                // Table always narrows the server fetch to its own display
                // columns (there's never a reason to fetch more than what
                // it shows); json/toon/tsv fetch the complete object by
                // default, but --fields narrows any format that asks for it.
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
                // complete result set first -- lower memory, faster first
                // output. Only when there's no --jmespath: a JMESPath
                // expression can reshape/aggregate across the whole array
                // (sort, slice, count, ...), so it still needs the complete
                // result fetched first, same as json/toon/table/tsv.
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
                    // ignore them, showing the complete fetched object
                    // regardless) -- when --fields narrowed what was actually
                    // fetched, the display columns have to follow the same
                    // override, or table/tsv would show a column for every
                    // field COLUMNS expects but --fields didn't ask for,
                    // which the server response then doesn't have at all
                    // (rendering as blank).
                    let display_columns: Vec<&str> = match &args.fields {
                        Some(fields) => fields.iter().map(String::as_str).collect(),
                        None => COLUMNS.to_vec(),
                    };
                    // --query reshapes the already-fetched result client-side
                    // (AWS CLI's --query) -- distinct from --filter, which
                    // narrows what's fetched in the first place.
                    let result: serde_json::Value = serde_json::Value::Array(result);
                    let result = match &args.jmespath {
                        Some(expr) => crate::query::apply(result, expr)?,
                        None => result,
                    };
                    crate::output::print_result(&result, &display_columns, format)?;
                }
            }
        } else if *verb == "get" {
            let method_expr = http_method_expr(op)?;
            quote! {
                let path = #path_expr;
                let result = crate::http::call_one(base_url, token, #method_expr, &path, None).await?;
                crate::output::print_result(&result, COLUMNS, format)?;
            }
        } else if *verb == "create" || *verb == "update" {
            let request_ty_name = op.request_body_type.as_deref().with_context(|| {
                format!(
                    "resource `{}`, verb `{verb}` ({method_name}): couldn't find this \
                     operation's request body schema -- needed to validate --request",
                    resource.name
                )
            })?;
            let request_ty: syn::Type = syn::parse_str(&format!("waldur_client::{request_ty_name}"))
                .with_context(|| format!("invalid generated type name `{request_ty_name}`"))?;
            let method_expr = http_method_expr(op)?;
            let method_str = op.http_verb.to_uppercase();
            let const_ident = format_ident!("{}_SKELETON", verb.to_uppercase());
            let path_stmts = body_path_stmts(op)?;
            quote! {
                if let Some(fmt) = args.generate_skeleton {
                    crate::request::print_skeleton(#const_ident, fmt)?;
                    return Ok(());
                }
                let body = crate::request::load_body(args.request.as_deref(), args.request_file.as_deref())?;
                // Validate the body even under --dry-run, so a dry run still
                // catches a malformed request rather than only previewing it.
                serde_json::from_str::<#request_ty>(&body)
                    .with_context(|| "the request body is not valid JSON for this resource's request schema".to_string())?;
                #path_stmts
                if dry_run {
                    return crate::output::print_dry_run(#method_str, &path, Some(&body), format);
                }
                let result = crate::http::call_one(base_url, token, #method_expr, &path, Some(&body)).await?;
                crate::output::print_result(&result, COLUMNS, format)?;
            }
        } else {
            // delete
            let method_expr = http_method_expr(op)?;
            let method_str = op.http_verb.to_uppercase();
            let uuid_ident = op
                .path_param
                .as_ref()
                .map(|p| field_ident(p))
                .unwrap_or_else(|| format_ident!("uuid"));
            quote! {
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

        verb_arms.push(quote! {
            #resource_enum_ident::#variant_ident(args) => {
                #output_stmt
            }
        });

        // Args struct emitted separately below via a side list; collect here.
        ARGS_STRUCTS.with(|cell| {
            cell.borrow_mut().push(quote! {
                #[derive(clap::Args, Debug)]
                #struct_attr
                pub struct #args_ident {
                    #(#field_defs)*
                }
            });
        });
    }

    // Marketplace-order provisioning: resources with an `[order]` config get
    // `provision` (submit a marketplace order + poll to completion) and
    // `terminate` (terminate the marketplace resource + poll) subcommands,
    // for the async order flow that has no direct REST create/delete.
    if resource.order.is_some() {
        let skeleton = order_skeletons.get(&resource.name).with_context(|| {
            format!("internal error: no order skeleton built for resource `{}`", resource.name)
        })?;
        skeleton_consts.push(quote! { const PROVISION_SKELETON: &str = #skeleton; });

        let provision_args = format_ident!("{}ProvisionArgs", resource_pascal);
        let terminate_args = format_ident!("{}TerminateArgs", resource_pascal);
        let provision_about = format!("Provision {} via a marketplace order", resource.about.to_lowercase());
        let terminate_about = format!("Terminate {} via a marketplace order", resource.about.to_lowercase());
        let body_group = format!("{}_provision_body", resource.name.replace('-', "_"));

        verb_variants.push(quote! {
            #[doc = #provision_about]
            Provision(#provision_args),
            #[doc = #terminate_about]
            Terminate(#terminate_args),
        });

        verb_arms.push(quote! {
            #resource_enum_ident::Provision(args) => {
                if let Some(fmt) = args.generate_skeleton {
                    crate::request::print_skeleton(PROVISION_SKELETON, fmt)?;
                    return Ok(());
                }
                let body = crate::request::load_body(args.request.as_deref(), args.request_file.as_deref())?;
                crate::order::provision(base_url, token, &body, project, dry_run, !args.no_wait, args.timeout, format).await?;
            }
            #resource_enum_ident::Terminate(args) => {
                crate::order::terminate(base_url, token, &args.uuid, args.request.as_deref(), dry_run, !args.no_wait, args.timeout, format).await?;
            }
        });

        ARGS_STRUCTS.with(|cell| {
            cell.borrow_mut().push(quote! {
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
                }
            });
        });
    }

    let args_structs = ARGS_STRUCTS.with(|cell| {
        let v = cell.borrow().clone();
        cell.borrow_mut().clear();
        v
    });

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

        #(#filter_spec_consts)*

        #(#skeleton_consts)*

        #[doc = #about]
        #[derive(clap::Subcommand, Debug)]
        pub enum #resource_enum_ident {
            #(#verb_variants)*
        }

        #(#args_structs)*

        pub async fn run(
            _client: &waldur_client::HttpClient,
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

pub fn generate_all(
    manifest: &Manifest,
    operations: &HashMap<String, ExtractedOperation>,
    field_enum_values: &HashMap<String, Vec<String>>,
    request_skeletons: &HashMap<String, String>,
    order_skeletons: &HashMap<String, String>,
) -> Result<GeneratedOutput> {
    let mut resources = Vec::new();
    let mut group_mod_decls: HashMap<String, String> = HashMap::new();

    for group in &manifest.group {
        let mut resource_mod_decls = Vec::new();
        for resource in &group.resource {
            let tokens = generate_resource_module(resource, operations, field_enum_values, request_skeletons, order_skeletons)
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
                    crate::commands::#group_mod::#resource_mod::run(client, base_url, token, project, dry_run, cmd, format).await
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
            client: &waldur_client::HttpClient,
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
