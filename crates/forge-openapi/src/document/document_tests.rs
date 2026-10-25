//! Component naming and path parsing, the parts of the builder that are not
//! reachable with every input through the public API.

use super::*;

fn component(type_name: &'static str, explicit: Option<&str>) -> Component {
    Component {
        type_name,
        explicit_name: explicit.map(str::to_owned),
        schema: Value::Null,
    }
}

fn name_of(type_name: &'static str) -> String {
    component_name(&component(type_name, None), Qualification::Bare)
}

#[test]
fn names_are_upper_camel_case_with_type_arguments_reduced_to_their_names() {
    for (type_name, expected) in [
        ("app::Post", "Post"),
        ("app::post_summary", "PostSummary"),
        ("alloc::vec::Vec<app::Post>", "Vec_Post"),
        (
            "std::collections::HashMap<alloc::string::String, app::Post>",
            "HashMap_String_Post",
        ),
        ("&app::Post", "Post"),
        ("[app::Post; 4]", "Post_4"),
        ("(app::A, app::B)", "A_B"),
        ("app::Page<app::Page<app::Post>>", "Page_Page_Post"),
        ("<unnamed>", "Unnamed"),
    ] {
        assert_eq!(name_of(type_name), expected, "{type_name}");
    }
}

#[test]
fn a_chosen_name_wins_unless_nothing_is_left_of_it() {
    let chosen = component("app::Report", Some("Quarterly Report!"));
    assert_eq!(
        component_name(&chosen, Qualification::Path),
        "Quarterly_Report_"
    );
    let blank = component("app::Report", Some(""));
    assert_eq!(component_name(&blank, Qualification::Bare), "Report");
}

#[test]
fn qualification_adds_the_module_or_the_whole_path() {
    let item = component("app::v1::Item", None);
    assert_eq!(component_name(&item, Qualification::Bare), "Item");
    assert_eq!(component_name(&item, Qualification::Module), "v1.Item");
    assert_eq!(component_name(&item, Qualification::Path), "app.v1.Item");
    let page = component("pagination::Page<app::http::Post>", None);
    assert_eq!(
        component_name(&page, Qualification::Module),
        "pagination.Page_http.Post"
    );
    assert_eq!(
        component_name(&page, Qualification::Path),
        "pagination.Page_app.http.Post"
    );
    let bare = component("Item", None);
    assert_eq!(component_name(&bare, Qualification::Path), "Item");
}

#[test]
fn a_collision_is_qualified_by_module_then_by_path_and_the_order_is_stable() {
    let names = assign_names(&[
        component("b::x::Item", None),
        component("a::y::Item", None),
        component("a::x::Item", None),
    ])
    .expect("distinct by module");
    // `b::x` and `a::x` share a module name, so the whole path is used.
    assert_eq!(names, ["b.x.Item", "a.y.Item", "a.x.Item"]);
}

#[test]
fn paths_are_split_into_static_and_wildcard_segments() {
    assert!(parse_path("/").expect("root").is_empty());
    let segments = parse_path("/posts/{id}/comments").expect("segments");
    assert!(matches!(
        segments.as_slice(),
        [Segment::Static(a), Segment::Param(b), Segment::Static(c)]
            if a == "posts" && b == "id" && c == "comments"
    ));
    for (path, said) in [
        ("posts", "is not absolute"),
        ("/posts/{id", "wildcard segment"),
        ("/posts/id}", "wildcard segment"),
        ("/posts/{}", "wildcard segment"),
        ("/posts/{a}{b}", "wildcard segment"),
        ("/posts/x{id}", "wildcard segment"),
        ("/posts/{i{d}}", "wildcard segment"),
    ] {
        let error = parse_path(path).expect_err(path);
        assert!(error.contains(said), "{path}: {error}");
    }
}

#[test]
fn operation_ids_skip_punctuation_and_keep_unicode_words() {
    assert_eq!(
        operation_id("get", "/v1/user-profiles/{profile_id}"),
        "getV1UserProfilesByProfileId"
    );
    assert_eq!(operation_id("put", "/á/{x}/ñ"), "putÁByXÑ");
    assert_eq!(upper_first(""), "");
    assert_eq!(upper_first("ñandú"), "Ñandú");
}

#[test]
fn header_names_that_are_not_tokens_are_kept_as_written() {
    assert_eq!(canonical_header("X-REQUEST-id"), "X-Request-Id");
    assert_eq!(canonical_header("ETag"), "Etag");
    assert_eq!(canonical_header("x_custom.header"), "X_custom.header");
    assert_eq!(canonical_header("naïve"), "naïve");
    assert_eq!(canonical_header(""), "");
}
