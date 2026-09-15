//! Derive macros for `soroban-migrate`.
//!
//! Three derives, each removing a class of boilerplate that is easy to get subtly
//! wrong and impossible to notice when it is:
//!
//! * [`macro@StorageSchema`] — declares a struct as a version of a storage shape and
//!   emits the metadata a deployed contract reports about itself.
//! * [`macro@Keyspace`] — implements `soroban_migrate::key::Keyspace` for a contract's
//!   `DataKey` enum, which is what keeps the framework's bookkeeping keys from
//!   colliding with the contract's own.
//! * [`macro@Migration`] — implements `soroban_migrate::migration::Migration` for an
//!   additive version change.
//!
//! # What the `Migration` derive covers, and what it deliberately refuses
//!
//! It covers additions and optionality changes: the cases whose idempotency can be
//! *proved* rather than argued. A new optional field is filled only when it is empty,
//! so a second visit is a no-op by construction.
//!
//! It refuses renames and type changes, with an error that says where to go instead.
//! Those are cases where the migration has to consume an old key, and being
//! re-runnable therefore requires reading the old shape through a *tolerant shadow
//! struct* — a struct declaring only the consumed keys, each as `Option<..>`, so that
//! an already-consumed key decodes as `None` instead of trapping. A derive macro sees
//! one struct and cannot synthesize the other one. `soroban-migrate generate` can,
//! because it has both schemas, and it is what the error message tells you to run.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Expr, LitInt, LitStr, Path, Type};

mod attrs;

use attrs::{parse_attrs, MigrationAttrs, StorageSchemaAttrs};

/// Declares a struct as one version of a storage shape.
///
/// ```ignore
/// /// Balances held for a single account.
/// #[derive(StorageSchema)]
/// #[storage_schema(version = 2)]
/// #[contracttype]
/// pub struct Account {
///     pub owner: Address,
///     pub amount: i128,
///     pub frozen: Option<bool>,
/// }
/// ```
///
/// # Arguments
///
/// * `version = N` — required. The schema version this shape is.
/// * `name = "..."` — optional. The schema's identity across versions, defaulting to
///   the struct's name. Set it when the struct is renamed between versions without
///   the shape changing, which is otherwise indistinguishable from a schema
///   identity change.
/// * `note = "..."` — optional. Copied into `JSON`. Defaults to the struct's doc
///   comment.
///
/// # Generated items
///
/// An implementation of `soroban_migrate::schema::StorageSchema`, providing
/// `VERSION`, `NAME`, `JSON`, and `fields()`. `JSON` is byte-for-byte what
/// `soroban-migrate schema export` writes for the same struct, so a build can be
/// diffed against the repository by comparing strings.
#[proc_macro_derive(StorageSchema, attributes(storage_schema))]
pub fn derive_storage_schema(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match storage_schema::expand(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Implements the framework's key space for a contract's `DataKey` enum.
///
/// ```ignore
/// #[derive(Keyspace)]
/// #[contracttype]
/// pub enum DataKey {
///     /// Reserved for the migration framework.
///     #[migration]
///     Migration(MigrationKey),
///     Balance(Address),
/// }
/// ```
///
/// The variant marked `#[migration]` must be a newtype wrapping
/// `soroban_migrate::key::MigrationKey`. If no variant is marked, one named
/// `Migration` is used, so the marker is only needed when the variant is named
/// something else. Marking more than one variant is an error, because a key space
/// with two framework namespaces would make the framework's own keys ambiguous.
#[proc_macro_derive(Keyspace, attributes(migration))]
pub fn derive_keyspace(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match expand_keyspace(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Implements a migration for an additive version change.
///
/// ```ignore
/// #[derive(Migration)]
/// #[migration(
///     from = 1,
///     to = 2,
///     entry = AccountV2,
///     init(frozen = false),   // new optional field, filled when empty
///     name = "add frozen flag",
///     reversible,             // also generate `down`
/// )]
/// pub struct AddFrozen;
/// ```
///
/// # Arguments
///
/// * `from = N`, `to = N` — required, and `to` must be `from + 1`.
/// * `entry = Type` — required. The struct holding the *new* shape. It must be
///   readable from a pre-migration entry, which for additions means the new fields
///   are `Option<..>`.
/// * `keys = Path` — optional, defaults to `crate::DataKey`.
/// * `init(field = expr)` — repeatable. Each named field is filled, when empty, with
///   `expr`. May appear more than once.
/// * `drop(field)` — repeatable. Documentation only: declares that a field present in
///   the old shape is intentionally discarded. The generated code needs no change for
///   it, but recording it keeps the derive's view of the change complete.
/// * `reversible` — generate a `down` that clears every initialised field, so the
///   framework's rollback path can be wired up without hand-writing one.
/// * `name = "..."` — a human-readable name for CLI output.
///
/// `convert(..)` and `rename(..)` are rejected with a pointer to
/// `soroban-migrate generate`; see this module's documentation for why.
#[proc_macro_derive(Migration, attributes(migration))]
pub fn derive_migration(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match expand_migration(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// The contract's schema module for tests and for `soroban-migrate check`.
mod storage_schema {
    use super::*;

    pub fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
        let attrs = parse_attrs::<StorageSchemaAttrs>(&input)?;

        let Data::Struct(data) = &input.data else {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`StorageSchema` must be derived on a struct. A storage shape is a struct: enums \
                 encode as a tagged vector, which has no field names to diff or migrate.",
            ));
        };
        let syn::Fields::Named(fields) = &data.fields else {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`StorageSchema` requires named fields. A tuple struct has no stable key names, so \
                 a field-level diff is impossible.",
            ));
        };

        let ident = &input.ident;
        let version = attrs.version;
        let name = attrs.name.unwrap_or_else(|| ident.to_string());

        // Build the model both the string table and the JSON come from, so the two
        // can never disagree.
        let mut model_fields = Vec::new();
        let mut entries = Vec::new();
        for field in &fields.named {
            let Some(field_ident) = field.ident.as_ref() else {
                continue;
            };
            let declared = soroban_migrate_schema::parse::render_field_type(&field.ty);
            entries.push(format!("{field_ident}:{declared}"));
            model_fields.push(soroban_migrate_schema::model::Field::new(
                field_ident.to_string(),
                declared,
            ));
        }

        let mut schema =
            soroban_migrate_schema::model::Schema::new(version, name.clone(), model_fields);
        schema.note = attrs.note.or_else(|| doc_note(&input.attrs));
        let json = schema.to_json();

        let version_lit = LitInt::new(&version.to_string(), proc_macro2::Span::call_site());
        let name_lit = LitStr::new(&name, proc_macro2::Span::call_site());
        let json_lit = LitStr::new(&json, proc_macro2::Span::call_site());

        Ok(quote! {
            impl soroban_migrate::schema::StorageSchema for #ident {
                const VERSION: u32 = #version_lit;
                const NAME: &'static str = #name_lit;
                const JSON: &'static str = #json_lit;

                fn fields() -> &'static [&'static str] {
                    &[#(#entries),*]
                }
            }
        })
    }

    /// Reads `///` comments into a note, matching what the source parser does.
    fn doc_note(attrs: &[syn::Attribute]) -> Option<String> {
        let mut lines = Vec::new();
        for attr in attrs {
            if !attr.path().is_ident("doc") {
                continue;
            }
            if let syn::Meta::NameValue(nv) = &attr.meta {
                if let Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                {
                    lines.push(s.value().trim().to_string());
                }
            }
        }
        if lines.is_empty() {
            None
        } else {
            Some(lines.join(" ").trim().to_string())
        }
    }
}

fn expand_keyspace(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`Keyspace` must be derived on an enum: it names a variant to wrap \
             `MigrationKey` in, and a struct has no variants.",
        ));
    };
    let ident = &input.ident;

    let mut marked: Vec<syn::Ident> = Vec::new();
    for variant in &data.variants {
        let is_marked = variant.attrs.iter().any(|a| a.path().is_ident("migration"));
        if is_marked {
            marked.push(variant.ident.clone());
        }
    }

    let variant =
        match marked.len() {
            1 => marked.remove(0),
            0 => data
                .variants
                .iter()
                .find(|v| v.ident == "Migration")
                .map(|v| v.ident.clone())
                .ok_or_else(|| {
                    syn::Error::new_spanned(
                        ident,
                        "no variant marked `#[migration]`, and no variant named `Migration`. Add \
                     `#[migration]` to the variant that wraps `MigrationKey`, or name that variant \
                     `Migration`.",
                    )
                })?,
            _ => return Err(syn::Error::new_spanned(
                ident,
                "more than one variant is marked `#[migration]`. The framework addresses storage \
                 through exactly one namespace; two would make its own keys ambiguous.",
            )),
        };

    Ok(quote! {
        impl soroban_migrate::key::Keyspace for #ident {
            fn wrap(
                env: &soroban_sdk::Env,
                key: soroban_migrate::key::MigrationKey,
            ) -> soroban_sdk::Val {
                soroban_sdk::IntoVal::into_val(&#ident::#variant(key), env)
            }
        }
    })
}

fn expand_migration(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let attrs = parse_attrs::<MigrationAttrs>(&input)?;
    let ident = &input.ident;

    // Checked before anything is consumed, so the author sees every unsupported
    // directive at once instead of discovering them one compile at a time.
    if !attrs.denied.is_empty() {
        return Err(syn::Error::new_spanned(
            ident,
            format!(
                "`{}` cannot be handled by the derive. A {0} consumes an old key, so being \
                 re-runnable requires reading the old shape through a tolerant shadow struct — a \
                 struct declaring only the consumed keys, each as `Option<..>`, so an already-\
                 consumed key decodes as `None` instead of trapping. The derive sees one struct \
                 and cannot synthesize the other. Run `soroban-migrate generate`, which has both \
                 schemas and writes that struct for you.",
                attrs.denied.join("`, `")
            ),
        ));
    }

    let from = attrs.from;
    let to = attrs.to;
    if to != from + 1 {
        return Err(syn::Error::new_spanned(
            ident,
            format!(
                "`from = {from}` and `to = {to}` skip versions. Migrations advance one version at \
                 a time: a rollback walks the same index in reverse, and a jump makes its target \
                 ambiguous. Declare the intermediate change as its own migration."
            ),
        ));
    }

    let entry: Type = attrs.entry.ok_or_else(|| {
        syn::Error::new_spanned(
            ident,
            "`entry = Type` is required: it names the struct holding the new shape",
        )
    })?;
    let keys: Path = attrs
        .keys
        .unwrap_or_else(|| syn::parse_quote!(crate::DataKey));
    let name_lit = LitStr::new(
        &attrs.name.clone().unwrap_or_else(|| ident.to_string()),
        ident.span(),
    );
    let from_lit = LitInt::new(&from.to_string(), ident.span());
    let to_lit = LitInt::new(&to.to_string(), ident.span());

    let mut up_body = Vec::new();
    let mut down_body = Vec::new();
    let mut doc_lines = Vec::new();
    for (field, expr) in &attrs.init {
        up_body.push(quote! {
            if entry.#field.is_none() {
                entry.#field = Some(#expr);
                changed = true;
            }
        });
        down_body.push(quote! {
            if entry.#field.is_some() {
                entry.#field = None;
                changed = true;
            }
        });
        doc_lines.push(format!("`{field}` is filled when empty"));
    }
    if attrs.init.is_empty() && attrs.dropped.is_empty() {
        return Err(syn::Error::new_spanned(
            ident,
            "no change declared. A migration with nothing to do is a version bump that will \
             silently mark entries current; either declare `init(field = expr)`, or delete the \
             migration and change the schema version without one.",
        ));
    }

    let drop_comment = if attrs.dropped.is_empty() {
        quote!()
    } else {
        let names = attrs.dropped.join("`, `");
        let text = format!(
            "The plan declares `{names}` as intentionally discarded. No code is needed: \
             `#[contracttype]` drops map keys the new struct has no field for."
        );
        quote! { #[doc = #text] }
    };

    let down_impl = if attrs.reversible {
        quote! {
            /// Clears every field this migration filled, returning entries to the previous
            /// shape.
            ///
            /// Generated because `reversible` was declared. This is a real inverse for the
            /// additive case: the old shape has no field at all, so clearing it is exactly
            /// what "before" means. It is lossy in the sense that any value the field held
            /// is discarded, which is what the extra information in the new shape *was*.
            fn down(
                env: &soroban_sdk::Env,
                key: &soroban_sdk::Val,
            ) -> ::core::result::Result<
                soroban_migrate::migration::EntryOutcome,
                soroban_migrate::MigrationError,
            > {
                let storage = env.storage().persistent();
                if !storage.has::<soroban_sdk::Val>(key) {
                    return ::core::result::Result::Ok(
                        soroban_migrate::migration::EntryOutcome::Skipped,
                    );
                }
                let mut entry: #entry = match storage.get::<soroban_sdk::Val, #entry>(key) {
                    ::core::option::Option::Some(e) => e,
                    ::core::option::Option::None => {
                        return ::core::result::Result::Ok(
                            soroban_migrate::migration::EntryOutcome::Skipped,
                        )
                    }
                };
                let mut changed = false;
                #(#down_body)*
                if !changed {
                    return ::core::result::Result::Ok(
                        soroban_migrate::migration::EntryOutcome::AlreadyCurrent,
                    );
                }
                storage.set::<soroban_sdk::Val, #entry>(key, &entry);
                ::core::result::Result::Ok(soroban_migrate::migration::EntryOutcome::Migrated)
            }

            fn supports_down() -> bool {
                true
            }
        }
    } else {
        quote!()
    };

    let doc = format!(
        "Moves entries from schema v{from} to v{to}. {}",
        if doc_lines.is_empty() {
            "Declared only; see `drop` in the attribute.".to_string()
        } else {
            format!("{}.", doc_lines.join(", "))
        }
    );

    Ok(quote! {
        #[doc = #doc]
        #drop_comment
        impl soroban_migrate::migration::Migration for #ident {
            type Keys = #keys;

            const FROM: u32 = #from_lit;
            const TO: u32 = #to_lit;

            /// Fills each declared field when it is empty.
            ///
            /// The guard is the whole reason this is safe to re-run: a field that already has
            /// a value is left alone, so visiting an entry twice cannot overwrite a value a
            /// later migration or the application put there.
            fn up(
                env: &soroban_sdk::Env,
                key: &soroban_sdk::Val,
            ) -> ::core::result::Result<
                soroban_migrate::migration::EntryOutcome,
                soroban_migrate::MigrationError,
            > {
                let storage = env.storage().persistent();
                if !storage.has::<soroban_sdk::Val>(key) {
                    return ::core::result::Result::Ok(
                        soroban_migrate::migration::EntryOutcome::Skipped,
                    );
                }
                let mut entry: #entry = match storage.get::<soroban_sdk::Val, #entry>(key) {
                    ::core::option::Option::Some(e) => e,
                    ::core::option::Option::None => {
                        return ::core::result::Result::Ok(
                            soroban_migrate::migration::EntryOutcome::Skipped,
                        )
                    }
                };
                let mut changed = false;
                #(#up_body)*
                if !changed {
                    return ::core::result::Result::Ok(
                        soroban_migrate::migration::EntryOutcome::AlreadyCurrent,
                    );
                }
                storage.set::<soroban_sdk::Val, #entry>(key, &entry);
                ::core::result::Result::Ok(soroban_migrate::migration::EntryOutcome::Migrated)
            }

            #down_impl

            fn name() -> &'static str {
                #name_lit
            }
        }
    })
}
