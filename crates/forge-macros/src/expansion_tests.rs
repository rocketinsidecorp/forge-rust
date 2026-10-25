//! What the derive expands to and refuses, attribute by attribute. The
//! expansions are checked as token text: the behavior of the generated
//! implementations is tested where they are used (`forge-openapi`).

use super::*;

fn parse(source: &str) -> DeriveInput {
    match syn::parse_str(source) {
        Ok(input) => input,
        Err(error) => panic!("{source}: {error}"),
    }
}

fn expansion(source: &str) -> String {
    match expand(&parse(source)) {
        Ok(tokens) => tokens.to_string(),
        Err(error) => panic!("{source}: {error}"),
    }
}

fn refusal(source: &str) -> String {
    match expand(&parse(source)) {
        Ok(tokens) => panic!("{source} must be refused, expanded to {tokens}"),
        Err(error) => error.to_string(),
    }
}

fn refused_with(cases: &[(&str, &str)]) {
    for (source, reason) in cases {
        let error = refusal(source);
        assert!(error.contains(reason), "{source}: {error}");
    }
}

#[test]
fn a_named_struct_is_a_component_with_one_call_per_serialized_member() {
    let tokens = expansion(
        "struct Post { id: u8, #[serde(rename = \"title\")] name: String, r#type: String, \
         #[serde(skip)] cache: u8, #[serde(skip_deserializing)] etag: String }",
    );
    assert!(
        tokens.contains("component :: < Self > (:: core :: option :: Option :: None"),
        "{tokens}"
    );
    assert!(
        tokens.contains("\"id\"") && tokens.contains("\"title\""),
        "{tokens}"
    );
    assert!(
        tokens.contains("\"type\""),
        "a raw identifier is written plain: {tokens}"
    );
    assert!(
        !tokens.contains("\"cache\"") && !tokens.contains("\"name\""),
        "{tokens}"
    );
    assert!(
        tokens.contains("\"etag\""),
        "skipping only on read still writes it: {tokens}"
    );
    assert!(tokens.contains("fn flatten"), "{tokens}");
}

#[test]
fn a_container_description_and_name_are_passed_on() {
    let tokens =
        expansion("#[openapi(name = \"Entry\", doc = \"An entry.\")] struct Item { a: u8 }");
    assert!(tokens.contains("Some (\"Entry\")"), "{tokens}");
    assert!(tokens.contains("description (\"An entry.\")"), "{tokens}");
}

#[test]
fn member_options_become_field_calls() {
    let tokens = expansion(
        "struct A { #[openapi(doc = \"d\", example = \"1\", format = \"email\", required, nonnull)] \
         #[serde(skip_serializing_if = \"Option::is_none\", alias = \"mail\", alias = \"addr\")] \
         email: Option<String> }",
    );
    for expected in [
        "alias (\"mail\")",
        "alias (\"addr\")",
        "optional ()",
        "required ()",
        "nonnull ()",
        "doc (\"d\")",
        "example (\"1\")",
        "format (\"email\")",
    ] {
        assert!(tokens.contains(expected), "{expected}: {tokens}");
    }
}

#[test]
fn a_member_written_by_a_custom_function_is_described_by_its_declared_type() {
    for attribute in [
        "with = \"m\"",
        "serialize_with = \"m::s\"",
        "deserialize_with = \"m::d\"",
    ] {
        let tokens = expansion(&format!(
            "struct A {{ #[serde({attribute})] #[openapi(as = String)] x: u8 }}"
        ));
        assert!(tokens.contains("< String >"), "{attribute}: {tokens}");
    }
    // The order of the attributes does not matter.
    let tokens = expansion("struct A { #[openapi(as = String)] #[serde(with = \"m\")] x: u8 }");
    assert!(tokens.contains("< String >"), "{tokens}");
}

#[test]
fn decoding_only_attributes_do_not_change_the_description() {
    let tokens = expansion(
        "#[serde(default, bound = \"T: Default\", expecting = \"a post\", crate = \"serde_x\", \
         deny_unknown_fields, rename = \"Entry\")] struct A { \
         #[serde(default = \"d\", borrow, bound(serialize = \"T: X\", deserialize = \"T: Y\"), getter = \"g\")] x: u8 }",
    );
    assert!(tokens.contains("\"x\""), "{tokens}");
}

#[test]
fn write_only_members_are_accepted_but_not_described() {
    let tokens = expansion(
        "#[serde(rename_all = \"camelCase\")] struct Login { email: String, \
         #[serde(skip_serializing, alias = \"pass\")] pass_word: String }",
    );
    assert!(tokens.contains("accept"), "{tokens}");
    assert!(
        tokens.contains("\"passWord\"") && tokens.contains("\"pass\""),
        "{tokens}"
    );
    assert!(
        !tokens.contains("field :: < String > (generator , \"passWord\""),
        "{tokens}"
    );
}

#[test]
fn flattened_members_are_merged_and_optional_when_serde_may_skip_them() {
    let tokens = expansion(
        "struct A { id: u8, #[serde(flatten)] audit: Audit, \
         #[serde(flatten, skip_serializing_if = \"Extra::is_empty\")] extra: Extra }",
    );
    assert!(
        tokens.contains("flatten :: < Audit > (generator , \"audit\")"),
        "{tokens}"
    );
    assert!(
        tokens.contains("flatten_optional :: < Extra > (generator , \"extra\")"),
        "{tokens}"
    );
    // A flattened member serde never writes contributes nothing.
    let tokens = expansion("struct A { #[serde(flatten, skip_serializing)] audit: Audit }");
    assert!(!tokens.contains("Audit"), "{tokens}");
}

#[test]
fn newtypes_and_transparent_structs_are_their_member() {
    for source in [
        "struct Sku(String);",
        "#[serde(transparent)] struct Sku(String);",
        "#[serde(transparent)] struct Email { address: String }",
        "#[serde(transparent)] struct Email { address: String, #[serde(skip)] cache: u8 }",
        "#[serde(transparent)] struct Sku(#[serde(skip)] u8, String);",
    ] {
        let tokens = expansion(source);
        assert!(tokens.contains("subschema"), "{source}: {tokens}");
        assert!(tokens.contains("flatten_of"), "{source}: {tokens}");
        assert!(!tokens.contains("component"), "{source}: {tokens}");
    }
    let tokens = expansion("struct Marker;");
    assert!(tokens.contains("null_schema"), "{tokens}");
}

#[test]
fn a_type_can_be_described_as_another() {
    let tokens = expansion(
        "#[openapi(as = String)] #[serde(tag = \"kind\", bound = \"x\")] enum Opaque { A { x: u8 } }",
    );
    assert!(tokens.contains("subschema :: < String >"), "{tokens}");
    assert!(tokens.contains("flatten_of :: < String >"), "{tokens}");
    let tokens = expansion("#[openapi(as = Vec<String>)] struct Tags(u8);");
    assert!(
        tokens.contains("subschema :: < Vec < String > >"),
        "{tokens}"
    );
}

#[test]
fn a_unit_enum_lists_the_names_serde_writes() {
    let tokens = expansion(
        "#[serde(rename_all = \"kebab-case\")] #[openapi(doc = \"Where.\")] enum State { \
         InReview, #[serde(rename = \"live\")] Published, #[serde(skip)] Hidden, \
         #[serde(skip_serializing)] Internal, #[serde(alias = \"gone\", skip_deserializing, other, bound = \"x\")] Archived, \
         r#Type }",
    );
    assert!(tokens.contains("\"in-review\""), "{tokens}");
    assert!(tokens.contains("\"live\""), "{tokens}");
    assert!(tokens.contains("\"archived\""), "{tokens}");
    assert!(tokens.contains("\"type\""), "{tokens}");
    assert!(
        !tokens.contains("hidden") && !tokens.contains("internal"),
        "{tokens}"
    );
    assert!(tokens.contains("Some (\"Where.\")"), "{tokens}");
    let plain = expansion("enum Mode { Fast, Slow }");
    assert!(
        plain.contains("Option :: None")
            && plain.contains("\"Fast\"")
            && plain.contains("\"Slow\""),
        "{plain}"
    );
}

#[test]
fn rename_all_applies_to_variants_like_serde() {
    for (rule, expected) in [
        ("lowercase", "\"inreview\""),
        ("UPPERCASE", "\"INREVIEW\""),
        ("PascalCase", "\"InReview\""),
        ("camelCase", "\"inReview\""),
        ("snake_case", "\"in_review\""),
        ("SCREAMING_SNAKE_CASE", "\"IN_REVIEW\""),
        ("kebab-case", "\"in-review\""),
        ("SCREAMING-KEBAB-CASE", "\"IN-REVIEW\""),
    ] {
        let tokens = expansion(&format!(
            "#[serde(rename_all = \"{rule}\")] enum S {{ InReview }}"
        ));
        assert!(tokens.contains(expected), "{rule}: {tokens}");
    }
    let tokens = expansion(
        "#[serde(rename_all(serialize = \"snake_case\", deserialize = \"snake_case\"))] enum S { InReview }",
    );
    assert!(tokens.contains("\"in_review\""), "{tokens}");
}

#[test]
fn rename_all_applies_to_fields_like_serde() {
    for (rule, expected) in [
        ("lowercase", "\"created_at\""),
        ("UPPERCASE", "\"CREATED_AT\""),
        ("PascalCase", "\"CreatedAt\""),
        ("camelCase", "\"createdAt\""),
        ("snake_case", "\"created_at\""),
        ("SCREAMING_SNAKE_CASE", "\"CREATED_AT\""),
        ("kebab-case", "\"created-at\""),
        ("SCREAMING-KEBAB-CASE", "\"CREATED-AT\""),
    ] {
        let tokens = expansion(&format!(
            "#[serde(rename_all = \"{rule}\")] struct S {{ created_at: u8 }}"
        ));
        assert!(tokens.contains(expected), "{rule}: {tokens}");
    }
    // An explicit rename beats the rule.
    let tokens = expansion(
        "#[serde(rename_all = \"camelCase\")] struct S { #[serde(rename = \"when\")] created_at: u8 }",
    );
    assert!(
        tokens.contains("\"when\"") && !tokens.contains("createdAt"),
        "{tokens}"
    );
    // `rename(serialize = ...)` is the written name.
    let tokens = expansion("struct S { #[serde(rename(serialize = \"when\"))] created_at: u8 }");
    assert!(tokens.contains("\"when\""), "{tokens}");
    let tokens = expansion(
        "struct S { #[serde(rename(serialize = \"when\", deserialize = \"when\"))] created_at: u8 }",
    );
    assert!(tokens.contains("\"when\""), "{tokens}");
}

#[test]
fn what_cannot_be_described_is_refused_with_the_reason() {
    refused_with(&[
        // Containers.
        (
            "#[serde(rename_all = \"Title Case\")] struct A { x: u8 }",
            "unknown rename rule",
        ),
        (
            "#[serde(transparent)] struct A { a: u8, b: u8 }",
            "exactly one serialized field",
        ),
        (
            "#[serde(transparent)] struct A { #[serde(skip)] a: u8 }",
            "exactly one serialized field",
        ),
        ("#[serde(transparent)] enum A { B }", "applies to structs"),
        ("enum A {}", "without serialized variants"),
        ("enum A { #[serde(skip)] B }", "without serialized variants"),
        (
            "#[serde(rename_all = \"snake_case\")] enum A { B { x: u8 } }",
            "variants are all units",
        ),
        (
            "enum A { #[openapi(doc = \"x\")] B }",
            "variants take no openapi options",
        ),
        (
            "enum A { #[serde(flatten)] B }",
            "cannot describe this serde attribute on a variant",
        ),
        (
            "#[serde(from = \"u8\")] struct A { x: u8 }",
            "changes the JSON shape",
        ),
        (
            "#[openapi(unknown)] struct A { x: u8 }",
            "unknown openapi option on a type",
        ),
        // Members.
        (
            "struct A { #[serde(getter_x)] x: u8 }",
            "cannot describe this serde attribute",
        ),
        (
            "struct A { #[openapi(nope)] x: u8 }",
            "unknown openapi option on a member",
        ),
        ("struct A(u8, u8);", "newtype structs"),
        (
            "struct A(#[serde(skip)] u8, #[serde(skip)] u8);",
            "newtype structs",
        ),
        // Names.
        (
            "struct A { #[serde(rename(deserialize = \"b\"))] x: u8 }",
            "deserialize-only name",
        ),
        (
            "struct A { #[serde(rename(other = \"b\"))] x: u8 }",
            "want serialize or deserialize",
        ),
        (
            "struct A { #[serde(rename(serialize = \"a\", deserialize = \"b\"))] x: u8 }",
            "cannot share one schema",
        ),
        (
            "enum A { #[serde(rename(serialize = \"a\", deserialize = \"b\"))] B }",
            "cannot share one schema",
        ),
        // Flattened members have no name or schema of their own.
        (
            "struct A { #[serde(flatten)] #[openapi(doc = \"d\")] x: B }",
            "flattened member",
        ),
        (
            "struct A { #[serde(flatten)] #[openapi(example = \"e\")] x: B }",
            "flattened member",
        ),
        (
            "struct A { #[serde(flatten)] #[openapi(format = \"f\")] x: B }",
            "flattened member",
        ),
        (
            "struct A { #[serde(flatten)] #[openapi(required)] x: B }",
            "flattened member",
        ),
        (
            "struct A { #[serde(flatten)] #[openapi(nonnull)] x: B }",
            "flattened member",
        ),
        // Types.
        ("struct A<'a> { x: &'a str }", "lifetime parameters"),
        ("union A { x: u8 }", "union"),
    ]);
}

#[test]
fn generic_types_bound_their_parameters_and_keep_the_where_clause() {
    let tokens = expansion("struct Page<T, U: Clone> where U: Send { items: Vec<T>, extra: U }");
    assert!(tokens.matches("OpenApiSchema").count() >= 3, "{tokens}");
    assert!(tokens.contains("'static"), "{tokens}");
    assert!(tokens.contains("where U : Send"), "{tokens}");
}

#[test]
fn the_crate_path_is_configurable() {
    let tokens = expansion("#[openapi(crate = \"my::openapi\")] struct A { x: u8 }");
    assert!(
        tokens.contains("my :: openapi :: OpenApiSchema"),
        "{tokens}"
    );
    assert!(!tokens.contains(":: forge ::"), "{tokens}");
}
