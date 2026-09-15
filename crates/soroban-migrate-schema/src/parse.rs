//! Extracting schema declarations from Rust source.
//!
//! Schemas are declared with an attribute on the struct that carries the shape:
//!
//! ```ignore
//! /// Balances held for a single account.
//! #[storage_schema(version = 2)]
//! #[contracttype]
//! pub struct Account {
//!     pub owner: Address,
//!     pub amount: i128,
//!     pub frozen: Option<bool>,
//! }
//! ```
//!
//! # Why the parser is in the library rather than only in the proc macro
//!
//! The CLI needs to read schemas too, and it needs to read them from *source* rather
//! than from compiled output, so that `soroban-migrate schema export` works on a
//! contract that has never been built for Wasm. That means the same understanding of
//! the attribute has to exist in both places. Rather than keep two implementations in
//! sync, the proc macro and the CLI share this one: the macro re-reads the attribute
//! with `syn` in order to reject unknown arguments at compile time, and the CLI uses
//! [`parse_source`] to build the JSON snapshot it diffs.

use syn::{Attribute, Fields, Item, Lit, Type};

use crate::error::SchemaError;
use crate::model::{Field, Schema};

/// Parses every `#[storage_schema]` declaration in one Rust source file.
///
/// Returns them in declaration order. A file with none is not an error: contracts
/// keep schemas in their own module, and a directory walk will hand this function
/// plenty of files that have nothing to do with migrations.
///
/// # Errors
///
/// [`SchemaError::Parse`] for malformed Rust, and for a `#[storage_schema]`
/// attribute that is missing `version`, has an unparseable argument, or is applied to
/// something other than a struct with named fields.
pub fn parse_source(file: &str, source: &str) -> Result<Vec<Schema>, SchemaError> {
    let ast = syn::parse_file(source).map_err(|e| SchemaError::Parse {
        file: file.to_string(),
        message: e.to_string(),
    })?;

    let mut schemas = Vec::new();
    for item in &ast.items {
        if let Some(schema) = parse_item(file, item)? {
            schemas.push(schema);
        }
    }
    Ok(schemas)
}

fn parse_item(file: &str, item: &Item) -> Result<Option<Schema>, SchemaError> {
    let Item::Struct(item) = item else {
        // An attribute on a non-struct is only an error if it is actually present.
        if storage_schema_attr(item_attrs(item)).is_some() {
            return Err(SchemaError::Parse {
                file: file.to_string(),
                message: format!(
                    "`#[storage_schema]` on `{}` must be applied to a struct with named fields. \
                     A storage schema is a shape, and only a struct declares one; enums encode as \
                     a tagged vector and cannot be migrated field by field.",
                    item_name(item)
                ),
            });
        }
        return Ok(None);
    };

    let Some(attr) = storage_schema_attr(&item.attrs) else {
        return Ok(None);
    };

    let mut version: Option<u32> = None;
    let mut name: Option<String> = None;
    let mut note: Option<String> = None;

    // `#[storage_schema]` with no argument list is the most common way to forget the
    // version. Reporting it as "expected attribute arguments in parentheses" is
    // technically true and useless, so it gets its own message.
    if matches!(attr.meta, syn::Meta::Path(_)) {
        return Err(SchemaError::Parse {
            file: file.to_string(),
            message: format!(
                "`#[storage_schema]` on `{}` is missing `version = N`. Without a version there is \
                 nothing to migrate from or to, and the framework's reads cannot be gated on it.",
                item.ident
            ),
        });
    }

    let parse_result = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("version") {
            let value = meta.value()?;
            let lit: syn::LitInt = value.parse()?;
            version = Some(lit.base10_parse()?);
            Ok(())
        } else if meta.path.is_ident("name") {
            let value = meta.value()?;
            let lit: syn::LitStr = value.parse()?;
            name = Some(lit.value());
            Ok(())
        } else if meta.path.is_ident("note") {
            let value = meta.value()?;
            let lit: syn::LitStr = value.parse()?;
            note = Some(lit.value());
            Ok(())
        } else {
            Err(meta.error(
                "unrecognised `#[storage_schema]` argument. Supported: `version = N`, \
                 `name = \"Type\"`, `note = \"...\"`",
            ))
        }
    });
    if let Err(e) = parse_result {
        return Err(SchemaError::Parse {
            file: file.to_string(),
            message: e.to_string(),
        });
    }

    let version = version.ok_or_else(|| SchemaError::Parse {
        file: file.to_string(),
        message: format!(
            "`#[storage_schema]` on `{}` is missing `version = N`. Without a version there is \
             nothing to migrate from or to, and the framework's reads cannot be gated.",
            item.ident
        ),
    })?;

    if version == 0 {
        return Err(SchemaError::Parse {
            file: file.to_string(),
            message: "schema versions start at 1; 0 is reserved for `unset`".into(),
        });
    }

    let Fields::Named(named) = &item.fields else {
        return Err(SchemaError::Parse {
            file: file.to_string(),
            message: format!(
                "`{}` must have named fields. A tuple struct has no stable key names, so a \
                 field-level diff — the whole point of a schema declaration — is impossible.",
                item.ident
            ),
        });
    };

    let mut fields = Vec::new();
    for field in &named.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        fields.push(Field::new(ident.to_string(), render_type(&field.ty)));
    }

    Ok(Some(Schema {
        version,
        name: name.unwrap_or_else(|| item.ident.to_string()),
        note: note.or_else(|| doc_comment(&item.attrs)),
        fields,
    }))
}

fn storage_schema_attr(attrs: &[Attribute]) -> Option<&Attribute> {
    attrs.iter().find(|a| {
        a.path()
            .segments
            .last()
            .is_some_and(|s| s.ident == "storage_schema")
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Enum(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        _ => &[],
    }
}

fn item_name(item: &Item) -> String {
    match item {
        Item::Enum(i) => i.ident.to_string(),
        Item::Union(i) => i.ident.to_string(),
        Item::Trait(i) => i.ident.to_string(),
        Item::Type(i) => i.ident.to_string(),
        _ => "<item>".to_string(),
    }
}

/// Renders a `syn::Type` the way an author would write it, without the spaces
/// `TokenStream::to_string` inserts.
///
/// A canonical spelling matters more than a pretty one: the value is committed to a
/// schema file and compared against later snapshots, so `Vec< u32 >` and `Vec<u32>`
/// must not look like a change.
fn render_type(ty: &Type) -> String {
    let text = quote::quote!(#ty).to_string();
    compact(&text)
}

/// The type renderer, exposed so the derive macros spell a field's type exactly the way
/// this parser does.
///
/// The derive macro emits the same declaration into the contract's own metadata, and
/// `soroban-migrate check` compares that against the schema snapshot built by this
/// parser. If the two spellings disagreed, every check would report a phantom
/// `Retyped` change. Sharing one function is the only way to guarantee they cannot.
pub fn render_field_type(ty: &Type) -> String {
    render_type(ty)
}

/// Removes the spaces `quote!` inserts around punctuation in a rendered type.
fn compact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            let prev = out.chars().next_back();
            let next = chars.peek().copied();
            let binds_left = matches!(prev, Some('<' | '(' | '[' | '&' | '\''));
            let binds_right = matches!(next, Some('>' | ')' | ']' | ',' | '<'));
            if binds_left || binds_right {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Joins a struct's `///` comments into a note.
fn doc_comment(attrs: &[Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                lines.push(s.value().trim().to_string());
            }
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join(" ").trim().to_string())
}

/// Renders Rust source for a schema file, so `soroban-migrate schema export` output
/// is reproducible and reviewable.
pub fn to_rust_module(schemas: &[Schema]) -> String {
    let mut out = String::from(
        "//! Storage schemas exported by `soroban-migrate schema export`.\n\
         //!\n\
         //! One struct per version. Historical versions are kept because a migration\n\
         //! diff needs both sides long after the old struct has stopped being the\n\
         //! shape the contract reads.\n\n",
    );
    for schema in schemas {
        if let Some(note) = &schema.note {
            out.push_str(&format!("/// {note}\n"));
        }
        // The identity is written explicitly. Without it, re-parsing the exported
        // module would derive the schema's name from the generated struct name and a
        // round trip would not be the identity — which would make every exported
        // snapshot look like a schema rename on the next export.
        out.push_str(&format!(
            "#[storage_schema(version = {}, name = \"{}\")]\n\
             #[contracttype]\n\
             pub struct {}V{} {{\n",
            schema.version, schema.name, schema.name, schema.version
        ));
        for f in &schema.fields {
            out.push_str(&format!("    pub {}: {},\n", f.name, f.declared));
        }
        out.push_str("}\n\n");
    }
    out
}

/// Field names and types for a schema, as `(name, declared)` pairs, for templates.
pub fn field_list(schema: &Schema) -> Vec<(String, String)> {
    schema
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.declared.clone()))
        .collect()
}
