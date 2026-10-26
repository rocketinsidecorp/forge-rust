//! Browser previews of emails during development, the counterpart of Rails'
//! `ActionMailer::Preview` and Forge-Go's `mail.NewPreviewHandler`.

use std::{error::Error, fmt, sync::Arc};

use askama::Template;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use forge_config::{AppConfig, Environment};
use forge_http::{
    BoxResponseFuture, HeaderValue, Method, Middleware, Next, Request, Response, StatusCode, header,
};

use sha2::{Digest as _, Sha256};

use crate::{Encoded, MailError, Message, encode, message::attachment_content_type};

type BuildFn = dyn Fn() -> Result<Message, Box<dyn Error + Send + Sync>> + Send + Sync;

/// One email an application can render in the browser with fixture data,
/// without sending anything.
#[derive(Clone)]
pub struct Preview {
    name: String,
    description: String,
    build: Arc<BuildFn>,
}

impl fmt::Debug for Preview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Preview")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

impl Preview {
    /// A preview called `name` (the URL segment: lowercase letters, digits,
    /// `-` and `_`, at most 64) whose `build` returns the message. `build`
    /// must use fixture data and must not send mail or touch production
    /// services.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        build: impl Fn() -> Result<Message, Box<dyn Error + Send + Sync>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            build: Arc::new(build),
        }
    }

    /// The URL segment.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn valid_preview_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

/// What a preview page's policy allows besides its own stylesheet. The page
/// has one inline `<style>` element and no attribute styles, so
/// [`page_policy`] admits it by SHA-256 hash instead of `'unsafe-inline'`.
const PAGE_CSP_REST: &str = "frame-src 'self'; base-uri 'none'; form-action 'none'";
/// The policy of a preview page whose stylesheet could not be hashed: it is
/// shown unstyled rather than with `'unsafe-inline'`.
const PAGE_CSP_UNSTYLED: &str =
    "default-src 'none'; frame-src 'self'; base-uri 'none'; form-action 'none'";
/// The policy of the HTML part of the message, shown in the sandboxed frame.
/// It keeps `style-src 'unsafe-inline'`: a mailer's HTML carries its own
/// `style` attributes and `<style>` blocks (email clients need them), which
/// cannot be known, so cannot be hashed. The frame can still run no script,
/// load nothing but its own inline images and submit no form.
const HTML_PART_CSP: &str =
    "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox";
/// Every other response (text part, raw source, inline attachments, errors):
/// nothing is rendered with style, so it allows none.
const PLAIN_CSP: &str = "default-src 'none'; sandbox";

/// The CSP source (`'sha256-...'`) of the one `<style>` element of `page`:
/// the hash of the exact bytes between the tags, which is what browsers
/// compare. `None` unless the page holds exactly one such element: every
/// user-supplied value in the page is HTML-escaped, so a second one would be
/// a template change that needs a new policy.
fn inline_style_source(page: &str) -> Option<String> {
    let (_, rest) = page.split_once("<style>")?;
    let (style, after) = rest.split_once("</style>")?;
    if after.contains("<style") {
        return None;
    }
    let digest = Sha256::digest(style.as_bytes());
    Some(format!("'sha256-{}'", STANDARD.encode(digest)))
}

/// The policy of the preview `page`: its stylesheet by hash, nothing inline
/// otherwise.
fn page_policy(page: &str) -> HeaderValue {
    inline_style_source(page)
        .and_then(|source| {
            HeaderValue::from_str(&format!(
                "default-src 'none'; style-src {source}; {PAGE_CSP_REST}"
            ))
            .ok()
        })
        .unwrap_or_else(|| HeaderValue::from_static(PAGE_CSP_UNSTYLED))
}

/// Serves registered [`Preview`]s below a prefix such as `/_mail`: an index
/// and, per preview, the headers, the HTML part in a sandboxed frame, the
/// text part, the inline images and the raw MIME source. Messages go through
/// the real encoder, so the page shows exactly the validation error
/// production would hit.
///
/// It is for development and takes two independent switches, both required:
/// `FORGE_ENV` must be `development` or `test`, **and**
/// `FORGE_MAIL_PREVIEW=true` must be set. The second one matters because an
/// unset `FORGE_ENV` means development: a production container that lost its
/// environment would otherwise serve previews. Configuration loading refuses
/// `FORGE_MAIL_PREVIEW` in production.
///
/// Register it as a layer (`router.layer(handler)`); it answers `GET` and
/// `HEAD` for its index and its previews below the prefix (whole path
/// segments only) and passes every other path on, so handlers with nested
/// prefixes (`/_mail`, `/_mail/user`) can be layered in any order. HTML is served under `Content-Security-Policy: ...; sandbox` and
/// inside a sandboxed iframe, so even hostile template data cannot run script
/// in the developer's browser; every response is `nosniff` and `no-store`.
pub struct PreviewHandler {
    prefix: String,
    previews: Vec<Preview>,
}

impl fmt::Debug for PreviewHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreviewHandler")
            .field("prefix", &self.prefix)
            .field("previews", &self.previews.len())
            .finish()
    }
}

impl PreviewHandler {
    /// Checks the two switches, the prefix and the preview names.
    pub fn new(
        config: &AppConfig,
        prefix: &str,
        previews: Vec<Preview>,
    ) -> Result<Self, MailError> {
        let refuse = |message: &str| Err(MailError::Configuration(message.to_owned()));
        if config.environment == Environment::Production {
            return refuse("mail previews are available only in development and test");
        }
        if !config.mail.preview {
            return refuse("mail previews need FORGE_MAIL_PREVIEW=true (explicit opt-in)");
        }
        if !prefix.starts_with('/')
            || prefix.ends_with('/')
            || prefix.starts_with("//")
            || prefix.contains(['?', '#', '\\', ' ', '%'])
        {
            return refuse("mail preview prefix must look like \"/_mail\"");
        }
        let mut seen = std::collections::HashSet::new();
        for preview in &previews {
            if !valid_preview_name(&preview.name) {
                return refuse("mail preview names must match [a-z0-9][a-z0-9_-]*");
            }
            if !seen.insert(preview.name.as_str()) {
                return refuse("duplicate mail preview name");
            }
        }
        Ok(Self {
            prefix: prefix.to_owned(),
            previews,
        })
    }

    /// Answers `request` when its path is the prefix (the index) or names
    /// one of the handler's previews below it. Other paths, including other
    /// segments below the prefix, are left to the next layer, so handlers
    /// nest: `/_mail` with the password reset preview and `/_mail/user` with
    /// a generated mailer's previews work in either order.
    pub fn serve(&self, request: &Request) -> Option<Response> {
        let rest = request.uri().path().strip_prefix(self.prefix.as_str())?;
        // The prefix must end at a path segment: `/_mailx` is not below it.
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        let first = rest.trim_start_matches('/').split('/').next().unwrap_or("");
        if !first.is_empty() && !self.previews.iter().any(|preview| preview.name == first) {
            return None;
        }
        let response = if request.method() != Method::GET && request.method() != Method::HEAD {
            let mut response = Response::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            secure(&mut response, HeaderValue::from_static(PLAIN_CSP));
            response
        } else {
            self.respond(rest)
        };
        Some(if request.method() == Method::HEAD {
            without_body(response)
        } else {
            response
        })
    }

    fn respond(&self, rest: &str) -> Response {
        let segments: Vec<&str> = rest.trim_matches('/').split('/').collect();
        if rest.is_empty() || rest == "/" {
            return self.render(StatusCode::OK, &self.index_view());
        }
        let Some(preview) = self
            .previews
            .iter()
            .find(|preview| preview.name == segments[0])
        else {
            return not_found();
        };
        let built = (preview.build)();
        let (message, encoded, error) = match built {
            Err(error) => (None, None, Some(format!("building the preview: {error}"))),
            Ok(message) => match encode(&message) {
                Ok(encoded) => (Some(message), Some(encoded), None),
                Err(error) => (Some(message), None, Some(error.to_string())),
            },
        };
        match (segments.as_slice(), &message, &encoded) {
            ([_], _, _) => {
                let status = if error.is_some() {
                    StatusCode::UNPROCESSABLE_ENTITY
                } else {
                    StatusCode::OK
                };
                let view = self.page_view(preview, message.as_ref(), encoded.as_ref(), error);
                self.render(status, &view)
            }
            (_, _, None) => text_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "text/plain; charset=utf-8",
                b"this preview does not build; see the preview page".to_vec(),
            ),
            ([_, "html"], Some(message), Some(_)) if !message.html.is_empty() => {
                let mut body = message.html.clone();
                for attachment in &message.attachments {
                    if !attachment.content_id.is_empty() {
                        body = body.replace(
                            &format!("cid:{}", attachment.content_id),
                            &format!(
                                "{}/{}/cid/{}",
                                self.prefix, preview.name, attachment.content_id
                            ),
                        );
                    }
                }
                content("text/html; charset=utf-8", body.into_bytes())
            }
            ([_, "text"], Some(message), Some(_)) if !message.text.is_empty() => content(
                "text/plain; charset=utf-8",
                message.text.clone().into_bytes(),
            ),
            ([_, "raw"], _, Some(encoded)) => {
                content("text/plain; charset=utf-8", encoded.data.clone())
            }
            ([_, "cid", id], Some(message), Some(_)) => {
                let id = percent_decode(id);
                message
                    .attachments
                    .iter()
                    .find(|attachment| {
                        !attachment.content_id.is_empty() && attachment.content_id == id
                    })
                    .map_or_else(not_found, |attachment| {
                        let content_type = attachment_content_type(&attachment.content_type)
                            .unwrap_or_else(|| "application/octet-stream".to_owned());
                        content(&content_type, attachment.data.clone())
                    })
            }
            _ => not_found(),
        }
    }

    fn index_view(&self) -> PreviewPage {
        PreviewPage {
            index: true,
            prefix: self.prefix.clone(),
            items: self
                .previews
                .iter()
                .map(|preview| (preview.name.clone(), preview.description.clone()))
                .collect(),
            ..PreviewPage::default()
        }
    }

    fn page_view(
        &self,
        preview: &Preview,
        message: Option<&Message>,
        encoded: Option<&Encoded>,
        error: Option<String>,
    ) -> PreviewPage {
        let mut view = PreviewPage {
            prefix: self.prefix.clone(),
            name: preview.name.clone(),
            error: error.unwrap_or_default(),
            ..PreviewPage::default()
        };
        if let Some(message) = message {
            view.has_message = true;
            view.from = join(std::slice::from_ref(&message.from));
            view.reply_to = join(&message.reply_to);
            view.to = join(&message.to);
            view.cc = join(&message.cc);
            view.bcc = join(&message.bcc);
            view.subject = message.subject.clone();
            view.text = message.text.clone();
            view.has_html = encoded.is_some() && !message.html.is_empty();
            view.message_id = encoded
                .map(|encoded| encoded.message_id.clone())
                .unwrap_or_default();
            view.attachments = message
                .attachments
                .iter()
                .map(|attachment| {
                    let mut label =
                        format!("{} ({} bytes)", attachment.filename, attachment.data.len());
                    if !attachment.content_id.is_empty() {
                        label.push_str(&format!(", inline as cid:{}", attachment.content_id));
                    }
                    label
                })
                .collect();
        }
        view
    }

    fn render(&self, status: StatusCode, view: &PreviewPage) -> Response {
        match view.render() {
            Ok(html) => {
                let policy = page_policy(&html);
                let mut response = Response::new(status, html);
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                );
                secure(&mut response, policy);
                response
            }
            Err(_) => text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain; charset=utf-8",
                b"preview page failed".to_vec(),
            ),
        }
    }
}

impl Middleware for PreviewHandler {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        Box::pin(async move {
            match self.serve(&request) {
                Some(response) => response,
                None => next.run(request).await,
            }
        })
    }
}

fn join(list: &[crate::Address]) -> String {
    list.iter()
        .map(|address| {
            if address.name.is_empty() {
                address.email.clone()
            } else {
                format!("{} <{}>", address.name, address.email)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let decoded = (bytes[index] == b'%')
            .then(|| {
                let high = char::from(*bytes.get(index + 1)?).to_digit(16)?;
                let low = char::from(*bytes.get(index + 2)?).to_digit(16)?;
                u8::try_from(high * 16 + low).ok()
            })
            .flatten();
        match decoded {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The headers every preview response carries. Previews never need script.
fn secure(response: &mut Response, csp: HeaderValue) {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
}

fn text_response(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response {
    let mut response = Response::new(status, body);
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    let csp = if content_type.starts_with("text/html") {
        HTML_PART_CSP
    } else {
        PLAIN_CSP
    };
    secure(&mut response, HeaderValue::from_static(csp));
    response
}

fn content(content_type: &str, body: Vec<u8>) -> Response {
    text_response(StatusCode::OK, content_type, body)
}

fn not_found() -> Response {
    text_response(
        StatusCode::NOT_FOUND,
        "text/plain; charset=utf-8",
        b"not found".to_vec(),
    )
}

fn without_body(response: Response) -> Response {
    let mut empty = Response::new(response.status(), Vec::new());
    *empty.headers_mut() = response.headers().clone();
    empty
}

#[derive(Default, Template)]
#[template(
    ext = "html",
    source = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Mail preview</title>
<style>
body{font:15px/1.5 system-ui,sans-serif;margin:0;color:#1b1f24;background:#f6f7f9}
main{max-width:60rem;margin:0 auto;padding:1.5rem 1rem}
a{color:#0b57d0}
h1{font-size:1.25rem;margin:.25rem 0 1rem}
table{border-collapse:collapse;background:#fff;width:100%;margin-bottom:1rem}
th,td{text-align:left;padding:.4rem .75rem;border-bottom:1px solid #e3e6ea;vertical-align:top}
th{width:9rem;color:#57606a;font-weight:600}
pre{background:#fff;border:1px solid #e3e6ea;padding:1rem;overflow:auto;white-space:pre-wrap}
iframe{width:100%;height:32rem;border:1px solid #e3e6ea;background:#fff}
.err{background:#fff1f0;border:1px solid #f4b4ae;padding:.75rem 1rem;border-radius:.375rem}
ul{padding-left:1.25rem}
</style></head><body><main>
{% if index %}
<h1>Mail previews</h1>
{% if items.is_empty() %}<p>No previews are registered.</p>
{% else %}<ul>{% for (name, description) in items %}<li><a href="{{ prefix }}/{{ name }}">{{ name }}</a>{% if !description.is_empty() %} &mdash; {{ description }}{% endif %}</li>{% endfor %}</ul>{% endif %}
{% else %}
<p><a href="{{ prefix }}/">&larr; All previews</a></p>
<h1>{{ name }}</h1>
{% if !error.is_empty() %}<p class="err"><strong>This message would not be sent:</strong> {{ error }}</p>{% endif %}
{% if has_message %}
<table>
<tr><th>From</th><td>{{ from }}</td></tr>
{% if !reply_to.is_empty() %}<tr><th>Reply-To</th><td>{{ reply_to }}</td></tr>{% endif %}
<tr><th>To</th><td>{{ to }}</td></tr>
{% if !cc.is_empty() %}<tr><th>Cc</th><td>{{ cc }}</td></tr>{% endif %}
{% if !bcc.is_empty() %}<tr><th>Bcc (envelope only)</th><td>{{ bcc }}</td></tr>{% endif %}
<tr><th>Subject</th><td>{{ subject }}</td></tr>
{% if !message_id.is_empty() %}<tr><th>Message-ID</th><td>{{ message_id }}</td></tr>{% endif %}
{% for attachment in attachments %}<tr><th>Attachment</th><td>{{ attachment }}</td></tr>{% endfor %}
</table>
{% if has_html %}<h2>HTML</h2><iframe sandbox src="{{ prefix }}/{{ name }}/html" title="HTML part"></iframe>{% endif %}
{% if !text.is_empty() %}<h2>Text</h2><pre>{{ text }}</pre>{% endif %}
{% if !message_id.is_empty() %}<p><a href="{{ prefix }}/{{ name }}/raw">Raw MIME source</a></p>{% endif %}
{% endif %}
{% endif %}
</main></body></html>"#
)]
struct PreviewPage {
    index: bool,
    prefix: String,
    items: Vec<(String, String)>,
    name: String,
    error: String,
    has_message: bool,
    from: String,
    reply_to: String,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    message_id: String,
    attachments: Vec<String>,
    has_html: bool,
    text: String,
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use forge_http::{HeaderMap, Uri};

    use super::*;
    use crate::{Address, Attachment};

    fn config(environment: Environment, preview: bool) -> AppConfig {
        let mut config = AppConfig {
            environment,
            ..AppConfig::default()
        };
        config.mail.preview = preview;
        config
    }

    fn welcome() -> Preview {
        Preview::new("welcome", "After <sign-up>", || {
            Ok(Message::new(Address::new("App", "app@example.com")?)
                .to(Address::new("Ada <script>", "ada@example.org")?)
                .bcc(Address::new("", "audit@example.org")?)
                .subject("Welcome <b>")
                .text("Hello <Ada>")
                .html("<p>Hello</p><img src=\"cid:logo\"><script>alert(1)</script>")
                .attach(Attachment::new("logo.png", "image/png", vec![0x89, b'P']).inline("logo")))
        })
    }

    fn broken() -> Preview {
        Preview::new("broken", "", || {
            let mut message = Message::new(Address::new("App", "app@example.com")?)
                .to(Address::new("", "ada@example.org")?)
                .text("x");
            message.subject = "Hi\r\nBcc: eve@example.com".to_owned();
            Ok(message)
        })
    }

    fn handler() -> PreviewHandler {
        PreviewHandler::new(
            &config(Environment::Development, true),
            "/_mail",
            vec![welcome(), broken()],
        )
        .expect("development")
    }

    fn request(method: Method, path: &str) -> Request {
        Request::new_for_test(
            method,
            path.parse::<Uri>().expect("uri"),
            HeaderMap::new(),
            Vec::new().into(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 9)),
            uuid::Uuid::nil(),
        )
    }

    fn get(path: &str) -> Option<Response> {
        handler().serve(&request(Method::GET, path))
    }

    fn body(response: &Response) -> String {
        String::from_utf8_lossy(response.body()).into_owned()
    }

    fn header_of(response: &Response, name: header::HeaderName) -> String {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn previews_need_development_or_test_and_the_explicit_flag() {
        for (environment, flag) in [
            (Environment::Production, true),
            (Environment::Production, false),
            (Environment::Development, false),
            (Environment::Test, false),
        ] {
            assert!(
                PreviewHandler::new(&config(environment, flag), "/_mail", vec![welcome()]).is_err(),
                "{environment:?} {flag}"
            );
        }
        assert!(
            PreviewHandler::new(&config(Environment::Test, true), "/_mail", vec![welcome()])
                .is_ok()
        );
        let dev = config(Environment::Development, true);
        for prefix in ["_mail", "/_mail/", "/_m ail", "//mail", "/_mail?x"] {
            assert!(
                PreviewHandler::new(&dev, prefix, Vec::new()).is_err(),
                "{prefix}"
            );
        }
        assert!(PreviewHandler::new(&dev, "/_mail", vec![welcome(), welcome()]).is_err());
        let bad_name = Preview::new("Welcome!", "", || Err("unused".into()));
        assert!(PreviewHandler::new(&dev, "/_mail", vec![bad_name]).is_err());
    }

    #[test]
    fn the_index_lists_previews_escaped() {
        let response = get("/_mail").expect("served");
        assert_eq!(response.status(), StatusCode::OK);
        let html = body(&response);
        assert!(html.contains("href=\"/_mail/welcome\""), "{html}");
        assert!(html.contains("After &#60;sign-up&#62;"), "{html}");
        assert!(
            header_of(&response, header::CONTENT_SECURITY_POLICY).contains("default-src 'none'")
        );
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
        assert_eq!(
            header_of(&response, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert_eq!(get("/_mail/").expect("served").status(), StatusCode::OK);
    }

    #[test]
    fn a_preview_page_escapes_everything_and_frames_html_in_a_sandbox() {
        let response = get("/_mail/welcome").expect("served");
        assert_eq!(response.status(), StatusCode::OK);
        let html = body(&response);
        assert!(
            html.contains("<iframe sandbox src=\"/_mail/welcome/html\""),
            "{html}"
        );
        assert!(html.contains("Ada &#60;script&#62;"), "{html}");
        assert!(html.contains("Welcome &#60;b&#62;"), "{html}");
        assert!(html.contains("Bcc (envelope only)"), "{html}");
        assert!(
            html.contains("logo.png (2 bytes), inline as cid:logo"),
            "{html}"
        );
        assert!(!html.contains("<script>"), "{html}");

        let frame = get("/_mail/welcome/html").expect("served");
        let csp = header_of(&frame, header::CONTENT_SECURITY_POLICY);
        assert!(csp.ends_with("sandbox"), "{csp}");
        // Remote images and fonts would phone home from the browser.
        assert!(csp.contains("img-src 'self' data:;"), "{csp}");
        assert!(!csp.contains("http:") && !csp.contains("https:"), "{csp}");
        assert!(!csp.contains("font-src"), "{csp}");
        assert!(body(&frame).contains("src=\"/_mail/welcome/cid/logo\""));
        let image = get("/_mail/welcome/cid/logo").expect("served");
        assert_eq!(header_of(&image, header::CONTENT_TYPE), "image/png");
        assert_eq!(image.body().as_ref(), [0x89, b'P']);
        assert_eq!(
            body(&get("/_mail/welcome/text").expect("served")),
            "Hello <Ada>"
        );
        let raw = body(&get("/_mail/welcome/raw").expect("served"));
        assert!(raw.starts_with("Date: "), "{raw}");
        assert!(!raw.contains("audit@"), "{raw}");
    }

    #[test]
    fn a_message_production_would_refuse_shows_the_reason() {
        let response = get("/_mail/broken").expect("served");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body(&response).contains("This message would not be sent"));
        assert!(body(&response).contains("Subject"));
        assert_eq!(
            get("/_mail/broken/raw").expect("served").status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn routing_is_by_whole_segments_and_read_only() {
        assert!(get("/_mailwelcome").is_none());
        assert!(get("/other").is_none());
        // A segment that names no preview is another handler's (nesting).
        assert!(get("/_mail/missing").is_none());
        assert!(get("/_mail/user/welcome").is_none());
        for path in ["/_mail/welcome/nope", "/_mail/welcome/cid/other"] {
            assert_eq!(
                get(path).expect("served").status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
        let post = handler()
            .serve(&request(Method::POST, "/_mail/welcome"))
            .expect("served");
        assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header_of(&post, header::ALLOW), "GET, HEAD");
        let head = handler()
            .serve(&request(Method::HEAD, "/_mail/welcome"))
            .expect("served");
        assert_eq!(head.status(), StatusCode::OK);
        assert!(head.body().is_empty());
    }

    /// The SHA-256 of the stylesheet of a served `page`, recomputed the way a
    /// browser does: the bytes between `<style>` and `</style>`.
    fn served_style_source(page: &str) -> String {
        assert_eq!(page.matches("<style").count(), 1, "{page}");
        assert!(!page.contains(" style="), "{page}");
        let start = page.find("<style>").expect("a style element") + "<style>".len();
        let end = start + page[start..].find("</style>").expect("closed");
        let style = page.get(start..end).expect("on char boundaries");
        format!("'sha256-{}'", STANDARD.encode(Sha256::digest(style)))
    }

    #[test]
    fn preview_pages_allow_their_stylesheet_by_hash_not_unsafe_inline() {
        for path in ["/_mail", "/_mail/welcome", "/_mail/broken"] {
            let response = get(path).expect("served");
            let csp = header_of(&response, header::CONTENT_SECURITY_POLICY);
            assert_eq!(
                csp,
                format!(
                    "default-src 'none'; style-src {}; frame-src 'self'; base-uri 'none'; \
                     form-action 'none'",
                    served_style_source(&body(&response))
                ),
                "{path}"
            );
            assert!(!csp.contains("unsafe-inline"), "{path}: {csp}");
        }
    }

    #[test]
    fn the_style_source_is_the_sha256_of_the_exact_bytes_between_the_tags() {
        // `printf 'a{}' | openssl dgst -sha256 -binary | base64`
        assert_eq!(
            inline_style_source("<head><style>a{}</style></head>").as_deref(),
            Some("'sha256-X1RutGBrXCt9KkSaXMK7tHftWiRscFHOhxsS8tv8hBk='")
        );
        // Whitespace is part of the hashed text.
        assert_ne!(
            inline_style_source("<style> a{} </style>"),
            inline_style_source("<style>a{}</style>")
        );
        // No stylesheet or more than one: nothing is allowed, never inline.
        for page in [
            "<p>no style</p>",
            "<style>a{}",
            "<style>a{}</style><style>b{}</style>",
        ] {
            assert_eq!(inline_style_source(page), None, "{page}");
            let policy = page_policy(page);
            let policy = policy.to_str().expect("ascii");
            assert!(!policy.contains("style-src"), "{policy}");
            assert!(!policy.contains("unsafe-inline"), "{policy}");
        }
    }

    /// The one use that cannot be hashed: a message's own HTML, whose
    /// `style` attributes and `<style>` blocks are the mailer's. Everything
    /// else allows no style at all.
    #[test]
    fn only_the_html_part_of_a_message_keeps_unsafe_inline() {
        let csp =
            |path: &str| header_of(&get(path).expect("served"), header::CONTENT_SECURITY_POLICY);
        assert!(csp("/_mail/welcome/html").contains("style-src 'unsafe-inline'"));
        for path in [
            "/_mail/welcome/text",
            "/_mail/welcome/raw",
            "/_mail/welcome/cid/logo",
            "/_mail/welcome/nope",
        ] {
            assert_eq!(csp(path), "default-src 'none'; sandbox", "{path}");
        }
        let post = handler()
            .serve(&request(Method::POST, "/_mail/welcome"))
            .expect("served");
        assert_eq!(
            header_of(&post, header::CONTENT_SECURITY_POLICY),
            "default-src 'none'; sandbox"
        );
    }
}
