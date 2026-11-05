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

use std::collections::BTreeMap;

use syn::{Attribute, Fields, Item, Lit, Type};

use crate::error::SchemaError;
use crate::model::{referenced_names, Field, KeySpace, KeyVariant, Schema};

/// Parses every `#[storage_schema]` declaration in one Rust source file.
///
/// Returns them in declaration order. A file with none is not an error: contracts
/// keep schemas in their own module, and a directory walk will hand this function
/// plenty of files that have nothing to do with migrations.
///
/// Embedded shapes are resolved within this file only. A contract whose embedded
/// structs live in another file should use [`parse_sources`], which sees all of them.
///
/// # Errors
///
/// [`SchemaError::Parse`] for malformed Rust, and for a `#[storage_schema]`
/// attribute that is missing `version`, has an unparseable argument, or is applied to
/// something other than a struct with named fields. Also for a schema that reaches
/// itself through its fields; see [`parse_sources`].
pub fn parse_source(file: &str, source: &str) -> Result<Vec<Schema>, SchemaError> {
    parse_sources(&[(file, source)])
}

/// Parses every `#[storage_schema]` declaration across a set of Rust source files,
/// and records for each one the shapes its fields reach.
///
/// # Embedded shapes
///
/// A field whose type is another `#[contracttype]` struct — `meta: Metadata`, or
/// `Vec<Metadata>` — is encoded as a map inside the entry's own map. Changing
/// `Metadata` changes every entry that embeds it, even when the embedding struct is
/// untouched. So each schema's [`Schema::nested`] records the transitive closure of
/// struct shapes its fields reach, read from every file at once: an embedded struct
/// usually lives in a different module from the schema that uses it.
///
/// A struct counts as a shape if it carries `#[contracttype]` or `#[storage_schema]`
/// and has named fields. Shapes are matched to field types by identifier, which is
/// the only name a field type can use for them.
///
/// # Errors
///
/// Everything [`parse_source`] reports, and two more, both naming the file:
///
/// * A schema that reaches **itself**, directly or through another shape. Soroban's
///   map encoding has no references, so a recursive shape has no fixed layout to
///   snapshot and cannot be migrated field by field.
/// * A schema that reaches a struct name declared **more than once** with different
///   fields. Which of the two the field means depends on imports this parser does not
///   resolve, and guessing would check the wrong shape. Duplicates nothing reaches are
///   ignored.
pub fn parse_sources(files: &[(&str, &str)]) -> Result<Vec<Schema>, SchemaError> {
    let mut schemas: Vec<(String, String, Schema)> = Vec::new();
    let mut shapes = ShapeTable::default();

    for (file, source) in files {
        let ast = syn::parse_file(source).map_err(|e| SchemaError::Parse {
            file: (*file).to_string(),
            message: e.to_string(),
        })?;
        for item in &ast.items {
            if let Some(schema) = parse_item(file, item)? {
                let Item::Struct(s) = item else {
                    unreachable!("parse_item only returns a schema for a struct")
                };
                schemas.push(((*file).to_string(), s.ident.to_string(), schema));
            }
            if let Item::Struct(s) = item {
                shapes.record(file, s);
            }
        }
    }

    schemas
        .into_iter()
        .map(|(file, ident, mut schema)| {
            schema.nested = shapes.closure(&file, &ident, &schema)?;
            Ok(schema)
        })
        .collect()
}

/// Every struct shape in the source, by identifier.
#[derive(Default)]
struct ShapeTable {
    /// Each identifier's declarations, as `(file, fields)`. More than one only when the
    /// same name is declared in several places.
    by_ident: BTreeMap<String, Vec<(String, Vec<Field>)>>,
}

impl ShapeTable {
    fn record(&mut self, file: &str, item: &syn::ItemStruct) {
        let is_shape = carries_attr_named(&item.attrs, "contracttype")
            || storage_schema_attr(&item.attrs).is_some();
        let Fields::Named(named) = &item.fields else {
            return;
        };
        if !is_shape {
            return;
        }
        let fields = named
            .named
            .iter()
            .filter_map(|f| {
                f.ident
                    .as_ref()
                    .map(|i| Field::new(i.to_string(), render_type(&f.ty)))
            })
            .collect();
        self.by_ident
            .entry(item.ident.to_string())
            .or_default()
            .push((file.to_string(), fields));
    }

    /// The one declaration of `ident`, if it is a shape.
    fn get(&self, file: &str, ident: &str) -> Result<Option<&Vec<Field>>, SchemaError> {
        let Some(decls) = self.by_ident.get(ident) else {
            return Ok(None);
        };
        let first = &decls[0].1;
        if decls.iter().any(|(_, fields)| fields != first) {
            return Err(SchemaError::Parse {
                file: file.to_string(),
                message: format!(
                    "`{ident}` is embedded in a storage schema, but more than one struct of that                      name is declared, with different fields ({}). Which one the field means                      depends on imports, and checking the wrong one would hide a change to the                      other. Rename one of them.",
                    decls
                        .iter()
                        .map(|(f, _)| format!("`{f}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Ok(Some(first))
    }

    /// Every shape `schema` reaches through its fields, refusing a cycle.
    fn closure(
        &self,
        file: &str,
        root: &str,
        schema: &Schema,
    ) -> Result<BTreeMap<String, Vec<Field>>, SchemaError> {
        let mut out = BTreeMap::new();
        let mut path = vec![root.to_string()];
        for field in &schema.fields {
            self.walk(file, &field.declared, &mut path, &mut out)?;
        }
        Ok(out)
    }

    /// Depth-first over the shapes `declared` names. `path` is the chain of shapes
    /// being expanded, so a name already on it is a cycle; `out` doubles as the
    /// visited set, so a shape reached twice without a cycle is expanded once.
    fn walk(
        &self,
        file: &str,
        declared: &str,
        path: &mut Vec<String>,
        out: &mut BTreeMap<String, Vec<Field>>,
    ) -> Result<(), SchemaError> {
        for name in referenced_names(declared) {
            if path.contains(&name) {
                path.push(name);
                return Err(SchemaError::Parse {
                    file: file.to_string(),
                    message: format!(
                        "`{}` contains itself ({}). A `#[contracttype]` value is encoded by                          value, with no references, so a recursive shape has no fixed layout to                          snapshot or migrate. Store the inner values as separate entries under                          their own keys instead.",
                        path[0],
                        path.join(" -> ")
                    ),
                });
            }
            if out.contains_key(&name) {
                continue;
            }
            let Some(fields) = self.get(file, &name)? else {
                continue;
            };
            out.insert(name.clone(), fields.clone());
            path.push(name);
            for f in fields {
                self.walk(file, &f.declared, path, out)?;
            }
            path.pop();
        }
        Ok(())
    }
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
        nested: BTreeMap::new(),
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

/// Parses a contract's key-space enum out of one Rust source file.
///
/// # Which enum
///
/// A key space is identified by structure rather than by name, and either of two
/// independent signals is enough: an enum that derives `Keyspace`, or one with a
/// variant marked `#[migration]`. Accepting both means the key space is found whether
/// or not the derive is in scope, and whether or not the migration variant has been
/// renamed to something less obvious.
///
/// A file with neither returns `None`, which is the common case: most files have no
/// key space, and a scan over a source tree hands this function plenty of them.
///
/// # Errors
///
/// [`SchemaError::Parse`] only for source that is not valid Rust. Unlike
/// `#[storage_schema]`, there is no malformed key space to report: the enum carries no
/// attribute of ours to be malformed, and an enum shape that cannot be read is simply
/// not a key space.
pub fn parse_key_space(file: &str, source: &str) -> Result<Option<KeySpace>, SchemaError> {
    let ast = syn::parse_file(source).map_err(|e| SchemaError::Parse {
        file: file.to_string(),
        message: e.to_string(),
    })?;

    for item in &ast.items {
        if let Item::Enum(item) = item {
            if let Some(space) = key_space_of(item) {
                return Ok(Some(space));
            }
        }
    }
    Ok(None)
}

/// Builds a key space from an enum, if that enum is one.
fn key_space_of(item: &syn::ItemEnum) -> Option<KeySpace> {
    let marked = item
        .variants
        .iter()
        .any(|v| carries_attr(&v.attrs, "migration"));
    if !marked && !derives_named(&item.attrs, "Keyspace") {
        return None;
    }

    let variants = item
        .variants
        .iter()
        .map(|v| KeyVariant {
            name: v.ident.to_string(),
            payload: payload_of(&v.fields),
            migration: carries_attr(&v.attrs, "migration"),
        })
        .collect();
    Some(KeySpace::new(item.ident.to_string(), variants))
}

/// The declared payload of a variant, in declaration order.
///
/// Declaration order rather than a sorted order, because the encoded vector follows it.
/// A named-field variant keeps its field names, since those are its map keys.
fn payload_of(fields: &Fields) -> Option<String> {
    let rendered = match fields {
        Fields::Unit => return None,
        Fields::Unnamed(f) => f
            .unnamed
            .iter()
            .map(|f| render_field_type(&f.ty))
            .collect::<Vec<_>>()
            .join(", "),
        Fields::Named(f) => f
            .named
            .iter()
            .map(|f| {
                let name = f
                    .ident
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                format!("{}: {}", name, render_field_type(&f.ty))
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    Some(rendered)
}

/// Whether an attribute list contains `#[name]`.
fn carries_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.path().is_ident(name))
}

/// Whether an attribute list contains `#[name]` or `#[path::to::name]`, matched on the
/// last path segment so `#[soroban_sdk::contracttype]` counts.
fn carries_attr_named(attrs: &[Attribute], name: &str) -> bool {
    attrs
        .iter()
        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == name))
}

/// Whether a `#[derive(..)]` list names a particular trait.
fn derives_named(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("derive") {
            return false;
        }
        let mut found = false;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(name) {
                found = true;
            }
            Ok(())
        });
        found
    })
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
