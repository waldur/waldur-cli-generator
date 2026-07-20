use crate::extract::{ExtractedMethod, ParamKind};
use crate::manifest::{Manifest, Resource, KNOWN_VERBS};
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

struct FieldPlan {
    /// Empty for params that don't get a struct field (SkippedOptional).
    field_def: TokenStream,
    call_expr: TokenStream,
    /// Set for the first RequiredStr param (the uuid path param), used to
    /// build a friendly delete confirmation message.
    is_path_uuid: bool,
}

fn plan_param(param: &crate::extract::ExtractedParam) -> Result<FieldPlan> {
    let ident = format_ident!("{}", param.name);
    Ok(match &param.kind {
        ParamKind::RequiredStr => FieldPlan {
            field_def: quote! { pub #ident: String, },
            call_expr: quote! { args.#ident.as_str() },
            is_path_uuid: true,
        },
        ParamKind::OptionalStr => FieldPlan {
            field_def: quote! { #[arg(long)] pub #ident: Option<String>, },
            call_expr: quote! { args.#ident.as_deref() },
            is_path_uuid: false,
        },
        ParamKind::RequiredBool => FieldPlan {
            field_def: quote! { #[arg(long)] pub #ident: bool, },
            call_expr: quote! { args.#ident },
            is_path_uuid: false,
        },
        ParamKind::OptionalBool => FieldPlan {
            field_def: quote! { #[arg(long)] pub #ident: Option<bool>, },
            call_expr: quote! { args.#ident },
            is_path_uuid: false,
        },
        ParamKind::RequiredI64 => FieldPlan {
            field_def: quote! { pub #ident: i64, },
            call_expr: quote! { args.#ident },
            is_path_uuid: false,
        },
        ParamKind::OptionalI64 => FieldPlan {
            field_def: quote! { #[arg(long)] pub #ident: Option<i64>, },
            call_expr: quote! { args.#ident },
            is_path_uuid: false,
        },
        ParamKind::JsonBody(type_name) => {
            let ty: syn::Type = syn::parse_str(&format!("waldur_client::{type_name}"))
                .with_context(|| format!("invalid generated type name `{type_name}`"))?;
            FieldPlan {
                field_def: quote! { #[arg(long)] pub #ident: String, },
                call_expr: quote! {
                    serde_json::from_str::<#ty>(&args.#ident)
                        .with_context(|| format!("--{} is not valid JSON for the expected request body", stringify!(#ident)))?
                },
                is_path_uuid: false,
            }
        }
        ParamKind::OptionalJsonBody(type_name) => {
            let ty: syn::Type = syn::parse_str(&format!("waldur_client::{type_name}"))
                .with_context(|| format!("invalid generated type name `{type_name}`"))?;
            FieldPlan {
                field_def: quote! { #[arg(long)] pub #ident: Option<String>, },
                call_expr: quote! {
                    args.#ident.as_deref()
                        .map(|s| serde_json::from_str::<#ty>(s))
                        .transpose()
                        .with_context(|| format!("--{} is not valid JSON for the expected request body", stringify!(#ident)))?
                },
                is_path_uuid: false,
            }
        }
        ParamKind::SkippedOptional => FieldPlan {
            field_def: quote! {},
            call_expr: quote! { None },
            is_path_uuid: false,
        },
        ParamKind::SkippedRequired => {
            bail!(
                "parameter `{}` has a type this generator can't map to a CLI flag \
                 (not a string/bool/i64/JSON-body shape) -- either extend classify_type() \
                 in extract.rs, or drop this method from commands.toml",
                param.name
            );
        }
    })
}

/// One resource's generated file: Args structs + Command enum + run().
fn generate_resource_module(resource: &Resource, methods: &HashMap<String, ExtractedMethod>) -> Result<TokenStream> {
    let resource_pascal = pascal_case(&resource.name);
    let resource_enum_ident = format_ident!("{}Command", resource_pascal);
    let columns = &resource.columns;

    let mut verb_variants = Vec::new();
    let mut verb_arms = Vec::new();
    let mut uses_json_body = false;

    for verb in KNOWN_VERBS {
        let Some(method_name) = resource.commands.get(*verb) else {
            continue;
        };
        let method = methods.get(method_name).with_context(|| {
            format!(
                "internal error: method `{method_name}` (resource `{}`, verb `{verb}`) \
                 was not extracted",
                resource.name
            )
        })?;
        let method_ident = format_ident!("{}", method.name);

        let mut field_defs = Vec::new();
        let mut call_exprs = Vec::new();
        let mut uuid_field: Option<proc_macro2::Ident> = None;
        for param in &method.params {
            if matches!(param.kind, ParamKind::JsonBody(_) | ParamKind::OptionalJsonBody(_)) {
                uses_json_body = true;
            }
            let plan = plan_param(param)
                .with_context(|| format!("in resource `{}`, verb `{verb}` ({method_name})", resource.name))?;
            if !plan.field_def.is_empty() {
                field_defs.push(plan.field_def);
            }
            call_exprs.push(plan.call_expr);
            if plan.is_path_uuid && uuid_field.is_none() {
                uuid_field = Some(format_ident!("{}", param.name));
            }
        }

        let verb_pascal = pascal_case(verb);
        let args_ident = format_ident!("{}{}Args", resource_pascal, verb_pascal);
        let variant_ident = format_ident!("{}", verb_pascal);

        let about = format!("{} {}", pascal_case(verb), resource.about.to_lowercase());
        verb_variants.push(quote! {
            #[doc = #about]
            #variant_ident(#args_ident),
        });

        let call = quote! { client.#method_ident(#(#call_exprs),*).await? };

        let output_stmt = if *verb == "delete" {
            let uuid_ident = uuid_field.unwrap_or_else(|| format_ident!("uuid"));
            quote! {
                let _ = #call;
                match format {
                    crate::output::OutputFormat::Json => {
                        println!("{}", serde_json::json!({"deleted": true, "uuid": args.#uuid_ident}));
                    }
                    crate::output::OutputFormat::Table => {
                        println!("Deleted {}", args.#uuid_ident);
                    }
                }
            }
        } else {
            quote! {
                let result = #call;
                crate::output::print_result(&result, COLUMNS, format)?;
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
                pub struct #args_ident {
                    #(#field_defs)*
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
    let context_import = if uses_json_body {
        quote! { use anyhow::Context; }
    } else {
        quote! {}
    };

    Ok(quote! {
        //! Generated by waldur-cli-generator from `commands.toml`. Do not edit by hand;
        //! see that repo's README for how to regenerate.
        #![allow(clippy::too_many_arguments)]

        #context_import
        use waldur_client::HttpClient;

        const COLUMNS: &[&str; #columns_len] = &[#(#columns),*];

        #[doc = #about]
        #[derive(clap::Subcommand, Debug)]
        pub enum #resource_enum_ident {
            #(#verb_variants)*
        }

        #(#args_structs)*

        pub async fn run(
            client: &HttpClient,
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

pub fn generate_all(manifest: &Manifest, methods: &HashMap<String, ExtractedMethod>) -> Result<GeneratedOutput> {
    let mut resources = Vec::new();
    let mut group_mod_decls: HashMap<String, String> = HashMap::new();

    for group in &manifest.group {
        let mut resource_mod_decls = Vec::new();
        for resource in &group.resource {
            let tokens = generate_resource_module(resource, methods)
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
                    crate::commands::#group_mod::#resource_mod::run(client, cmd, format).await
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
