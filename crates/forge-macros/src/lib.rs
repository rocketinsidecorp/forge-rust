//! Narrowly scoped derives for Forge (ADR 0039).
//!
//! `#[derive(OpenApiSchema)]` writes the `forge::openapi::OpenApiSchema`
//! implementation a person would write by hand: one
//! `ObjectSchema::field::<Type>(generator, "name", Field::new()...)` call per
//! member serde writes. It reads serde's own attributes, so the description
//! follows the encoding (`rename`, `rename_all`, `skip`,
//! `skip_serializing_if`, `flatten`, `transparent`), and refuses, at compile
//! time, every serde attribute that changes the JSON shape in a way it cannot
//! describe (`tag`, `untagged`, `with`, `from`, ...): a wrong description is
//! worse than none. `#[openapi(as = Type)]` describes such a type or member as
//! "encodes like `Type`".
//!
//! The expansion is plain trait code with no hidden behavior; the guide
//! (`docs/openapi.md`) shows the hand-written equivalent.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::quote;
use syn::ext::IdentExt as _;
use syn::meta::ParseNestedMeta;
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Error, Fields, GenericParam, LitStr, Path,
    Result, Token, Type, parse_macro_input, parse_quote,
};

/// Derives `forge::openapi::OpenApiSchema` from a struct or a unit-only enum
/// and its serde attributes.
///
/// Container options, `#[openapi(...)]`: `name = "Name"` (the component
/// name, instead of the Rust name), `doc = "..."` (its description),
/// `as = Type` (describe it as `Type`), `crate = "path"` (where
/// `OpenApiSchema` lives, `forge::openapi` by default).
///
/// Member options: `doc = "..."`, `example = "..."` (JSON, or text),
/// `format = "email"`, `required` (required although serde may omit it),
/// `nonnull` (not nullable although it is an `Option`), `as = Type`.
#[proc_macro_derive(OpenApiSchema, attributes(openapi))]
pub fn derive_openapi_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput) -> Result<Tokens> {
    let container = Container::parse(&input.attrs)?;
    let krate = container
        .krate
        .clone()
        .unwrap_or_else(|| parse_quote!(::forge::openapi));
    if let Some(lifetime) = input.generics.lifetimes().next() {
        return Err(Error::new_spanned(
            lifetime,
            "OpenApiSchema describes owned types: a type with lifetime parameters cannot be a \
             component; describe an owned counterpart with #[openapi(as = Type)]",
        ));
    }
    let mut generics = input.generics.clone();
    for parameter in &mut generics.params {
        if let GenericParam::Type(parameter) = parameter {
            parameter.bounds.push(parse_quote!(#krate::OpenApiSchema));
            parameter.bounds.push(parse_quote!('static));
        }
    }
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();
    let name = &input.ident;
    let methods = if let Some(target) = &container.as_type {
        quote! {
            fn schema(generator: &mut #krate::SchemaGenerator) -> #krate::Schema {
                generator.subschema::<#target>()
            }
            fn flatten(generator: &mut #krate::SchemaGenerator) -> ::core::option::Option<#krate::ObjectSchema> {
                generator.flatten_of::<#target>()
            }
        }
    } else {
        match &input.data {
            Data::Struct(data) => structure(&container, &krate, data)?,
            Data::Enum(data) => enumeration(&container, &krate, data)?,
            Data::Union(data) => {
                return Err(Error::new_spanned(
                    data.union_token,
                    "OpenApiSchema cannot describe a union",
                ));
            }
        }
    };
    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #krate::OpenApiSchema for #name #type_generics #where_clause {
            #methods
        }
    })
}

/// A named struct is a component, an object with one property per member;
/// a newtype or `transparent` struct is its member's schema.
fn structure(container: &Container, krate: &Path, data: &DataStruct) -> Result<Tokens> {
    match &data.fields {
        Fields::Named(fields) => {
            if container.transparent {
                let members: Vec<_> = fields
                    .named
                    .iter()
                    .map(|field| Member::parse(field, &field.attrs))
                    .collect::<Result<_>>()?;
                let shown: Vec<_> = members.iter().filter(|member| !member.skipped).collect();
                let [only] = shown.as_slice() else {
                    return Err(Error::new(
                        Span::call_site(),
                        "#[serde(transparent)] needs exactly one serialized field",
                    ));
                };
                return Ok(delegate(krate, &only.schema_type));
            }
            let mut statements = Vec::new();
            for field in &fields.named {
                let member = Member::parse(field, &field.attrs)?;
                let ident = field
                    .ident
                    .as_ref()
                    .map(|ident| ident.unraw().to_string())
                    .unwrap_or_default();
                if member.write_only {
                    // Never written, so not in the document, but a request
                    // may carry it (a password): typed decoding must not
                    // refuse it as unknown.
                    let wire = member.wire_name(container, &ident);
                    let names = std::iter::once(wire).chain(member.aliases.iter().cloned());
                    statements.push(quote! {
                        object.accept([#(#names),*]);
                    });
                    continue;
                }
                if member.skipped {
                    continue;
                }
                let schema_type = &member.schema_type;
                if member.flatten {
                    // serde writes none of a skipped flattened member's
                    // members, so none of them is required.
                    statements.push(if member.optional {
                        quote! { object.flatten_optional::<#schema_type>(generator, #ident); }
                    } else {
                        quote! { object.flatten::<#schema_type>(generator, #ident); }
                    });
                    continue;
                }
                let wire = member.wire_name(container, &ident);
                let options = member.options(krate);
                statements.push(quote! {
                    object.field::<#schema_type>(generator, #wire, #options);
                });
            }
            let description = container.doc.as_ref().map(|doc| {
                quote! { object.description(#doc); }
            });
            let component = component_name(container);
            Ok(quote! {
                fn schema(generator: &mut #krate::SchemaGenerator) -> #krate::Schema {
                    generator.component::<Self>(#component, |generator| {
                        match <Self as #krate::OpenApiSchema>::flatten(generator) {
                            ::core::option::Option::Some(object) => object.into_schema(),
                            ::core::option::Option::None => #krate::Schema::Null,
                        }
                    })
                }
                fn flatten(generator: &mut #krate::SchemaGenerator) -> ::core::option::Option<#krate::ObjectSchema> {
                    let mut object = #krate::ObjectSchema::new();
                    #description
                    #(#statements)*
                    ::core::option::Option::Some(object)
                }
            })
        }
        Fields::Unnamed(fields) => {
            let members: Vec<_> = fields
                .unnamed
                .iter()
                .map(|field| Member::parse(field, &field.attrs))
                .collect::<Result<_>>()?;
            let shown: Vec<_> = members.iter().filter(|member| !member.skipped).collect();
            match shown.as_slice() {
                [only] if fields.unnamed.len() == 1 || container.transparent => {
                    Ok(delegate(krate, &only.schema_type))
                }
                _ => Err(Error::new_spanned(
                    fields,
                    "OpenApiSchema describes newtype structs (one field) only: serde writes a \
                     tuple struct as an array; describe it with #[openapi(as = Type)]",
                )),
            }
        }
        Fields::Unit => Ok(quote! {
            fn schema(_generator: &mut #krate::SchemaGenerator) -> #krate::Schema {
                #krate::null_schema()
            }
        }),
    }
}

/// A newtype is written as its only member. The calls go through the
/// generator, which refuses a type that contains itself this way
/// (`struct A(Option<Box<A>>)`) instead of overflowing the stack.
fn delegate(krate: &Path, inner: &Type) -> Tokens {
    quote! {
        fn schema(generator: &mut #krate::SchemaGenerator) -> #krate::Schema {
            generator.subschema::<#inner>()
        }
        fn flatten(generator: &mut #krate::SchemaGenerator) -> ::core::option::Option<#krate::ObjectSchema> {
            generator.flatten_of::<#inner>()
        }
    }
}

/// An enum whose variants are all units is a string component listing the
/// variant names serde writes.
fn enumeration(container: &Container, krate: &Path, data: &DataEnum) -> Result<Tokens> {
    if container.transparent {
        return Err(Error::new(
            Span::call_site(),
            "#[serde(transparent)] applies to structs",
        ));
    }
    let mut values = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new_spanned(
                variant,
                "OpenApiSchema describes enums whose variants are all units (written as strings); \
                 describe this enum by implementing OpenApiSchema, or with #[openapi(as = Type)]",
            ));
        }
        let attributes = VariantAttributes::parse(&variant.attrs)?;
        if attributes.skipped {
            continue;
        }
        let ident = variant.ident.unraw().to_string();
        values.push(attributes.rename.unwrap_or_else(|| {
            container
                .rename_all
                .map_or_else(|| ident.clone(), |rule| rule.apply_to_variant(&ident))
        }));
    }
    if values.is_empty() {
        return Err(Error::new(
            Span::call_site(),
            "OpenApiSchema cannot describe an enum without serialized variants",
        ));
    }
    let component = component_name(container);
    let description = container.doc.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |doc| quote! { ::core::option::Option::Some(#doc) },
    );
    Ok(quote! {
        fn schema(generator: &mut #krate::SchemaGenerator) -> #krate::Schema {
            generator.component::<Self>(#component, |_generator| {
                #krate::string_enum(&[#(#values),*], #description)
            })
        }
    })
}

fn component_name(container: &Container) -> Tokens {
    container.name.as_ref().map_or_else(
        || quote! { ::core::option::Option::None },
        |name| quote! { ::core::option::Option::Some(#name) },
    )
}

/// The attributes of the deriving type.
#[derive(Default)]
struct Container {
    name: Option<String>,
    doc: Option<String>,
    as_type: Option<Type>,
    krate: Option<Path>,
    rename_all: Option<RenameRule>,
    transparent: bool,
}

impl Container {
    fn parse(attrs: &[Attribute]) -> Result<Self> {
        let mut container = Self::default();
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("openapi")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    container.name = Some(string(&meta)?);
                } else if meta.path.is_ident("doc") {
                    container.doc = Some(string(&meta)?);
                } else if meta.path.is_ident("as") {
                    container.as_type = Some(meta.value()?.parse()?);
                } else if meta.path.is_ident("crate") {
                    container.krate = Some(meta.value()?.parse::<LitStr>()?.parse()?);
                } else {
                    return Err(meta
                        .error("unknown openapi option on a type (want name, doc, as or crate)"));
                }
                Ok(())
            })?;
        }
        if container.as_type.is_some() {
            // The type says what it encodes like; its serde shape does not
            // matter.
            return Ok(container);
        }
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename_all") {
                    let rule = serialized_name(&meta)?;
                    container.rename_all = Some(RenameRule::parse(&meta, &rule)?);
                } else if meta.path.is_ident("transparent") {
                    container.transparent = true;
                } else if meta.path.is_ident("deny_unknown_fields") {
                    // Objects are always described with no other members.
                } else if ["rename", "default", "bound", "crate", "expecting"]
                    .iter()
                    .any(|name| meta.path.is_ident(name))
                {
                    // Names and decoding details that do not change what
                    // serde_json writes.
                    skip_value(&meta)?;
                } else {
                    return Err(meta.error(
                        "OpenApiSchema cannot describe this serde attribute (it changes the JSON \
                         shape); implement OpenApiSchema by hand or use #[openapi(as = Type)]",
                    ));
                }
                Ok(())
            })?;
        }
        Ok(container)
    }
}

/// One struct member and what its attributes say.
struct Member {
    schema_type: Type,
    rename: Option<String>,
    skipped: bool,
    /// `skip_serializing` alone: never written, but still read from a
    /// request, so it is accepted without being described.
    write_only: bool,
    /// `skip_deserializing`/`skip`: the other halves of the above.
    skip_read: bool,
    skip_write: bool,
    /// Extra names a request may use (`alias`).
    aliases: Vec<String>,
    flatten: bool,
    optional: bool,
    required: bool,
    nonnull: bool,
    doc: Option<String>,
    example: Option<String>,
    format: Option<String>,
}

impl Member {
    fn parse(field: &syn::Field, attrs: &[Attribute]) -> Result<Self> {
        let mut member = Self {
            schema_type: field.ty.clone(),
            rename: None,
            skipped: false,
            write_only: false,
            skip_read: false,
            skip_write: false,
            aliases: Vec::new(),
            flatten: false,
            optional: false,
            required: false,
            nonnull: false,
            doc: None,
            example: None,
            format: None,
        };
        let mut overridden = false;
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("openapi")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("doc") {
                    member.doc = Some(string(&meta)?);
                } else if meta.path.is_ident("example") {
                    member.example = Some(string(&meta)?);
                } else if meta.path.is_ident("format") {
                    member.format = Some(string(&meta)?);
                } else if meta.path.is_ident("required") {
                    member.required = true;
                } else if meta.path.is_ident("nonnull") {
                    member.nonnull = true;
                } else if meta.path.is_ident("as") {
                    member.schema_type = meta.value()?.parse()?;
                    overridden = true;
                } else {
                    return Err(meta.error(
                        "unknown openapi option on a member \
                         (want doc, example, format, required, nonnull or as)",
                    ));
                }
                Ok(())
            })?;
        }
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    member.rename = Some(serialized_name(&meta)?);
                } else if meta.path.is_ident("skip") {
                    member.skipped = true;
                    member.skip_read = true;
                    member.skip_write = true;
                } else if meta.path.is_ident("skip_serializing") {
                    member.skipped = true;
                    member.skip_write = true;
                } else if meta.path.is_ident("skip_deserializing") {
                    member.skip_read = true;
                } else if meta.path.is_ident("alias") {
                    member.aliases.push(string(&meta)?);
                } else if meta.path.is_ident("skip_serializing_if") {
                    skip_value(&meta)?;
                    member.optional = true;
                } else if meta.path.is_ident("flatten") {
                    member.flatten = true;
                } else if ["with", "serialize_with", "deserialize_with"]
                    .iter()
                    .any(|name| meta.path.is_ident(name))
                {
                    skip_value(&meta)?;
                    if !overridden {
                        return Err(meta.error(
                            "a member serialized with a custom function needs \
                             #[openapi(as = Type)]: the type it is written as",
                        ));
                    }
                } else if ["default", "borrow", "bound", "getter"]
                    .iter()
                    .any(|name| meta.path.is_ident(name))
                {
                    // Decoding-only details: the member is written as declared.
                    skip_value(&meta)?;
                } else {
                    return Err(meta.error(
                        "OpenApiSchema cannot describe this serde attribute; \
                         use #[openapi(as = Type)] or implement OpenApiSchema by hand",
                    ));
                }
                Ok(())
            })?;
        }
        member.write_only = member.skip_write && !member.skip_read && !member.flatten;
        if member.flatten
            && (member.rename.is_some()
                || member.doc.is_some()
                || member.example.is_some()
                || member.format.is_some()
                || member.required
                || member.nonnull)
        {
            return Err(Error::new_spanned(
                field,
                "a flattened member has no name or schema of its own: \
                 rename and the openapi options do not apply",
            ));
        }
        Ok(member)
    }

    /// The name serde writes (and reads): `rename`, else `rename_all`.
    fn wire_name(&self, container: &Container, ident: &str) -> String {
        self.rename.clone().unwrap_or_else(|| {
            container
                .rename_all
                .map_or_else(|| ident.to_owned(), |rule| rule.apply_to_field(ident))
        })
    }

    /// The `Field` options of this member.
    fn options(&self, krate: &Path) -> Tokens {
        let mut options = quote! { #krate::Field::new() };
        for alias in &self.aliases {
            options = quote! { #options.alias(#alias) };
        }
        if self.optional {
            options = quote! { #options.optional() };
        }
        if self.required {
            options = quote! { #options.required() };
        }
        if self.nonnull {
            options = quote! { #options.nonnull() };
        }
        if let Some(doc) = &self.doc {
            options = quote! { #options.doc(#doc) };
        }
        if let Some(example) = &self.example {
            options = quote! { #options.example(#example) };
        }
        if let Some(format) = &self.format {
            options = quote! { #options.format(#format) };
        }
        options
    }
}

/// The serde attributes of a unit variant.
struct VariantAttributes {
    rename: Option<String>,
    skipped: bool,
}

impl VariantAttributes {
    fn parse(attrs: &[Attribute]) -> Result<Self> {
        let mut variant = Self {
            rename: None,
            skipped: false,
        };
        if let Some(attr) = attrs.iter().find(|attr| attr.path().is_ident("openapi")) {
            return Err(Error::new_spanned(attr, "variants take no openapi options"));
        }
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    variant.rename = Some(serialized_name(&meta)?);
                } else if meta.path.is_ident("skip") || meta.path.is_ident("skip_serializing") {
                    variant.skipped = true;
                } else if ["alias", "skip_deserializing", "other", "bound"]
                    .iter()
                    .any(|name| meta.path.is_ident(name))
                {
                    skip_value(&meta)?;
                } else {
                    return Err(meta
                        .error("OpenApiSchema cannot describe this serde attribute on a variant"));
                }
                Ok(())
            })?;
        }
        Ok(variant)
    }
}

/// `name = "text"`.
fn string(meta: &ParseNestedMeta<'_>) -> Result<String> {
    Ok(meta.value()?.parse::<LitStr>()?.value())
}

/// The name serde writes: `rename = "x"`, or `rename(serialize = "x")`.
/// A different deserialize name would make the request and the response
/// shapes differ, which one schema cannot say.
fn serialized_name(meta: &ParseNestedMeta<'_>) -> Result<String> {
    if meta.input.peek(Token![=]) {
        return string(meta);
    }
    let mut serialize = None;
    let mut deserialize = None;
    meta.parse_nested_meta(|inner| {
        if inner.path.is_ident("serialize") {
            serialize = Some(string(&inner)?);
        } else if inner.path.is_ident("deserialize") {
            deserialize = Some(string(&inner)?);
        } else {
            return Err(inner.error("want serialize or deserialize"));
        }
        Ok(())
    })?;
    match (serialize, deserialize) {
        (Some(serialize), Some(deserialize)) if serialize != deserialize => {
            Err(meta.error("different serialize and deserialize names cannot share one schema"))
        }
        (Some(name), _) => Ok(name),
        _ => Err(meta.error(
            "a deserialize-only name differs from what serde writes; \
             give the serialize name too",
        )),
    }
}

/// Consumes `= value` or `(...)` of an attribute that does not change the
/// description.
fn skip_value(meta: &ParseNestedMeta<'_>) -> Result<()> {
    if meta.input.peek(Token![=]) {
        meta.value()?.parse::<LitStr>()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.parse_nested_meta(|inner| skip_value(&inner))?;
    }
    Ok(())
}

/// serde's `rename_all` rules, applied exactly as serde applies them.
#[derive(Clone, Copy)]
enum RenameRule {
    Lower,
    Upper,
    Pascal,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

impl RenameRule {
    fn parse(meta: &ParseNestedMeta<'_>, text: &str) -> Result<Self> {
        Ok(match text {
            "lowercase" => Self::Lower,
            "UPPERCASE" => Self::Upper,
            "PascalCase" => Self::Pascal,
            "camelCase" => Self::Camel,
            "snake_case" => Self::Snake,
            "SCREAMING_SNAKE_CASE" => Self::ScreamingSnake,
            "kebab-case" => Self::Kebab,
            "SCREAMING-KEBAB-CASE" => Self::ScreamingKebab,
            _ => return Err(meta.error(format!("unknown rename rule {text:?}"))),
        })
    }

    /// A member name (snake_case in Rust).
    fn apply_to_field(self, field: &str) -> String {
        match self {
            Self::Lower | Self::Snake => field.to_owned(),
            Self::Upper | Self::ScreamingSnake => field.to_ascii_uppercase(),
            Self::Pascal => {
                let mut pascal = String::new();
                let mut capitalize = true;
                for ch in field.chars() {
                    if ch == '_' {
                        capitalize = true;
                    } else if capitalize {
                        pascal.push(ch.to_ascii_uppercase());
                        capitalize = false;
                    } else {
                        pascal.push(ch);
                    }
                }
                pascal
            }
            Self::Camel => {
                let pascal = Self::Pascal.apply_to_field(field);
                lower_first(&pascal)
            }
            Self::Kebab => field.replace('_', "-"),
            Self::ScreamingKebab => field.to_ascii_uppercase().replace('_', "-"),
        }
    }

    /// A variant name (PascalCase in Rust).
    fn apply_to_variant(self, variant: &str) -> String {
        match self {
            Self::Pascal => variant.to_owned(),
            Self::Lower => variant.to_ascii_lowercase(),
            Self::Upper => variant.to_ascii_uppercase(),
            Self::Camel => lower_first(variant),
            Self::Snake => {
                let mut snake = String::new();
                for (index, ch) in variant.char_indices() {
                    if index > 0 && ch.is_uppercase() {
                        snake.push('_');
                    }
                    snake.push(ch.to_ascii_lowercase());
                }
                snake
            }
            Self::ScreamingSnake => Self::Snake.apply_to_variant(variant).to_ascii_uppercase(),
            Self::Kebab => Self::Snake.apply_to_variant(variant).replace('_', "-"),
            Self::ScreamingKebab => Self::ScreamingSnake
                .apply_to_variant(variant)
                .replace('_', "-"),
        }
    }
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_ascii_lowercase().to_string() + chars.as_str()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(source: &str) -> String {
        let input: DeriveInput = match syn::parse_str(source) {
            Ok(input) => input,
            Err(error) => panic!("{error}"),
        };
        match expand(&input) {
            Ok(tokens) => panic!("{source} must be refused, expanded to {tokens}"),
            Err(error) => error.to_string(),
        }
    }

    fn expansion(source: &str) -> String {
        let input: DeriveInput = match syn::parse_str(source) {
            Ok(input) => input,
            Err(error) => panic!("{error}"),
        };
        match expand(&input) {
            Ok(tokens) => tokens.to_string(),
            Err(error) => panic!("{source}: {error}"),
        }
    }

    #[test]
    fn shapes_it_cannot_describe_are_compile_errors() {
        for (source, reason) in [
            (
                "#[serde(tag = \"type\")] enum A { B { x: u8 } }",
                "changes the JSON shape",
            ),
            ("#[serde(untagged)] enum A { B }", "changes the JSON shape"),
            (
                "#[serde(into = \"u8\")] struct A { x: u8 }",
                "changes the JSON shape",
            ),
            ("enum A { B(u8) }", "variants are all units"),
            ("struct A<'a> { x: &'a str }", "lifetime parameters"),
            ("struct A(u8, u8);", "newtype structs"),
            (
                "struct A { #[serde(with = \"m\")] x: u8 }",
                "#[openapi(as = Type)]",
            ),
            (
                "struct A { #[openapi(colour = \"x\")] x: u8 }",
                "unknown openapi option",
            ),
            (
                "#[openapi(colour = \"x\")] struct A { x: u8 }",
                "unknown openapi option",
            ),
            (
                "struct A { #[serde(rename(serialize = \"a\", deserialize = \"b\"))] x: u8 }",
                "cannot share one schema",
            ),
            (
                "struct A { #[serde(flatten, rename = \"b\")] x: B }",
                "flattened member",
            ),
            ("union A { x: u8 }", "union"),
        ] {
            let error = refusal(source);
            assert!(error.contains(reason), "{source}: {error}");
        }
    }

    #[test]
    fn the_expansion_calls_the_public_api_with_serde_names() {
        let tokens = expansion(
            "#[serde(rename_all = \"camelCase\")] struct Post { created_at: u8, \
             #[serde(skip_serializing_if = \"Option::is_none\")] #[openapi(doc = \"d\")] \
             body: Option<String>, #[serde(skip)] hidden: u8 }",
        );
        assert!(tokens.contains("\"createdAt\""), "{tokens}");
        assert!(tokens.contains("optional ()"), "{tokens}");
        assert!(tokens.contains("doc (\"d\")"), "{tokens}");
        assert!(!tokens.contains("hidden"), "{tokens}");
        assert!(
            tokens.contains(":: forge :: openapi :: OpenApiSchema"),
            "{tokens}"
        );
        let custom =
            expansion("#[openapi(crate = \"forge_openapi\", name = \"X\")] struct A<T> { x: T }");
        assert!(
            custom.contains("forge_openapi :: OpenApiSchema"),
            "{custom}"
        );
        assert!(custom.contains("Some (\"X\")"), "{custom}");
        assert!(custom.contains("'static"), "{custom}");
    }

    #[test]
    fn rename_rules_match_serde() {
        let cases = [
            (
                RenameRule::Lower,
                "outcome_code",
                "outcome_code",
                "OutcomeCode",
                "outcomecode",
            ),
            (
                RenameRule::Upper,
                "outcome_code",
                "OUTCOME_CODE",
                "OutcomeCode",
                "OUTCOMECODE",
            ),
            (
                RenameRule::Pascal,
                "outcome_code",
                "OutcomeCode",
                "OutcomeCode",
                "OutcomeCode",
            ),
            (
                RenameRule::Camel,
                "outcome_code",
                "outcomeCode",
                "OutcomeCode",
                "outcomeCode",
            ),
            (
                RenameRule::Snake,
                "outcome_code",
                "outcome_code",
                "OutcomeCode",
                "outcome_code",
            ),
            (
                RenameRule::ScreamingSnake,
                "outcome_code",
                "OUTCOME_CODE",
                "OutcomeCode",
                "OUTCOME_CODE",
            ),
            (
                RenameRule::Kebab,
                "outcome_code",
                "outcome-code",
                "OutcomeCode",
                "outcome-code",
            ),
            (
                RenameRule::ScreamingKebab,
                "outcome_code",
                "OUTCOME-CODE",
                "OutcomeCode",
                "OUTCOME-CODE",
            ),
        ];
        for (rule, field, field_out, variant, variant_out) in cases {
            assert_eq!(rule.apply_to_field(field), field_out);
            assert_eq!(rule.apply_to_variant(variant), variant_out);
        }
    }
}
