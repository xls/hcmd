//! `#[derive(ConfigDoc)]`.
//!
//! The design makes the config structs the single source of truth for
//! `config.toml`: the file is generated from them rather than hand-kept beside
//! them. A doc comment lives in the source and nowhere else, and the only way
//! to read it back as data is at compile time, from the `#[doc = "..."]`
//! attributes the compiler has already turned each `///` line into. That is
//! what this derive does, and it is the whole reason a proc-macro crate exists
//! in this workspace.
//!
//! For a struct `S` it emits one inherent method:
//!
//! ```ignore
//! impl S {
//!     fn describe(section: &str, out: &mut Vec<crate::config::emit::SchemaItem>) { .. }
//! }
//! ```
//!
//! which appends, in field-declaration order, the schema for this struct:
//!
//! * a **leaf** field becomes an option under `section` (its key and comment),
//! * a `#[config(section)]` field becomes a `[section.key]` header (comment
//!   taken from the field, the parent) followed by that sub-struct's own
//!   `describe`,
//! * a `#[config(array)]` field becomes a `[[section.key]]` array-of-tables
//!   header,
//! * a `#[config(skip)]` or `#[serde(skip)]` field is left out.
//!
//! The section header comment is taken from the **field** in the parent, not
//! from the sub-struct's own doc: the field docs in `Config` are the short,
//! user-facing one-liners the file wants at the top of each section, so the
//! design reads the header from there and leaves each sub-struct's own doc for
//! `rustdoc`.
//!
//! The generated code names `crate::config::emit` by an absolute path. The
//! derive has exactly one user in the tree, the config module, so a fixed path
//! is honest about that rather than pretending to be a general-purpose tool.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Attribute, Data, DeriveInput, Fields, LitStr, Meta, parse_macro_input};

/// Derive `describe(section, out)`, the ordered schema of this config struct.
#[proc_macro_derive(ConfigDoc, attributes(config))]
pub fn derive_config_doc(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let Data::Struct(data) = &input.data else {
        return syn::Error::new_spanned(&input.ident, "ConfigDoc is only for structs")
            .to_compile_error()
            .into();
    };
    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(&input.ident, "ConfigDoc needs named fields")
            .to_compile_error()
            .into();
    };

    let rename_all = struct_rename_all(&input.attrs);

    let mut pushes = Vec::new();
    for field in &fields.named {
        let Some(ident) = field.ident.as_ref() else {
            continue;
        };
        let kind = field_kind(&field.attrs);
        if matches!(kind, Kind::Skip) {
            continue;
        }
        let key = field_key(&field.attrs, &ident.to_string(), rename_all.as_deref());
        let comment = doc_of(&field.attrs);
        let key_lit = LitStr::new(&key, ident.span());
        let comment_lit = LitStr::new(&comment, ident.span());

        match kind {
            Kind::Leaf => pushes.push(quote! {
                out.push(crate::config::emit::SchemaItem::option(section, #key_lit, #comment_lit));
            }),
            Kind::Section => {
                let ty = &field.ty;
                pushes.push(quote! {
                    {
                        let __child = crate::config::emit::join_section(section, #key_lit);
                        out.push(crate::config::emit::SchemaItem::section(&__child, #comment_lit));
                        <#ty>::describe(&__child, out);
                    }
                });
            }
            Kind::Array => pushes.push(quote! {
                {
                    let __child = crate::config::emit::join_section(section, #key_lit);
                    out.push(crate::config::emit::SchemaItem::array_section(&__child, #comment_lit));
                }
            }),
            Kind::Skip => {}
        }
    }

    let expanded = quote! {
        impl #name {
            #[doc(hidden)]
            pub fn describe(
                section: &str,
                out: &mut ::std::vec::Vec<crate::config::emit::SchemaItem>,
            ) {
                #(#pushes)*
            }
        }
    };
    expanded.into()
}

/// How a field maps into the schema.
enum Kind {
    /// An ordinary option under the current section.
    Leaf,
    /// A nested config struct, `[section.key]`.
    Section,
    /// An array-of-tables, `[[section.key]]`.
    Array,
    /// Not part of the generated file at all.
    Skip,
}

/// Read `#[config(...)]` (and `#[serde(skip)]`) to decide a field's kind.
fn field_kind(attrs: &[Attribute]) -> Kind {
    if serde_has_skip(attrs) {
        return Kind::Skip;
    }
    let mut kind = Kind::Leaf;
    for attr in attrs {
        if !attr.path().is_ident("config") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("section") {
                kind = Kind::Section;
            } else if meta.path.is_ident("array") {
                kind = Kind::Array;
            } else if meta.path.is_ident("skip") {
                kind = Kind::Skip;
            }
            Ok(())
        });
    }
    kind
}

/// The TOML key for a field: `#[serde(rename = "..")]` wins, then the struct's
/// `rename_all`, then the field name as written.
fn field_key(attrs: &[Attribute], ident: &str, rename_all: Option<&str>) -> String {
    if let Some(renamed) = serde_rename(attrs) {
        return renamed;
    }
    match rename_all {
        Some("lowercase") => ident.to_lowercase(),
        Some("snake_case") => ident.to_string(),
        _ => ident.to_string(),
    }
}

/// The `rename = ".."` on a field's `#[serde(..)]`, if any.
fn serde_rename(attrs: &[Attribute]) -> Option<String> {
    let mut found = None;
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename")
                && let Ok(value) = meta.value()
                && let Ok(lit) = value.parse::<LitStr>()
            {
                found = Some(lit.value());
            }
            Ok(())
        });
    }
    found
}

/// The `rename_all = ".."` on a struct's `#[serde(..)]`, if any.
fn struct_rename_all(attrs: &[Attribute]) -> Option<String> {
    let mut found = None;
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all")
                && let Ok(value) = meta.value()
                && let Ok(lit) = value.parse::<LitStr>()
            {
                found = Some(lit.value());
            }
            Ok(())
        });
    }
    found
}

/// Does a field carry `#[serde(skip)]` (or `skip_serializing`)?
fn serde_has_skip(attrs: &[Attribute]) -> bool {
    let mut skip = false;
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip")
                || meta.path.is_ident("skip_serializing")
                || meta.path.is_ident("skip_deserializing")
            {
                skip = true;
            }
            Ok(())
        });
    }
    skip
}

/// Join a struct's `///` lines into one comment, newlines kept so a multi-line
/// doc becomes multiple `#` lines in the file. One leading space is trimmed,
/// which is the space `///` puts after itself.
fn doc_of(attrs: &[Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let Meta::NameValue(nv) = &attr.meta else {
            continue;
        };
        if let syn::Expr::Lit(expr) = &nv.value
            && let syn::Lit::Str(lit) = &expr.lit
        {
            let raw = lit.value();
            lines.push(raw.strip_prefix(' ').unwrap_or(&raw).to_string());
        }
    }
    // Trailing blank doc lines carry no information into the file.
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}
