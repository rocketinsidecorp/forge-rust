use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use forge_http::{
    BoxResponseFuture, HeaderValue, Method, Middleware, Next, Request, Response, StatusCode, header,
};

use crate::{
    Info, OCTET_STREAM, Signer, Storage, StorageError, StorageKey, is_inline_safe,
    normalize_content_type, sanitize_filename,
};

/// How long a browser may reuse a response without asking again, whatever the
/// remaining life of the link.
const MAX_PRIVATE_CACHE: Duration = Duration::from_secs(3600);
const DEFAULT_MAX_SERVE_BYTES: u64 = 16 << 20;

type BindingFn = Arc<dyn Fn(&Request) -> String + Send + Sync>;

/// Configures a [`StorageHandler`].
#[derive(Clone)]
pub struct HandlerOptions {
    mount: String,
    binding: Option<BindingFn>,
    inline_types: Vec<String>,
    max_serve_bytes: u64,
}

impl Default for HandlerOptions {
    fn default() -> Self {
        Self {
            mount: "/files".to_owned(),
            binding: None,
            inline_types: Vec::new(),
            max_serve_bytes: DEFAULT_MAX_SERVE_BYTES,
        }
    }
}

impl std::fmt::Debug for HandlerOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HandlerOptions")
            .field("mount", &self.mount)
            .field("bound", &self.binding.is_some())
            .field("inline_types", &self.inline_types)
            .field("max_serve_bytes", &self.max_serve_bytes)
            .finish()
    }
}

impl HandlerOptions {
    /// The path the handler serves below (default `/files`): a request for
    /// `<mount>/<key>` serves the blob of `<key>`.
    #[must_use]
    pub fn mount(mut self, mount: impl Into<String>) -> Self {
        self.mount = mount.into();
        self
    }

    /// Returns the value links must have been signed with
    /// ([`SignOptions::binding`](crate::SignOptions::binding)), typically the
    /// authenticated user's ID. Without it links are not bound.
    #[must_use]
    pub fn binding(mut self, binding: impl Fn(&Request) -> String + Send + Sync + 'static) -> Self {
        self.binding = Some(Arc::new(binding));
        self
    }

    /// Adds content types that links may display inline, beyond the raster
    /// images. Add only types that cannot run code in the application's origin.
    #[must_use]
    pub fn inline_types(mut self, types: &[&str]) -> Self {
        self.inline_types = types.iter().map(|&value| value.to_owned()).collect();
        self
    }

    /// The largest blob the handler loads into memory to serve (default
    /// 16 MiB); larger blobs answer `413` because responses are buffered.
    #[must_use]
    pub const fn max_serve_bytes(mut self, max_serve_bytes: u64) -> Self {
        self.max_serve_bytes = max_serve_bytes;
        self
    }
}

/// Serves blobs to holders of a valid signed link ([`Signer::url`]).
///
/// Register it as a layer (`router.layer(handler)`): it answers `GET` and
/// `HEAD` requests below its mount path, at any key depth, and passes every
/// other request on. The order of work is method, then **signature, expiry and
/// binding**, and only then the store, so a request without a valid, unexpired
/// signature for exactly the requested key is answered `403` before the store
/// is touched and reveals nothing about which keys exist. Responses carry the
/// stored content type, but a file is only displayed inline when its link
/// asked for it and its type is safe (raster images); every other file is a
/// download with the name stored at upload time. All responses set
/// `X-Content-Type-Options: nosniff`, a sandboxing `Content-Security-Policy`,
/// `Cross-Origin-Resource-Policy: same-site` and `Cache-Control: private`.
///
/// Byte ranges are not served (`forge-http` sends whole buffered responses);
/// `ETag` and `If-None-Match` work.
pub struct StorageHandler {
    store: Arc<dyn Storage>,
    signer: Arc<Signer>,
    mount: String,
    binding: Option<BindingFn>,
    inline: Vec<String>,
    max_serve_bytes: u64,
}

impl std::fmt::Debug for StorageHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StorageHandler")
            .field("mount", &self.mount)
            .finish_non_exhaustive()
    }
}

impl StorageHandler {
    /// A handler for `store` with links verified by `signer`.
    pub fn new(
        store: Arc<dyn Storage>,
        signer: Arc<Signer>,
        options: HandlerOptions,
    ) -> Result<Self, StorageError> {
        let mount = options.mount.trim_end_matches('/').to_owned();
        if !mount.starts_with('/')
            || mount.starts_with("//")
            || mount.contains(['?', '#', '%', '\\'])
        {
            return Err(StorageError::Configuration(
                "storage handler mount must be an absolute path without query or escapes",
            ));
        }
        let mut inline: Vec<String> = Vec::new();
        for value in &options.inline_types {
            let normalized = normalize_content_type(value).map_err(|_| {
                StorageError::Configuration("storage handler inline type is invalid")
            })?;
            if normalized == OCTET_STREAM {
                return Err(StorageError::Configuration(
                    "storage handler inline type is invalid",
                ));
            }
            inline.push(normalized);
        }
        Ok(Self {
            store,
            signer,
            mount,
            binding: options.binding,
            inline,
            max_serve_bytes: options.max_serve_bytes,
        })
    }

    /// The key a request path addresses, when it is below the mount. The path
    /// is used as sent: a key with a percent escape, dot segment or empty
    /// segment is not a valid [`StorageKey`].
    fn key_of<'a>(&self, path: &'a str) -> Option<&'a str> {
        path.strip_prefix(self.mount.as_str())?.strip_prefix('/')
    }

    /// Serves `request` for the blob below the mount, or `None` when the path
    /// is not below it.
    pub async fn serve(&self, request: &Request) -> Option<Response> {
        let raw_key = self.key_of(request.uri().path())?;
        Some(match self.respond(request, raw_key).await {
            Ok(response) => response,
            Err(error) => {
                if matches!(error, StorageError::Io(_)) {
                    tracing::error!(%error, "storage read failed");
                }
                let mut response = error.into_response(request);
                harden(&mut response);
                response
            }
        })
    }

    async fn respond(&self, request: &Request, raw_key: &str) -> Result<Response, StorageError> {
        if request.method() != Method::GET && request.method() != Method::HEAD {
            let mut response = Response::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            harden(&mut response);
            return Ok(response);
        }
        // Signature, expiry and binding first: the store is not consulted for a
        // request that has not proven it may read exactly this key.
        let key = StorageKey::parse(raw_key).map_err(|_| StorageError::InvalidLink)?;
        let binding = self
            .binding
            .as_ref()
            .map_or_else(String::new, |f| f(request));
        let query = request.uri().query();
        let inline_requested = self.signer.verify(&key, query, &binding)?;

        let blob = self.store.open(&key, self.max_serve_bytes).await?;
        let info = &blob.info;
        let content_type =
            normalize_content_type(&info.content_type).unwrap_or_else(|_| OCTET_STREAM.to_owned());
        let inline = inline_requested
            && (is_inline_safe(&content_type) || self.inline.iter().any(|t| *t == content_type));

        let mut response = Response::new(StatusCode::OK, bytes::Bytes::new());
        let headers = response.headers_mut();
        set(headers, header::CONTENT_TYPE, &content_type);
        let cache = self
            .signer
            .remaining(query)
            .min(MAX_PRIVATE_CACHE)
            .as_secs();
        set(
            headers,
            header::CACHE_CONTROL,
            &format!("private, max-age={cache}"),
        );
        let disposition = if inline {
            "inline".to_owned()
        } else {
            attachment(info, &key)
        };
        set(headers, header::CONTENT_DISPOSITION, &disposition);
        if let Some(digest) = &info.sha256 {
            set(headers, header::ETAG, &format!("\"{digest}\""));
        }
        harden(&mut response);

        if not_modified(request, info) {
            return Ok(with_status(
                response,
                StatusCode::NOT_MODIFIED,
                bytes::Bytes::new(),
            ));
        }
        set(
            response.headers_mut(),
            header::CONTENT_LENGTH,
            &blob.body.len().to_string(),
        );
        if request.method() == Method::GET {
            return Ok(with_status(response, StatusCode::OK, blob.body));
        }
        Ok(response)
    }
}

fn set(headers: &mut forge_http::HeaderMap, name: header::HeaderName, value: &str) {
    // Every value is built from validated or sanitized parts; a value that is
    // somehow not a legal header is dropped rather than sent.
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// The headers every storage response carries, whatever its status.
fn harden(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox; frame-ancestors 'none'"),
    );
    headers.insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-site"),
    );
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
}

/// Whether `If-None-Match` names the stored digest (or `*`).
fn not_modified(request: &Request, info: &Info) -> bool {
    let Some(digest) = &info.sha256 else {
        return false;
    };
    let Some(candidates) = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let ours = format!("\"{digest}\"");
    candidates.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == ours
    })
}

/// `Response` has no status setter: rebuild it with a status and body.
fn with_status(response: Response, status: StatusCode, body: bytes::Bytes) -> Response {
    let mut rebuilt = Response::new(status, body);
    *rebuilt.headers_mut() = response.headers().clone();
    rebuilt
}

/// The `Content-Disposition` of a download: the stored name when it is
/// ASCII-safe, otherwise an ASCII fallback plus the RFC 5987 UTF-8 form, and
/// the key's last segment when there is no name.
fn attachment(info: &Info, key: &StorageKey) -> String {
    let stored = info
        .filename
        .as_deref()
        .map(sanitize_filename)
        .unwrap_or_default();
    let name = if stored.is_empty() {
        key.as_str()
            .rsplit('/')
            .next()
            .unwrap_or("download")
            .to_owned()
    } else {
        stored
    };
    if name.is_ascii() {
        return format!("attachment; filename=\"{name}\"");
    }
    let fallback: String = name
        .chars()
        .map(|character| if character.is_ascii() { character } else { '_' })
        .collect();
    let mut encoded = String::with_capacity(name.len() * 3);
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

impl Middleware for StorageHandler {
    fn handle<'a>(&'a self, request: Request, next: Next<'a>) -> BoxResponseFuture<'a> {
        let future: Pin<Box<dyn Future<Output = Response> + Send + 'a>> = Box::pin(async move {
            match self.serve(&request).await {
                Some(response) => response,
                None => next.run(request).await,
            }
        });
        future
    }
}
