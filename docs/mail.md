# Mail

`forge-mail` (re-exported as `forge::mail`) sends email, the counterpart of
Rails' Action Mailer and the port of Forge-Go's `mail` package. Messages are
validated data, delivery goes through the `Sender` port, and the defaults are
the safe ones. The design is in [ADR 0035](adr/0035-mail.md).

```toml
# Cargo.toml of the application: `views` (every full-stack app) includes it
forge = { version = "0.1", features = ["mail"] }
```

## Configuration

The variables are Forge-Go's, with the same rules.

| Variable | Default | Meaning |
| --- | --- | --- |
| `FORGE_MAIL_URL` | unset (mail disabled) | `smtp://user:password@host:587` (STARTTLS), `smtps://user:password@host:465` (implicit TLS) or `file:tmp/mail` / `file:///var/tmp/mail` (development `.eml` files, refused in production). Percent-encode `@`, `:`, `/` and `#` in the password. Redacted in diagnostics; takes no parameters. |
| `FORGE_MAIL_FROM` | required with a URL | Default sender, `app@example.com` or `App <app@example.com>`, checked with the same rules as every message. |
| `FORGE_MAIL_ALLOW_PLAINTEXT` | `false` | `true` lets the SMTP sender continue when the server offers no STARTTLS. It never sends the password in clear: AUTH without TLS goes only to `localhost`, 127.0.0.0/8 or `::1` (a local Mailpit); to any other host the send fails. Production refuses it with credentials for a remote host; without credentials it is accepted but logs a warning at start-up, because nothing then stops a network attacker from stripping `STARTTLS` (the mail itself travels in clear). |
| `FORGE_MAIL_TIMEOUT` | `30s` | Deadline of one delivery attempt, at most `10m` (Go duration syntax). |
| `FORGE_MAIL_PREVIEW` | `false` | `true` enables the preview handler; refused in production. |
| `FORGE_MAIL_TLS_CA_FILE` | unset | PEM roots trusted in addition to the Mozilla roots, for a private relay. There is no setting to skip certificate verification. |
| `FORGE_AUTH_PASSWORD_RESET_URL` | unset | Absolute URL of the reset page (`https://app.example.com/passwords/edit`; http only on localhost), without credentials, query or fragment. Reset emails need it. |

```bash
export FORGE_MAIL_URL='smtps://mailer:s3cr%40t@smtp.example.com:465'
export FORGE_MAIL_FROM='Shop <no-reply@example.com>'
export FORGE_AUTH_PASSWORD_RESET_URL=https://shop.example.com/passwords/edit
cargo run
```

In bootstrap code:

```rust
use forge::mail::{MailError, default_from, from_config, instrument};

let config = composition.config();
let sender = match from_config(config) {
    Err(MailError::NotConfigured) => None, // leave features that need mail disabled
    sender => Some(instrument(sender?, composition.telemetry())?),
};
let from = default_from(config)?;
```

## Sending

```rust
use forge::mail::{Address, Attachment, Message, Sender};

let message = Message::new(from.clone())
    .to(Address::new("Ada", "ada@example.org")?)
    .subject("Your receipt")
    .text("Thanks for your order.")
    .html("<p>Thanks for your order.</p>")
    .attach(Attachment::new("receipt.pdf", "application/pdf", pdf_bytes))
    .header("List-Unsubscribe", "<https://example.com/u/42>");
sender.send(&message).await?;
```

Application code depends on `Arc<dyn Sender>`; bootstrap chooses the
implementation. Everything user-controlled is validated before a byte leaves
the process: control characters, line breaks and direction overrides are
rejected in every header, address and file name; addresses must be plain
ASCII `local@domain`; extra headers come from an allow-list
(`forge::mail::ALLOWED_HEADERS`); recipients, sizes and attachments are
bounded. Errors name the field, never the value. `Bcc` recipients get the
message but appear in no header.

Header text is encoded so that it reads back exactly as written: a non-ASCII
display name is base64 encoded words (Q-encoding would leave a `\` or `"`
raw, which an address header does not allow); a subject or name containing
`=?` (which a client would decode as a hand-written encoded word) or a run of
spaces is base64 encoded words of the text without its outer spaces (folding
would otherwise collapse the run); other non-ASCII subjects are Q-encoded.
An attachment's media type is at most 127 bytes, so a part header never
approaches the 998-byte line limit. These are the rules of Forge-Go's
encoder, byte for byte.

`MailError::code()` is Forge-Go's fault code: `mail_invalid`,
`mail_address_unsupported`, `mail_unavailable` (retryable:
`is_retryable()`), `mail_rejected`, `mail_auth_failed`, `mail_tls_required`,
`mail_tls_failed`, `mail_template`, `mail_file_failed`. Errors carry the SMTP
stage and reply code (`stage()`, `reply_code()`), never credentials,
recipients or server reply text.

## Limits and known limitations

`Limits::default()`: subject 255 characters, 100 recipients, 2 MiB per body,
10 attachments of at most 7 MiB, 10 MiB of content in total (about 13.7 MB
once base64 encoded). Raise them with `Encoder::new().limits(...)`; messages
are held in memory while encoding, so send large files as links.

Addresses must be ASCII: Forge supports neither SMTPUTF8 nor IDNA. An address
such as `joão@exemplo.com` is rejected with `mail_address_unsupported`, never
converted. Display names may contain any Unicode text.

The SMTP sender opens one connection per message (no pooling) and trusts the
Mozilla root store plus `FORGE_MAIL_TLS_CA_FILE`, not the operating-system
store. It reads the `AUTH` mechanisms from both `250-AUTH PLAIN LOGIN` and
the legacy `250-AUTH=PLAIN LOGIN` advertisement, prefers `PLAIN` and falls
back to `LOGIN`; a server offering neither fails with `mail_auth_failed`.

**A delivery can be duplicated.** SMTP has no way to confirm a message that
was accepted: if the deadline (`FORGE_MAIL_TIMEOUT`) expires, or the
connection drops, after the server accepted the message data but before its
`250` reply reaches Forge, the send fails with `mail_unavailable` although
the server queued the message. A retry then delivers a second copy. This is
inherent to SMTP; make mail handlers tolerate a repeated message (an
idempotent action, or a deduplication key in the body), and do not retry
errors that are not `is_retryable()`.

## Templates

Bodies are askama templates, the engine the views use. HTML templates
(`.html`) escape every value; text templates (`.txt`) escape nothing.

```
templates/mailers/welcome.txt     Welcome, {{ name }}! Start here: {{ url }}
templates/mailers/welcome.html    <p>Welcome, {{ name }}!</p><p><a href="{{ url }}">Start</a></p>
```

```rust
use askama::Template;
use forge::mail::Templates;

#[derive(Template)]
#[template(path = "mailers/welcome.txt")]
struct WelcomeText<'a> { name: &'a str, url: &'a str }

#[derive(Template)]
#[template(path = "mailers/welcome.html")]
struct WelcomeHtml<'a> { name: &'a str, url: &'a str }

let message = Templates::new()
    .text(&WelcomeText { name, url })
    .html(&WelcomeHtml { name, url })
    .message(Message::new(from).to(to).subject("Welcome"))?;
sender.send(&message).await?;
```

Rendering is buffered: if any part fails the result is `mail_template` and
nothing is sent. A subject template (`.subject(&...)`) is used when the base
message has none, and is validated as a header like any subject.

## Development

With `FORGE_MAIL_URL=file:tmp/mail` every message becomes a `0600` `.eml`
file under `tmp/mail` and only its path is logged; a warning at start-up says
mail is not delivered. Open the newest file in any mail client. The directory
is created `0700`; an existing one that is group- or world-accessible is
tightened to `0700` (with a warning), and one that cannot be tightened is an
error. A sticky shared directory such as `/tmp`, and a symbolic link, are left
alone with a warning; the check and the change use one open handle, so the
path cannot be swapped in between. Two residual risks remain, both limited to
this development-only path and needing write access to the directory's
parent: symbolic links in the parent components of the path are still
followed, and replacing the directory with a FIFO between the check and the
open can block start-up (std cannot open a directory with `O_NOFOLLOW |
O_DIRECTORY` without a new dependency or unsafe code).

For a browser preview with fixture data, register previews. They need
`FORGE_ENV` development or test **and** `FORGE_MAIL_PREVIEW=true` (an unset
`FORGE_ENV` means development, so the environment alone is not enough):

```rust
use forge::mail::{Preview, PreviewHandler};

if composition.config().mail.preview {
    let welcome = Preview::new("welcome", "After sign-up", || {
        Ok(Message::new(Address::new("Shop", "shop@example.com")?)
            .to(Address::new("Ada", "ada@example.org")?)
            .subject("Welcome")
            .text("Hello Ada"))
    });
    let previews = PreviewHandler::new(composition.config(), "/_mail", vec![welcome])
        .map_err(forge::ComposeError::other)?;
    composition.router().layer(previews);
}
```

```bash
FORGE_MAIL_PREVIEW=true cargo run
# open http://127.0.0.1:3000/_mail
```

A handler answers its index and its own previews; any other path below its
prefix goes to the next layer, so handlers nest in any order: the password
reset preview at `/_mail` and the previews of a mailer generated by
`forge generate mailer user ...` at `/_mail/user` ([generators](generators.md#mailer)).

The page shows the headers, the HTML part in a sandboxed frame, the text
part, inline images, the raw MIME source and the validation error if the
message would not be sent. Responses are `GET`/`HEAD` only, `no-store`,
`nosniff` and served under a sandboxing Content-Security-Policy that allows
images only from the preview itself and `data:` URIs (`img-src 'self' data:`):
remote images and fonts are not loaded, so opening a preview does not contact
any server. Inline `cid:` images work.

## Testing

```rust
// Cargo.toml: [dev-dependencies] forge = { ..., features = ["views", "testing"] }
use forge::mail::testing::Recorder;

let outbox = Arc::new(Recorder::new());
let signup = Signup::new(outbox.clone());
signup.register("ada@example.org").await?;

let email = outbox.last().expect("one email");   // like ActionMailer::Base.deliveries
assert_eq!(email.recipients, ["ada@example.org"]);
assert_eq!(email.header("Subject").as_deref(), Some("Welcome"));
```

The recorder runs the real encoder, so a test fails on a message production
would reject. `outbox.fail_with(Some(MailError::Unavailable { .. }))`
simulates an outage.

## Password reset emails

Generated full-stack PostgreSQL applications wire this in
`src/bootstrap/mod.rs`:

```rust
let notifier = forge::mail::password_reset_notifier(
    composition.config(),
    env!("CARGO_PKG_NAME"),
    composition.telemetry(),
)
.map_err(forge::ComposeError::other)?;
```

With `FORGE_MAIL_URL`, `FORGE_MAIL_FROM` and `FORGE_AUTH_PASSWORD_RESET_URL`
set, reset links are emailed (`PasswordResetMailer`, a
`forge::auth::PasswordResetNotifier`); without `FORGE_MAIL_URL` the notifier
is the fail-closed `UndeliverablePasswordResets`; `FORGE_MAIL_URL` without
the reset URL stops the application at start-up. The token appears only in
the link inside the email; a failed delivery is logged with the error code and
SMTP stage only. Delivery runs on the bounded reset worker queue, off the
request path. With `FORGE_MAIL_PREVIEW=true` the email is previewed at
`/_mail/password_reset`. Replace the English template with
`PasswordResetEmails::with_template`. `PasswordResetEmails::new` checks the
reset URL with the same validator as the configuration loader
(`forge::config::validate_password_reset_url`), so a URL built in code is
held to the rules of `FORGE_AUTH_PASSWORD_RESET_URL`: https (http only on
localhost), a valid host and port, no credentials, query or fragment.

## Telemetry

`instrument(sender, telemetry)` records `forge.mail.send.duration`
(`forge_mail_send_duration_seconds{forge_mail_outcome="sent"|"mail_unavailable"|...}`);
`Instrumented::send_in(request.trace_context(), &message)` adds a
`mail send` client span to the request's trace. See
[observability](observability.md).

## Deliver later

`forge-mail` has no queue: a queue is a `Sender` that validates on the
request path and enqueues the serializable `Message` (for example as a
`forge::jobs` payload), and a worker that calls the real sender, retrying
`is_retryable()` errors and dead-lettering `mail_rejected` and
`mail_invalid`. Treat such payloads as secrets (they contain reset links and
attachments).
