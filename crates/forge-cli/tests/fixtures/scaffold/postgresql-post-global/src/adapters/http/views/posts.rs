//! View models of the post pages in `templates/posts/`. Values
//! are prepared as text by `crate::adapters::http::posts`; the
//! templates only lay them out (escaped).

use askama::Template;
use forge::views::FieldErrors;

use super::Layout;

/// A post ready for display.
pub struct PostRow {
    pub show_path: String,
    pub edit_path: String,
    pub title: String,
    pub body: String,
    pub published: String,
}

/// The values of the post form: as stored, or as submitted when the form
/// is shown again with errors.
#[derive(Default)]
pub struct PostFormValues {
    pub title: String,
    pub body: String,
    pub published: bool,
}

/// `templates/posts/index.html`.
#[derive(Template)]
#[template(path = "posts/index.html")]
pub struct PostIndex<'a> {
    pub layout: Layout<'a>,
    pub new_path: String,
    pub rows: Vec<PostRow>,
    pub next_path: Option<String>,
}

/// `templates/posts/show.html`, with the delete form.
#[derive(Template)]
#[template(path = "posts/show.html")]
pub struct PostShow<'a> {
    pub layout: Layout<'a>,
    pub csrf_field: String,
    pub back_path: String,
    pub version: i64,
    pub post: PostRow,
}

/// `templates/posts/new.html`, around `_form.html`.
#[derive(Template)]
#[template(path = "posts/new.html")]
pub struct PostNew<'a> {
    pub layout: Layout<'a>,
    pub csrf_field: String,
    pub action: String,
    /// `_method` for the form (`None` for a plain `POST`).
    pub method: Option<&'static str>,
    /// The optimistic-locking version the form was rendered with.
    pub version: Option<i64>,
    pub submit: &'static str,
    pub back_path: String,
    pub form: PostFormValues,
    pub errors: FieldErrors,
}

/// `templates/posts/edit.html`, around `_form.html`.
#[derive(Template)]
#[template(path = "posts/edit.html")]
pub struct PostEdit<'a> {
    pub layout: Layout<'a>,
    pub csrf_field: String,
    pub action: String,
    /// `_method` for the form (`Some("patch")`).
    pub method: Option<&'static str>,
    /// The optimistic-locking version the form was rendered with.
    pub version: Option<i64>,
    pub submit: &'static str,
    pub back_path: String,
    pub show_path: String,
    pub form: PostFormValues,
    pub errors: FieldErrors,
}

/// `templates/posts/not_found.html`, answered with `404`.
#[derive(Template)]
#[template(path = "posts/not_found.html")]
pub struct PostNotFound<'a> {
    pub layout: Layout<'a>,
    pub back_path: String,
}
