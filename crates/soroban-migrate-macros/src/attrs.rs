//! Attribute parsing for the derives.
//!
//! Every argument is parsed into a typed struct rather than matched as raw tokens, so
//! that a typo produces a compile error naming the argument instead of a derive that
//! silently ignores it. A migration framework that accepts a misspelled `initt` and
//! generates a migration which does nothing is worse than one that refuses to
//! compile.

use syn::{DeriveInput, Expr, Ident, LitInt, LitStr, Path, Type};

/// A type that can be built from a derive input's attributes.
pub trait FromAttrs: Sized {
    /// The attribute this reads, without the `#[..]` wrapper, e.g. `storage_schema`.
    const ATTRIBUTE: &'static str;

    /// Parses the attribute.
    fn parse(attr: &syn::Attribute) -> syn::Result<Self>;

    /// The message shown when the attribute is absent.
    fn missing_message(ident: &Ident) -> String;
}

/// Parses `T`'s attribute off `input`, or reports that it is missing.
pub fn parse_attrs<T: FromAttrs>(input: &DeriveInput) -> syn::Result<T> {
    let attr = input
        .attrs
        .iter()
        .find(|a| a.path().is_ident(T::ATTRIBUTE))
        .ok_or_else(|| syn::Error::new_spanned(&input.ident, T::missing_message(&input.ident)))?;
    T::parse(attr)
}

/// Arguments to `#[storage_schema(..)]`.
pub struct StorageSchemaAttrs {
    pub version: u32,
    pub name: Option<String>,
    pub note: Option<String>,
}

impl FromAttrs for StorageSchemaAttrs {
    const ATTRIBUTE: &'static str = "storage_schema";

    fn missing_message(ident: &Ident) -> String {
        format!(
            "`StorageSchema` on `{ident}` needs a `#[storage_schema(version = N)]` attribute. \
             Without a version there is nothing to migrate from or to, and the framework's reads \
             cannot be gated on it."
        )
    }

    fn parse(attr: &syn::Attribute) -> syn::Result<Self> {
        let mut version: Option<u32> = None;
        let mut name: Option<String> = None;
        let mut note: Option<String> = None;

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("version") {
                let lit: LitInt = meta.value()?.parse()?;
                let value: u32 = lit.base10_parse()?;
                if value == 0 {
                    return Err(meta.error(
                        "schema versions start at 1. `0` is reserved for \"no version recorded\", \
                         which is a different state from being at the first version.",
                    ));
                }
                version = Some(value);
                Ok(())
            } else if meta.path.is_ident("name") {
                let lit: LitStr = meta.value()?.parse()?;
                name = Some(lit.value());
                Ok(())
            } else if meta.path.is_ident("note") {
                let lit: LitStr = meta.value()?.parse()?;
                note = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error(
                    "unrecognised argument. `#[storage_schema]` accepts `version = N`, \
                     `name = \"...\"`, and `note = \"...\"`.",
                ))
            }
        })?;

        Ok(Self {
            version: version.ok_or_else(|| {
                syn::Error::new_spanned(
                    attr,
                    "`#[storage_schema]` is missing `version = N`. Without a version there is \
                     nothing to migrate from or to.",
                )
            })?,
            name,
            note,
        })
    }
}

/// Arguments to `#[migration(..)]`.
pub struct MigrationAttrs {
    pub from: u32,
    pub to: u32,
    pub entry: Option<Type>,
    pub keys: Option<Path>,
    pub init: Vec<(Ident, Expr)>,
    pub dropped: Vec<String>,
    pub reversible: bool,
    pub name: Option<String>,
    /// Directives that were recognised but are refused, so the error can name them
    /// all at once rather than making the author discover them one compile at a time.
    pub denied: Vec<String>,
}

impl FromAttrs for MigrationAttrs {
    const ATTRIBUTE: &'static str = "migration";

    fn missing_message(ident: &Ident) -> String {
        format!(
            "`Migration` on `{ident}` needs a `#[migration(from = N, to = M, entry = Type)]` \
             attribute. The derive cannot infer the version pair or the new shape from the struct \
             it is applied to, because the migration struct carries no data of its own."
        )
    }

    fn parse(attr: &syn::Attribute) -> syn::Result<Self> {
        let mut from: Option<u32> = None;
        let mut to: Option<u32> = None;
        let mut entry: Option<Type> = None;
        let mut keys: Option<Path> = None;
        let mut init: Vec<(Ident, Expr)> = Vec::new();
        let mut dropped: Vec<String> = Vec::new();
        let mut reversible = false;
        let mut name: Option<String> = None;
        let mut denied: Vec<String> = Vec::new();

        attr.parse_nested_meta(|meta| {
            let ident = meta.path.get_ident();
            let is = |s: &str| ident.is_some_and(|i| i == s);

            if is("from") {
                from = Some(meta.value()?.parse::<LitInt>()?.base10_parse()?);
                Ok(())
            } else if is("to") {
                to = Some(meta.value()?.parse::<LitInt>()?.base10_parse()?);
                Ok(())
            } else if is("entry") {
                entry = Some(meta.value()?.parse()?);
                Ok(())
            } else if is("keys") {
                keys = Some(meta.value()?.parse()?);
                Ok(())
            } else if is("name") {
                name = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if is("reversible") {
                reversible = true;
                Ok(())
            } else if is("init") {
                meta.parse_nested_meta(|inner| {
                    let field = inner.path.get_ident().cloned().ok_or_else(|| {
                        inner.error("expected a field name, as in `init(frozen = false)`")
                    })?;
                    let expr: Expr = inner.value()?.parse()?;
                    init.push((field, expr));
                    Ok(())
                })
            } else if is("drop") {
                // Accepts `drop = ["a", "b"]`. A list rather than a repeatable
                // `drop(a)` because dropping is a declaration about the schema, and a
                // list reads as one.
                let value = meta.value()?;
                let array: syn::ExprArray = value.parse()?;
                for element in &array.elems {
                    match element {
                        Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => dropped.push(s.value()),
                        other => {
                            return Err(syn::Error::new_spanned(
                                other,
                                "`drop` takes field names as strings, as in `drop = [\"legacy\"]`",
                            ))
                        }
                    }
                }
                Ok(())
            } else if is("convert") || is("rename") {
                // Recognised so the error can be specific, rather than "unknown
                // argument" for a directive the CLI does support.
                denied.push(ident.map(|i| i.to_string()).unwrap_or_default());
                Ok(())
            } else {
                Err(meta.error(
                    "unrecognised argument. `#[migration]` accepts `from = N`, `to = N`, \
                     `entry = Type`, `keys = Path`, `init(field = expr)`, `drop = [\"field\"]`, \
                     `reversible`, and `name = \"...\"`.",
                ))
            }
        })?;

        Ok(Self {
            from: from.ok_or_else(|| {
                syn::Error::new_spanned(attr, "`#[migration]` is missing `from = N`")
            })?,
            to: to.ok_or_else(|| {
                syn::Error::new_spanned(attr, "`#[migration]` is missing `to = N`")
            })?,
            entry,
            keys,
            init,
            dropped,
            reversible,
            name,
            denied,
        })
    }
}
