//! View models of the event pages in `templates/events/`. Values
//! are prepared as text by `crate::adapters::http::events`; the
//! templates only lay them out (escaped).

use askama::Template;
use forge::views::FieldErrors;

use super::Layout;

/// A event ready for display.
pub struct EventRow {
    /// `id` of the row element, the target of live updates.
    pub dom_id: String,
    pub show_path: String,
    pub edit_path: String,
    pub name: String,
    pub notes: String,
    pub starts_at: String,
    pub ends_at: String,
    pub capacity: String,
    pub attendees: String,
    pub public: String,
    pub featured: String,
    pub venue: String,
    pub organizer: String,
}

/// The values of the event form: as stored, or as submitted when the form
/// is shown again with errors.
#[derive(Default)]
pub struct EventFormValues {
    pub name: String,
    pub notes: String,
    pub starts_at: String,
    pub ends_at: String,
    pub capacity: String,
    pub attendees: String,
    pub public: bool,
    pub featured: String,
    pub venue: String,
    pub organizer: String,
}

/// `templates/events/index.html`.
#[derive(Template)]
#[template(path = "events/index.html")]
pub struct EventIndex<'a> {
    pub layout: Layout<'a>,
    pub new_path: String,
    pub rows: Vec<EventRow>,
    pub next_path: Option<String>,
    /// `<turbo-stream-source>` of the live updates, `None` when signing
    /// failed (the page then does not update live).
    pub stream_source: Option<String>,
}

/// `templates/events/_row.html` alone: the content of a live update.
/// It is rendered without a request and reaches every subscriber, so it
/// holds nothing specific to one viewer (no CSRF token).
#[derive(Template)]
#[template(path = "events/_row.html")]
pub struct EventRowFragment {
    pub row: EventRow,
}

/// `templates/events/show.html`, with the delete form.
#[derive(Template)]
#[template(path = "events/show.html")]
pub struct EventShow<'a> {
    pub layout: Layout<'a>,
    pub csrf_field: String,
    pub back_path: String,
    pub version: i64,
    pub event: EventRow,
}

/// `templates/events/new.html`, around `_form.html`.
#[derive(Template)]
#[template(path = "events/new.html")]
pub struct EventNew<'a> {
    pub layout: Layout<'a>,
    pub csrf_field: String,
    pub action: String,
    /// `_method` for the form (`None` for a plain `POST`).
    pub method: Option<&'static str>,
    /// The optimistic-locking version the form was rendered with.
    pub version: Option<i64>,
    pub submit: &'static str,
    pub back_path: String,
    pub form: EventFormValues,
    pub errors: FieldErrors,
}

/// `templates/events/edit.html`, around `_form.html`.
#[derive(Template)]
#[template(path = "events/edit.html")]
pub struct EventEdit<'a> {
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
    pub form: EventFormValues,
    pub errors: FieldErrors,
}

/// `templates/events/not_found.html`, answered with `404`.
#[derive(Template)]
#[template(path = "events/not_found.html")]
pub struct EventNotFound<'a> {
    pub layout: Layout<'a>,
    pub back_path: String,
}
