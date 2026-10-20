//! Message bodies rendered from askama templates.

use askama::DynTemplate;

use crate::{MailError, Message};

/// The parts of one email, each an askama template: an optional subject, a
/// `text/plain` body and a `text/html` body (at least one of the two).
///
/// Templates are compile-time checked structs, so their data is a typed
/// contract between the caller and the template. Give the HTML template an
/// `.html` file (or `ext = "html"`) so askama escapes every value; text and
/// subject templates use `.txt`, where nothing is escaped, which is right for
/// plain text. A rendered subject is still validated as a header when the
/// message is encoded, so template data cannot inject headers.
///
/// Rendering is buffered: if any part fails, [`Templates::message`] returns
/// [`MailError::Template`] and nothing is sent.
///
/// ```
/// use askama::Template;
/// use forge_mail::{Address, Message, Templates};
///
/// #[derive(Template)]
/// #[template(source = "Welcome, {{ name }}!", ext = "txt")]
/// struct WelcomeText<'a> { name: &'a str }
///
/// #[derive(Template)]
/// #[template(source = "<p>Welcome, {{ name }}!</p>", ext = "html")]
/// struct WelcomeHtml<'a> { name: &'a str }
///
/// let base = Message::new(Address::new("App", "app@example.com")?)
///     .to(Address::new("", "ada@example.org")?)
///     .subject("Welcome");
/// let message = Templates::new()
///     .text(&WelcomeText { name: "<Ada>" })
///     .html(&WelcomeHtml { name: "<Ada>" })
///     .message(base)?;
/// assert_eq!(message.text, "Welcome, <Ada>!");
/// assert_eq!(message.html, "<p>Welcome, &#60;Ada&#62;!</p>");
/// # Ok::<(), forge_mail::MailError>(())
/// ```
#[derive(Clone, Copy, Default)]
pub struct Templates<'a> {
    subject: Option<&'a dyn DynTemplate>,
    text: Option<&'a dyn DynTemplate>,
    html: Option<&'a dyn DynTemplate>,
}

impl std::fmt::Debug for Templates<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Templates")
            .field("subject", &self.subject.is_some())
            .field("text", &self.text.is_some())
            .field("html", &self.html.is_some())
            .finish()
    }
}

/// The rendered parts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rendered {
    /// The subject, trimmed; empty without a subject template.
    pub subject: String,
    /// The `text/plain` body; empty without a text template.
    pub text: String,
    /// The `text/html` body; empty without an HTML template.
    pub html: String,
}

impl Rendered {
    /// Fills a copy of `base`: text and HTML from the rendered parts, and
    /// the subject when one was rendered and `base` has none. Addresses,
    /// attachments and headers come from `base`.
    #[must_use]
    pub fn apply(self, mut base: Message) -> Message {
        if !self.subject.is_empty() && base.subject.is_empty() {
            base.subject = self.subject;
        }
        if !self.text.is_empty() {
            base.text = self.text;
        }
        if !self.html.is_empty() {
            base.html = self.html;
        }
        base
    }
}

impl<'a> Templates<'a> {
    /// No parts yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The subject template (one line; surrounding whitespace is trimmed).
    #[must_use]
    pub fn subject(mut self, template: &'a dyn DynTemplate) -> Self {
        self.subject = Some(template);
        self
    }

    /// The `text/plain` template.
    #[must_use]
    pub fn text(mut self, template: &'a dyn DynTemplate) -> Self {
        self.text = Some(template);
        self
    }

    /// The `text/html` template; use an HTML-escaping template.
    #[must_use]
    pub fn html(mut self, template: &'a dyn DynTemplate) -> Self {
        self.html = Some(template);
        self
    }

    /// Renders every part into memory. Any failure returns
    /// [`MailError::Template`] naming the part, and no output.
    pub fn render(&self) -> Result<Rendered, MailError> {
        if self.text.is_none() && self.html.is_none() {
            return Err(MailError::Template { part: "body" });
        }
        let render = |template: Option<&dyn DynTemplate>, part| {
            template.map_or_else(
                || Ok(String::new()),
                |template| {
                    template
                        .dyn_render()
                        .map_err(|_| MailError::Template { part })
                },
            )
        };
        Ok(Rendered {
            subject: render(self.subject, "subject")?.trim().to_owned(),
            text: render(self.text, "text")?,
            html: render(self.html, "html")?,
        })
    }

    /// Renders the parts into a copy of `base` (see [`Rendered::apply`]).
    pub fn message(&self, base: Message) -> Result<Message, MailError> {
        Ok(self.render()?.apply(base))
    }
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::*;
    use crate::Address;

    #[derive(Template)]
    #[template(path = "tests/welcome.html")]
    struct Html<'a> {
        name: &'a str,
        url: &'a str,
    }

    #[derive(Template)]
    #[template(path = "tests/welcome.txt")]
    struct Text<'a> {
        name: &'a str,
    }

    #[derive(Template)]
    #[template(source = "  Welcome {{ name }}\n", ext = "txt")]
    struct Subject<'a> {
        name: &'a str,
    }

    struct Failing;

    impl std::fmt::Display for Failing {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            Err(std::fmt::Error)
        }
    }

    #[derive(Template)]
    #[template(source = "{{ value }}", ext = "txt")]
    struct Broken {
        value: Failing,
    }

    fn base() -> Message {
        Message::new(Address::new("App", "app@example.com").expect("valid")).to(Address::new(
            "",
            "ada@example.org",
        )
        .expect("valid"))
    }

    #[test]
    fn html_is_escaped_and_text_is_not() {
        let hostile = "<script>alert(1)</script>\"'&";
        let message = Templates::new()
            .subject(&Subject { name: "Ada" })
            .text(&Text { name: hostile })
            .html(&Html {
                name: hostile,
                url: "javascript:alert(1)\" onclick=\"x",
            })
            .message(base())
            .expect("rendered");
        assert_eq!(message.subject, "Welcome Ada");
        assert!(message.text.contains(hostile));
        assert!(!message.html.contains("<script>"), "{}", message.html);
        assert!(
            message.html.contains("&#60;script&#62;"),
            "{}",
            message.html
        );
        assert!(!message.html.contains("\" onclick"), "{}", message.html);
        assert!(message.validate().is_ok());
    }

    #[test]
    fn subject_injection_through_template_data_is_refused_at_encoding() {
        let message = Templates::new()
            .subject(&Subject {
                name: "Ada\r\nBcc: eve@example.com",
            })
            .text(&Text { name: "Ada" })
            .message(base())
            .expect("rendered");
        assert_eq!(
            message.validate().expect_err("injection").code(),
            "mail_invalid"
        );
    }

    #[test]
    fn a_failing_part_renders_nothing() {
        let error = Templates::new()
            .text(&Text { name: "Ada" })
            .html(&Broken { value: Failing })
            .render()
            .expect_err("broken");
        assert_eq!(error, MailError::Template { part: "html" });
        assert_eq!(
            Templates::new().subject(&Subject { name: "x" }).render(),
            Err(MailError::Template { part: "body" })
        );
    }

    #[test]
    fn an_explicit_subject_wins() {
        let message = Templates::new()
            .subject(&Subject { name: "Ada" })
            .text(&Text { name: "Ada" })
            .message(base().subject("Chosen"))
            .expect("rendered");
        assert_eq!(message.subject, "Chosen");
    }
}
