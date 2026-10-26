//! Signed links, the serving handler and uploads, end to end through the Forge
//! router with the disk adapter: the negative tests of ADR 0029.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::Bytes;
use forge_http::{
    HeaderMap, HeaderValue, Method, Request, Response, Router, StatusCode, Uri, header,
};
use forge_storage::{
    Blob, DiskStorage, HandlerOptions, Info, PutOptions, SignOptions, Signer, Storage,
    StorageError, StorageHandler, StorageKey, UploadOptions, receive,
};
use uuid::Uuid;

const BOUNDARY: &str = "----forge-boundary-7d1";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";
const PDF: &[u8] = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n";
const SECRET: [u8; 32] = [42; 32];

fn key(text: &str) -> StorageKey {
    StorageKey::parse(text).expect("key")
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)], body: Bytes) -> Request {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(
            header::HeaderName::from_bytes(name.as_bytes()).expect("name"),
            HeaderValue::from_str(value).expect("value"),
        );
    }
    Request::new_for_test(
        method,
        uri.parse::<Uri>().expect("uri"),
        map,
        body,
        "203.0.113.7:4000".parse().expect("addr"),
        Uuid::now_v7(),
    )
}

fn get(uri: &str) -> Request {
    request(Method::GET, uri, &[], Bytes::new())
}

/// Counts how often the handler reaches the store.
struct Counting {
    inner: DiskStorage,
    opens: AtomicUsize,
}

#[async_trait]
impl Storage for Counting {
    async fn put(
        &self,
        key: &StorageKey,
        body: Bytes,
        options: PutOptions,
    ) -> Result<Info, StorageError> {
        self.inner.put(key, body, options).await
    }

    async fn open(&self, key: &StorageKey, max_bytes: u64) -> Result<Blob, StorageError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.inner.open(key, max_bytes).await
    }

    async fn stat(&self, key: &StorageKey) -> Result<Info, StorageError> {
        self.inner.stat(key).await
    }

    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        self.inner.delete(key).await
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    store: Arc<Counting>,
    signer: Arc<Signer>,
    router: Router,
}

fn harness(options: HandlerOptions) -> Harness {
    harness_with(options, Signer::new(&SECRET, &[]).expect("signer"))
}

fn harness_with(options: HandlerOptions, signer: Signer) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Counting {
        inner: DiskStorage::open(dir.path().join("root")).expect("open"),
        opens: AtomicUsize::new(0),
    });
    let signer = Arc::new(signer);
    let handler = StorageHandler::new(store.clone(), signer.clone(), options).expect("handler");
    let mut router = Router::new();
    router.layer(handler);
    router
        .route(Method::GET, "/health", |_request: Request| async {
            Response::new(StatusCode::OK, "ok")
        })
        .expect("route");
    Harness {
        _dir: dir,
        store,
        signer,
        router,
    }
}

async fn put(
    h: &Harness,
    text: &str,
    body: &[u8],
    content_type: &str,
    filename: &str,
) -> StorageKey {
    let k = key(text);
    h.store
        .put(
            &k,
            Bytes::copy_from_slice(body),
            PutOptions::new(1 << 20)
                .expect("o")
                .content_type(content_type)
                .filename(filename),
        )
        .await
        .expect("put");
    k
}

fn header_of(response: &Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().expect("ascii").to_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn serves_a_signed_link_with_safe_headers() {
    let h = harness(HandlerOptions::default());
    let k = put(
        &h,
        "docs/report.pdf",
        PDF,
        "application/pdf",
        "Quarterly report.pdf",
    )
    .await;
    let url = h
        .signer
        .url(
            "/files",
            &k,
            &SignOptions::default().ttl(Duration::from_secs(600)),
        )
        .expect("url");
    let response = h.router.dispatch(get(&url)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().as_ref(), PDF);
    assert_eq!(header_of(&response, "content-type"), "application/pdf");
    assert_eq!(header_of(&response, "x-content-type-options"), "nosniff");
    assert_eq!(
        header_of(&response, "content-security-policy"),
        "default-src 'none'; sandbox; frame-ancestors 'none'"
    );
    assert_eq!(
        header_of(&response, "content-disposition"),
        "attachment; filename=\"Quarterly report.pdf\""
    );
    let cache = header_of(&response, "cache-control");
    assert!(cache.starts_with("private, max-age="), "{cache}");
    let max_age: u64 = cache
        .rsplit('=')
        .next()
        .expect("age")
        .parse()
        .expect("number");
    assert!((590..=601).contains(&max_age), "{cache}");
    assert_eq!(
        header_of(&response, "content-length"),
        PDF.len().to_string()
    );
    let etag = header_of(&response, "etag");
    assert!(etag.starts_with('"') && etag.len() == 66, "{etag}");

    // Conditional request.
    let conditional = request(Method::GET, &url, &[("if-none-match", &etag)], Bytes::new());
    let not_modified = h.router.dispatch(conditional).await;
    assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
    assert!(not_modified.body().is_empty());

    // HEAD: same headers, no body.
    let head = h
        .router
        .dispatch(request(Method::HEAD, &url, &[], Bytes::new()))
        .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.body().is_empty());
    assert_eq!(header_of(&head, "content-length"), PDF.len().to_string());
    assert_eq!(header_of(&head, "etag"), etag);
}

#[tokio::test]
async fn the_max_age_never_outlives_the_link() {
    let h = harness(HandlerOptions::default());
    let k = put(&h, "a.png", PNG, "image/png", "").await;
    let url = h
        .signer
        .url(
            "/files",
            &k,
            &SignOptions::default().ttl(Duration::from_secs(7 * 24 * 3600)),
        )
        .expect("url");
    let cache = header_of(&h.router.dispatch(get(&url)).await, "cache-control");
    assert_eq!(cache, "private, max-age=3600", "capped at one hour");
}

#[tokio::test]
async fn active_content_is_never_displayed_inline() {
    let h = harness(HandlerOptions::default());
    let cases: [(&str, &[u8], &str); 8] = [
        (
            "x/page.html",
            b"<!DOCTYPE html><script>alert(1)</script>",
            "text/html",
        ),
        (
            "x/image.svg",
            b"<?xml version=\"1.0\"?><svg onload=\"alert(1)\"/>",
            "image/svg+xml",
        ),
        ("x/data.xml", b"<?xml version=\"1.0\"?><a/>", "text/xml"),
        ("x/doc.pdf", PDF, "application/pdf"),
        ("x/note.txt", b"hello", "text/plain"),
        ("x/app.js", b"alert(1)", "application/javascript"),
        ("x/unknown", b"\x00\x01", "application/octet-stream"),
        (
            "x/lying.png",
            b"<html><script>alert(1)</script>",
            "text/html",
        ),
    ];
    for (name, body, content_type) in cases {
        let k = put(&h, name, body, content_type, "").await;
        let url = h
            .signer
            .url("/files", &k, &SignOptions::default().inline(true))
            .expect("url");
        let response = h.router.dispatch(get(&url)).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let disposition = header_of(&response, "content-disposition");
        assert!(
            disposition.starts_with("attachment"),
            "{name} was offered inline: {disposition}"
        );
        assert_eq!(header_of(&response, "x-content-type-options"), "nosniff");
        assert!(
            header_of(&response, "content-security-policy").contains("sandbox"),
            "{name}"
        );
        assert_eq!(header_of(&response, "content-type"), content_type, "{name}");
    }
    // Raster images are inline when the link asks for it, and only then.
    let png = put(&h, "x/pic.png", PNG, "image/png", "pic.png").await;
    let inline = h
        .signer
        .url("/files", &png, &SignOptions::default().inline(true))
        .expect("url");
    assert_eq!(
        header_of(
            &h.router.dispatch(get(&inline)).await,
            "content-disposition"
        ),
        "inline"
    );
    let plain = h
        .signer
        .url("/files", &png, &SignOptions::default())
        .expect("url");
    assert!(
        header_of(&h.router.dispatch(get(&plain)).await, "content-disposition")
            .starts_with("attachment")
    );
    // Adding `disposition=inline` to a link that was not signed for it is a forgery.
    let forged = format!("{plain}&disposition=inline");
    assert_eq!(
        h.router.dispatch(get(&forged)).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn the_inline_set_can_be_extended_but_never_with_octet_stream() {
    let h = harness(HandlerOptions::default().inline_types(&["audio/mpeg"]));
    let k = put(&h, "song.mp3", b"ID3\x03\x00", "audio/mpeg", "").await;
    let url = h
        .signer
        .url("/files", &k, &SignOptions::default().inline(true))
        .expect("url");
    assert_eq!(
        header_of(&h.router.dispatch(get(&url)).await, "content-disposition"),
        "inline"
    );
    let signer = Arc::new(Signer::new(&SECRET, &[]).expect("signer"));
    let store: Arc<dyn Storage> = h.store.clone();
    assert!(
        StorageHandler::new(
            store,
            signer,
            HandlerOptions::default().inline_types(&["application/octet-stream"])
        )
        .is_err()
    );
}

#[tokio::test]
async fn invalid_and_expired_links_never_touch_the_store() {
    let h = harness(HandlerOptions::default());
    let k = put(&h, "secret/file.txt", b"classified", "text/plain", "").await;
    let good = h
        .signer
        .url("/files", &k, &SignOptions::default())
        .expect("url");
    let (path, query) = good.split_once('?').expect("query");
    let sig = query
        .split('&')
        .find_map(|p| p.strip_prefix("sig="))
        .expect("sig");
    let expires = query
        .split('&')
        .find_map(|p| p.strip_prefix("expires="))
        .expect("expires");
    let other_signer = Signer::new(&[1; 32], &[]).expect("signer");
    let foreign = other_signer
        .url("/files", &k, &SignOptions::default())
        .expect("url");
    let other_key = h
        .signer
        .url("/files", &key("secret/other.txt"), &SignOptions::default())
        .expect("url");
    let swapped = format!(
        "/files/secret/file.txt?{}",
        other_key.split_once('?').expect("q").1
    );
    let mut tampered_sig = sig.to_owned();
    tampered_sig.replace_range(0..1, if sig.starts_with('A') { "B" } else { "A" });

    let uris = [
        "/files/secret/file.txt".to_owned(),
        format!("{path}?expires={expires}"),
        format!("{path}?sig={sig}"),
        format!("{path}?expires={expires}&sig={tampered_sig}"),
        format!(
            "{path}?expires={}&sig={sig}",
            expires.parse::<u64>().expect("n") + 3600
        ),
        foreign,
        swapped,
        format!("{good}&expires={expires}"),
        format!("{good}&sig={sig}"),
        "/files/secret/file.txt?expires=abc&sig=xyz".to_owned(),
        "/files/..%2fsecret/file.txt?expires=1&sig=x".to_owned(),
        "/files/secret/%66ile.txt?".to_owned() + query,
        "/files/secret//file.txt?".to_owned() + query,
        "/files/./secret/file.txt?".to_owned() + query,
        "/files/.hidden?".to_owned() + query,
        "/files/?".to_owned() + query,
        "/files?".to_owned() + query,
    ];
    for uri in uris {
        let response = h.router.dispatch(get(&uri)).await;
        // `/files` itself is below no key, so the router answers it; every other form is refused.
        if uri.starts_with("/files?") {
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            continue;
        }
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(header_of(&response, "x-content-type-options"), "nosniff");
        let text = String::from_utf8_lossy(response.body());
        assert!(text.contains("link_invalid"), "{uri}: {text}");
        assert!(!text.contains("classified"));
    }
    assert_eq!(
        h.store.opens.load(Ordering::SeqCst),
        0,
        "the store was consulted for an unauthenticated request"
    );

    // Whether the key exists must not be observable without a valid link.
    let missing = h.router.dispatch(get("/files/no/such/key.txt")).await;
    let existing = h.router.dispatch(get("/files/secret/file.txt")).await;
    assert_eq!(missing.status(), existing.status());
    // Only the per-request id differs: the same code and message either way.
    for response in [&missing, &existing] {
        let text = String::from_utf8_lossy(response.body());
        assert!(text.contains("link_invalid") && text.contains("invalid or tampered link"));
    }
}

#[tokio::test]
async fn an_authentic_link_that_expired_is_reported_as_expired() {
    // The signer reads a fake clock, so the link expires by moving it.
    let now = Arc::new(AtomicU64::new(1_800_000_000));
    let clock = Arc::clone(&now);
    let signer = Signer::new(&SECRET, &[])
        .expect("signer")
        .with_clock(Box::new(move || {
            UNIX_EPOCH + Duration::from_secs(clock.load(Ordering::SeqCst))
        }));
    let h = harness_with(HandlerOptions::default(), signer);
    let k = put(&h, "a.txt", b"x", "text/plain", "").await;
    let url = h
        .signer
        .url(
            "/files",
            &k,
            &SignOptions::default().ttl(Duration::from_secs(1)),
        )
        .expect("url");
    let valid = h.router.dispatch(get(&url)).await;
    assert_eq!(
        valid.status(),
        StatusCode::OK,
        "the link works until it expires"
    );
    let opens = h.store.opens.load(Ordering::SeqCst);
    now.fetch_add(3, Ordering::SeqCst);
    let response = h.router.dispatch(get(&url)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(response.body()).contains("link_expired"));
    assert_eq!(h.store.opens.load(Ordering::SeqCst), opens);
}

#[tokio::test]
async fn a_bound_link_stops_working_for_anyone_else() {
    let bound = HandlerOptions::default().binding(|request: &Request| {
        request
            .headers()
            .get("x-user")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    });
    let h = harness(bound);
    let k = put(&h, "mine.txt", b"private", "text/plain", "").await;
    let url = h
        .signer
        .url("/files", &k, &SignOptions::default().binding("user-1"))
        .expect("url");
    let as_user = |user: &str| request(Method::GET, &url, &[("x-user", user)], Bytes::new());
    assert_eq!(
        h.router.dispatch(as_user("user-1")).await.status(),
        StatusCode::OK
    );
    for other in ["user-2", "", "USER-1"] {
        assert_eq!(
            h.router.dispatch(as_user(other)).await.status(),
            StatusCode::FORBIDDEN,
            "{other:?}"
        );
    }
}

#[tokio::test]
async fn methods_other_than_get_and_head_and_unrelated_paths() {
    let h = harness(HandlerOptions::default());
    let k = put(&h, "a.txt", b"x", "text/plain", "").await;
    let url = h
        .signer
        .url("/files", &k, &SignOptions::default())
        .expect("url");
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        let response = h
            .router
            .dispatch(request(method.clone(), &url, &[], Bytes::new()))
            .await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
        assert_eq!(header_of(&response, "allow"), "GET, HEAD");
    }
    // Other paths, and paths that merely share the mount's prefix, reach the router.
    assert_eq!(
        h.router.dispatch(get("/health")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        h.router.dispatch(get("/filesystem/x")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.store.opens.load(Ordering::SeqCst), 0);
}

/// A store that returns whatever metadata it is told to.
struct Crafted(Info, Bytes);

#[async_trait]
impl Storage for Crafted {
    async fn put(&self, _: &StorageKey, _: Bytes, _: PutOptions) -> Result<Info, StorageError> {
        Err(StorageError::NotFound)
    }

    async fn open(&self, _: &StorageKey, _: u64) -> Result<Blob, StorageError> {
        Ok(Blob {
            info: self.0.clone(),
            body: self.1.clone(),
        })
    }

    async fn stat(&self, _: &StorageKey) -> Result<Info, StorageError> {
        Ok(self.0.clone())
    }

    async fn delete(&self, _: &StorageKey) -> Result<(), StorageError> {
        Ok(())
    }
}

#[tokio::test]
async fn stored_metadata_cannot_inject_headers() {
    let k = key("blob");
    let info = Info {
        key: k.clone(),
        size: 1,
        content_type: "text/html\r\nSet-Cookie: pwn=1".to_owned(),
        filename: Some("a\r\nSet-Cookie: pwn=2\r\n\r\n<script>.txt".to_owned()),
        sha256: Some("ab\r\nX: y".to_owned()),
        modified: std::time::SystemTime::now(),
    };
    let signer = Arc::new(Signer::new(&SECRET, &[]).expect("signer"));
    let handler = StorageHandler::new(
        Arc::new(Crafted(info, Bytes::from_static(b"x"))),
        signer.clone(),
        HandlerOptions::default(),
    )
    .expect("handler");
    let url = signer
        .url("/files", &k, &SignOptions::default().inline(true))
        .expect("url");
    let response = handler.serve(&get(&url)).await.expect("served");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(
        header_of(&response, "content-type"),
        "application/octet-stream",
        "an invalid stored type is a download"
    );
    for (name, value) in response.headers() {
        let text = value.to_str().expect("ascii header");
        assert!(!text.contains(['\r', '\n']), "{name}: {text:?}");
    }
    assert!(header_of(&response, "content-disposition").starts_with("attachment"));
}

#[tokio::test]
async fn non_ascii_file_names_use_the_rfc_5987_form_and_keys_name_nameless_downloads() {
    let h = harness(HandlerOptions::default());
    let k = put(
        &h,
        "docs/r.pdf",
        PDF,
        "application/pdf",
        "relatório final.pdf",
    )
    .await;
    let url = h
        .signer
        .url("/files", &k, &SignOptions::default())
        .expect("url");
    let disposition = header_of(&h.router.dispatch(get(&url)).await, "content-disposition");
    assert_eq!(
        disposition,
        "attachment; filename=\"relat_rio final.pdf\"; filename*=UTF-8''relat%C3%B3rio%20final.pdf"
    );
    let nameless = put(&h, "docs/nameless", b"x", "application/octet-stream", "").await;
    let url = h
        .signer
        .url("/files", &nameless, &SignOptions::default())
        .expect("url");
    assert_eq!(
        header_of(&h.router.dispatch(get(&url)).await, "content-disposition"),
        "attachment; filename=\"nameless\""
    );
}

#[tokio::test]
async fn blobs_over_the_serving_limit_are_413_not_loaded() {
    let h = harness(HandlerOptions::default().max_serve_bytes(10));
    let k = put(&h, "big.bin", &[0; 100], "application/octet-stream", "").await;
    let url = h
        .signer
        .url("/files", &k, &SignOptions::default())
        .expect("url");
    assert_eq!(
        h.router.dispatch(get(&url)).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn handler_options_are_validated() {
    let signer = Arc::new(Signer::new(&SECRET, &[]).expect("signer"));
    let dir = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn Storage> = Arc::new(DiskStorage::open(dir.path().join("r")).expect("open"));
    for bad in ["files", "//evil", "/f?x", "/f%2e", "/f\\x"] {
        assert!(
            StorageHandler::new(
                store.clone(),
                signer.clone(),
                HandlerOptions::default().mount(bad)
            )
            .is_err(),
            "{bad}"
        );
    }
    assert!(
        StorageHandler::new(
            store,
            signer,
            HandlerOptions::default().mount("/downloads/")
        )
        .is_ok()
    );
}

// ---- uploads ---------------------------------------------------------------

struct Part {
    field: &'static str,
    filename: Option<&'static str>,
    content_type: Option<&'static str>,
    content: Vec<u8>,
    headers: Vec<(&'static str, &'static str)>,
}

fn text(field: &'static str, value: &str) -> Part {
    Part {
        field,
        filename: None,
        content_type: None,
        content: value.as_bytes().to_vec(),
        headers: vec![],
    }
}

fn file(
    field: &'static str,
    filename: &'static str,
    content_type: &'static str,
    content: &[u8],
) -> Part {
    Part {
        field,
        filename: Some(filename),
        content_type: (!content_type.is_empty()).then_some(content_type),
        content: content.to_vec(),
        headers: vec![],
    }
}

fn multipart(parts: &[Part]) -> Bytes {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{}\"",
                part.field
            )
            .as_bytes(),
        );
        if let Some(filename) = part.filename {
            body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        if let Some(content_type) = part.content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        for (name, value) in &part.headers {
            body.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part.content);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    Bytes::from(body)
}

fn upload_request(body: Bytes) -> Request {
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
    request(
        Method::POST,
        "/avatars",
        &[("content-type", &content_type)],
        body,
    )
}

fn default_options() -> UploadOptions {
    UploadOptions::new("avatar", &["image/png", "image/jpeg", "text/csv"], 4096)
        .expect("options")
        .prefix("avatars")
}

fn blob_count(dir: &std::path::Path) -> usize {
    fn count(path: &std::path::Path) -> usize {
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .map(|e| e.expect("entry").path())
                    .map(|p| if p.is_dir() { count(&p) } else { 1 })
                    .sum()
            })
            .unwrap_or(0)
    }
    count(&dir.join("blobs")) + count(&dir.join("meta")) + count(&dir.join("tmp"))
}

struct Uploads {
    dir: tempfile::TempDir,
    store: DiskStorage,
}

fn uploads() -> Uploads {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = DiskStorage::open(dir.path().join("root")).expect("open");
    Uploads { dir, store }
}

impl Uploads {
    fn blobs(&self) -> usize {
        blob_count(&self.dir.path().join("root"))
    }

    async fn receive(
        &self,
        parts: &[Part],
        options: &UploadOptions,
    ) -> Result<forge_storage::Received, StorageError> {
        receive(&upload_request(multipart(parts)), &self.store, options).await
    }
}

#[tokio::test]
async fn stores_an_allowed_file_and_returns_the_text_fields() {
    let u = uploads();
    let got = u
        .receive(
            &[
                text("authenticity_token", "tok"),
                text("name", "Ana"),
                file("avatar", "me.png", "image/png", PNG),
            ],
            &default_options(),
        )
        .await
        .expect("receive");
    assert_eq!(got.files.len(), 1);
    assert_eq!(got.fields["name"], vec!["Ana"]);
    assert_eq!(got.fields["authenticity_token"], vec!["tok"]);
    let info = &got.files[0];
    assert!(info.key.as_str().starts_with("avatars/"), "{}", info.key);
    assert_eq!(
        (
            info.content_type.as_str(),
            info.filename.as_deref(),
            info.size
        ),
        ("image/png", Some("me.png"), PNG.len() as u64)
    );
    let stored = u.store.open(&info.key, 1 << 20).await.expect("open");
    assert_eq!(
        stored.body.as_ref(),
        PNG,
        "stored content differs from the upload"
    );
}

#[tokio::test]
async fn rejects_disallowed_and_disguised_types_and_stores_nothing() {
    let html = b"<!DOCTYPE html><html><script>alert(1)</script></html>".as_slice();
    let svg =
        br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"/>"#
            .as_slice();
    let exe = [b"MZ\x90\x00\x03\x00\x00\x00".as_slice(), &[0; 100]].concat();
    let mut polyglot = b"GIF89a".to_vec();
    polyglot.extend_from_slice(b"<script>alert(1)</script>");
    let cases: Vec<(&str, Part)> = vec![
        ("html as png", file("avatar", "a.png", "image/png", html)),
        ("html as csv", file("avatar", "a.csv", "text/csv", html)),
        (
            "html declared html",
            file("avatar", "a.html", "text/html", html),
        ),
        ("svg as png", file("avatar", "a.png", "image/png", svg)),
        (
            "svg declared svg",
            file("avatar", "a.svg", "image/svg+xml", svg),
        ),
        (
            "pdf not allowed",
            file("avatar", "a.pdf", "application/pdf", PDF),
        ),
        ("pdf as png", file("avatar", "a.png", "image/png", PDF)),
        (
            "gif polyglot not allowed",
            file("avatar", "a.png", "image/png", &polyglot),
        ),
        (
            "junk claims png",
            file("avatar", "a.png", "image/png", b"this is not a png at all"),
        ),
        ("exe claims png", file("avatar", "a.png", "image/png", &exe)),
        ("exe claims csv", file("avatar", "a.csv", "text/csv", &exe)),
        (
            "unlisted declared plain",
            file("avatar", "a.txt", "text/plain", b"hi"),
        ),
        ("no content type, binary", file("avatar", "a.bin", "", &exe)),
    ];
    for (name, part) in cases {
        let u = uploads();
        let result = u.receive(&[part], &default_options()).await;
        assert!(
            matches!(result, Err(StorageError::UnsupportedType)),
            "{name}: {result:?}"
        );
        assert_eq!(u.blobs(), 0, "{name}: a blob was stored");
    }
}

#[tokio::test]
async fn a_garbage_label_on_recognizable_content_changes_nothing() {
    // The bytes decide: a malformed or lying label is irrelevant to an allowed image.
    let u = uploads();
    let got = u
        .receive(&[file("avatar", "a.png", "nope", PNG)], &default_options())
        .await
        .expect("png");
    assert_eq!(got.files[0].content_type, "image/png");
    // ... but a malformed label on content with no signature is refused.
    let result = u
        .receive(
            &[file("avatar", "a.csv", "nope", b"a,b\n")],
            &default_options(),
        )
        .await;
    assert!(
        matches!(result, Err(StorageError::UnsupportedType)),
        "{result:?}"
    );
}

#[tokio::test]
async fn undetectable_types_are_accepted_only_when_declared_and_allowed() {
    let u = uploads();
    let got = u
        .receive(
            &[file("avatar", "d.csv", "text/csv", b"a,b\n1,2\n")],
            &default_options(),
        )
        .await
        .expect("csv");
    assert_eq!(got.files[0].content_type, "text/csv");
    // SVG with a prolog is stored as SVG when explicitly allowed (the handler never shows it inline).
    let options = UploadOptions::new("avatar", &["image/svg+xml"], 4096).expect("options");
    let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#;
    let got = u
        .receive(&[file("avatar", "x.svg", "image/svg+xml", svg)], &options)
        .await
        .expect("svg");
    assert_eq!(got.files[0].content_type, "image/svg+xml");
}

#[tokio::test]
async fn oversized_files_leave_nothing_behind() {
    let u = uploads();
    let big = [PNG, &vec![1; 5000]].concat();
    let result = u
        .receive(
            &[file("avatar", "big.png", "image/png", &big)],
            &default_options(),
        )
        .await;
    assert!(matches!(result, Err(StorageError::TooLarge)), "{result:?}");
    assert_eq!(u.blobs(), 0);
    // A file over the limit after a good one: nothing of the request is stored.
    let options = default_options().max_files(2);
    let result = u
        .receive(
            &[
                file("avatar", "ok.png", "image/png", PNG),
                file("avatar", "big.png", "image/png", &big),
            ],
            &options,
        )
        .await;
    assert!(matches!(result, Err(StorageError::TooLarge)), "{result:?}");
    assert_eq!(u.blobs(), 0);
}

#[tokio::test]
async fn the_request_body_cap_applies_before_anything_is_parsed() {
    let u = uploads();
    let options = UploadOptions::new("avatar", &["image/png"], 1000)
        .expect("options")
        .text_fields(1, 100);
    // Far more than one file, one field and the framing allowance.
    let huge = [PNG, &vec![1; 300_000]].concat();
    let result = u
        .receive(&[file("avatar", "h.png", "image/png", &huge)], &options)
        .await;
    assert!(matches!(result, Err(StorageError::TooLarge)), "{result:?}");
    assert_eq!(u.blobs(), 0);
}

/// A named upload, the parts it sends and the specific error it must produce (if any).
type Case = (&'static str, Vec<Part>, Option<fn(&StorageError) -> bool>);

#[tokio::test]
async fn structural_violations_are_rejected_and_store_nothing() {
    let png = |field| file(field, "a.png", "image/png", PNG);
    let encoded = Part {
        headers: vec![("Content-Transfer-Encoding", "base64")],
        ..file("avatar", "a.png", "image/png", PNG)
    };
    let cases: Vec<Case> = vec![
        ("file in another field", vec![png("other")], None),
        ("too many files", vec![png("avatar"), png("avatar")], None),
        (
            "empty file",
            vec![file("avatar", "e.png", "image/png", b"")],
            None,
        ),
        ("encoded part", vec![encoded], None),
        (
            "no file",
            vec![text("name", "x")],
            Some(|e| matches!(e, StorageError::NoFile)),
        ),
        (
            "empty file input",
            vec![Part {
                filename: Some(""),
                ..text("avatar", "")
            }],
            Some(|e| matches!(e, StorageError::NoFile)),
        ),
        (
            "oversized text field",
            vec![text("bio", &"x".repeat(70_000)), png("avatar")],
            Some(|e| matches!(e, StorageError::TooLarge)),
        ),
    ];
    for (name, parts, expected) in cases {
        let u = uploads();
        let error = u.receive(&parts, &default_options()).await.expect_err(name);
        if let Some(check) = expected {
            assert!(check(&error), "{name}: {error:?}");
        }
        assert_eq!(u.blobs(), 0, "{name}");
    }
    // Too many text fields.
    let u = uploads();
    let mut parts: Vec<Part> = Vec::new();
    for field in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        parts.push(text(field, "v"));
    }
    let options = default_options().text_fields(4, 1024);
    parts.push(png("avatar"));
    assert!(
        u.receive(&parts, &options).await.is_err(),
        "8 text fields accepted with a budget of 4"
    );
    assert_eq!(u.blobs(), 0);
}

#[tokio::test]
async fn repeated_field_names_count_against_the_field_budget() {
    let options = UploadOptions::new("avatar", &["image/png"], 8 << 20)
        .expect("options")
        .text_fields(2, 4 << 10);
    // One name repeated: a per-name count would see a single field and buffer every value.
    let u = uploads();
    let mut parts: Vec<Part> = (0..1500)
        .map(|_| text("note", &"x".repeat(4 << 10)))
        .collect();
    parts.push(file("avatar", "a.png", "image/png", PNG));
    let result = u.receive(&parts, &options).await;
    assert!(
        result.is_err(),
        "1500 values of one field name were accepted with a budget of 2"
    );
    assert_eq!(u.blobs(), 0);
    // Within the budget, repetitions are fine.
    let got = u
        .receive(
            &[
                text("tag", "a"),
                text("tag", "b"),
                file("avatar", "a.png", "image/png", PNG),
            ],
            &options,
        )
        .await
        .expect("two repeated values");
    assert_eq!(got.fields["tag"], vec!["a", "b"]);
}

#[tokio::test]
async fn text_fields_are_bounded_in_total_bytes_too() {
    // Each value is within its own bound; together they exceed budget x bound.
    let options = UploadOptions::new("avatar", &["image/png"], 1024)
        .expect("options")
        .text_fields(3, 100);
    let u = uploads();
    let parts = [
        text("a", &"x".repeat(100)),
        text("b", &"x".repeat(100)),
        text("c", &"x".repeat(100)),
        file("avatar", "a.png", "image/png", PNG),
    ];
    assert!(
        u.receive(&parts, &options).await.is_ok(),
        "exactly the budget"
    );
    let over = [
        text("a", &"x".repeat(101)),
        file("avatar", "a.png", "image/png", PNG),
    ];
    assert!(matches!(
        u.receive(&over, &options).await,
        Err(StorageError::TooLarge)
    ));
}

#[tokio::test]
async fn wrong_requests_are_415() {
    let u = uploads();
    let options = default_options();
    for content_type in [
        "application/json",
        "application/x-www-form-urlencoded",
        "multipart/mixed; boundary=abc",
        "multipart/form-data",
        "multipart/form-data; boundary=",
        "",
    ] {
        let mut headers = Vec::new();
        if !content_type.is_empty() {
            headers.push(("content-type", content_type));
        }
        let result = receive(
            &request(
                Method::POST,
                "/u",
                &headers,
                multipart(&[file("avatar", "a.png", "image/png", PNG)]),
            ),
            &u.store,
            &options,
        )
        .await;
        assert!(
            matches!(result, Err(StorageError::NotMultipart)),
            "{content_type:?}: {result:?}"
        );
    }
    assert_eq!(u.blobs(), 0);
}

#[tokio::test]
async fn malformed_bodies_are_400() {
    let u = uploads();
    for body in [
        Bytes::new(),
        Bytes::from_static(b"not multipart at all"),
        Bytes::from(format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"a.png\"\r\n\r\ntruncated"
        )),
    ] {
        let result = receive(&upload_request(body), &u.store, &default_options()).await;
        assert!(
            matches!(result, Err(StorageError::InvalidUpload(_))),
            "{result:?}"
        );
    }
    assert_eq!(u.blobs(), 0);
}

#[test]
fn option_validation() {
    assert!(UploadOptions::new("", &["image/png"], 1).is_err());
    assert!(
        UploadOptions::new("f", &[], 1).is_err(),
        "an allow-list is mandatory"
    );
    assert!(
        UploadOptions::new("f", &["*/*"], 1).is_err(),
        "no wildcards"
    );
    assert!(UploadOptions::new("f", &["image/*"], 1).is_err());
    assert!(UploadOptions::new("f", &["image/png"], 0).is_err());
}

#[tokio::test]
async fn bad_options_are_refused_at_receive_time() {
    let u = uploads();
    let body = [file("avatar", "a.png", "image/png", PNG)];
    for options in [
        default_options().max_files(0),
        default_options().prefix("../escape"),
        default_options().prefix(".hidden"),
        default_options().text_fields(1, 0),
    ] {
        assert!(matches!(
            u.receive(&body, &options).await,
            Err(StorageError::Configuration(_))
        ));
    }
    assert_eq!(u.blobs(), 0);
}

#[tokio::test]
async fn multiple_files_get_unique_uuidv7_keys() {
    let u = uploads();
    let options = default_options().max_files(3);
    let parts: Vec<Part> = (0..3)
        .map(|_| file("avatar", "same.png", "image/png", PNG))
        .collect();
    let got = u.receive(&parts, &options).await.expect("receive");
    let mut keys: Vec<&str> = got.files.iter().map(|f| f.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 3);
    for info in &got.files {
        let id = info.key.as_str().strip_prefix("avatars/").expect("prefix");
        assert_eq!(Uuid::parse_str(id).expect("uuid").get_version_num(), 7);
    }
}

#[tokio::test]
async fn the_client_file_name_is_never_a_key_or_a_path() {
    let u = uploads();
    let got = u
        .receive(
            &[file("avatar", "../../etc/passwd.png", "image/png", PNG)],
            &default_options(),
        )
        .await
        .expect("receive");
    let info = &got.files[0];
    assert!(!info.key.as_str().contains("passwd"), "{}", info.key);
    assert_eq!(
        info.filename.as_deref(),
        Some("passwd.png"),
        "sanitized and kept as metadata only"
    );
}

#[tokio::test]
async fn a_missing_file_can_still_deliver_the_fields_when_optional() {
    let u = uploads();
    let options = default_options().optional_file(true);
    let got = u
        .receive(&[text("name", "Ana")], &options)
        .await
        .expect("receive");
    assert!(got.files.is_empty());
    assert_eq!(got.fields["name"], vec!["Ana"]);
}

/// A store that fails its second `put`, like a full disk half way through.
struct FailSecond {
    inner: DiskStorage,
    puts: Mutex<usize>,
}

#[async_trait]
impl Storage for FailSecond {
    async fn put(
        &self,
        key: &StorageKey,
        body: Bytes,
        options: PutOptions,
    ) -> Result<Info, StorageError> {
        let nth = {
            let mut puts = self.puts.lock().expect("lock");
            *puts += 1;
            *puts
        };
        if nth == 2 {
            return Err(StorageError::Io(std::io::Error::other("disk full")));
        }
        self.inner.put(key, body, options).await
    }

    async fn open(&self, key: &StorageKey, max_bytes: u64) -> Result<Blob, StorageError> {
        self.inner.open(key, max_bytes).await
    }

    async fn stat(&self, key: &StorageKey) -> Result<Info, StorageError> {
        self.inner.stat(key).await
    }

    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        self.inner.delete(key).await
    }
}

#[tokio::test]
async fn a_store_failure_half_way_removes_the_files_already_stored() {
    let u = uploads();
    let store = FailSecond {
        inner: u.store.clone(),
        puts: Mutex::new(0),
    };
    let options = default_options().max_files(2);
    let parts = [
        file("avatar", "1.png", "image/png", PNG),
        file("avatar", "2.png", "image/png", PNG),
    ];
    let result = receive(&upload_request(multipart(&parts)), &store, &options).await;
    assert!(matches!(result, Err(StorageError::Io(_))), "{result:?}");
    assert_eq!(u.blobs(), 0, "the first file was left behind");
}

#[tokio::test]
async fn end_to_end_upload_then_signed_download_through_the_router() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(DiskStorage::open(dir.path().join("root")).expect("open"));
    let signer = Arc::new(Signer::new(&SECRET, &[]).expect("signer"));
    let mut router = Router::new();
    router.layer(
        StorageHandler::new(store.clone(), signer.clone(), HandlerOptions::default())
            .expect("handler"),
    );
    let (route_store, route_signer) = (store.clone(), signer.clone());
    router
        .route(Method::POST, "/avatars", move |request: Request| {
            let (store, signer) = (route_store.clone(), route_signer.clone());
            async move {
                let options = UploadOptions::new("avatar", &["image/png"], 4096)
                    .expect("options")
                    .prefix("avatars");
                match receive(&request, store.as_ref(), &options).await {
                    Ok(received) => {
                        let url = signer
                            .url(
                                "/files",
                                &received.files[0].key,
                                &SignOptions::default().inline(true),
                            )
                            .expect("url");
                        Response::new(StatusCode::CREATED, url)
                    }
                    Err(error) => error.into_response(&request),
                }
            }
        })
        .expect("route");

    let created = router
        .dispatch(upload_request(multipart(&[file(
            "avatar",
            "me.png",
            "image/png",
            PNG,
        )])))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let url = String::from_utf8(created.body().to_vec()).expect("utf8");
    let served = router.dispatch(get(&url)).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.body().as_ref(), PNG);
    assert_eq!(header_of(&served, "content-disposition"), "inline");

    // A rejected upload answers with a JSON error, not storage internals.
    let rejected = router
        .dispatch(upload_request(multipart(&[file(
            "avatar",
            "a.png",
            "image/png",
            b"<html><script>",
        )])))
        .await;
    assert_eq!(rejected.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body = String::from_utf8_lossy(rejected.body()).into_owned();
    assert!(
        body.contains("unsupported_media_type") && body.contains("request_id"),
        "{body}"
    );
    assert!(
        !body.contains(dir.path().to_str().expect("utf8 path")),
        "a path leaked: {body}"
    );
}

/// A store whose writes are slow, so a request timeout lands mid-upload.
///
/// The upload runs on its own runtime. Dropping a runtime waits for the
/// blocking jobs it started, so once it is gone the cancelled job has
/// noticed the cancellation and cleaned up, and the check needs no sleep and
/// no polling.
#[test]
fn a_cancelled_upload_leaves_no_committed_blob_behind() {
    let u = uploads();
    let options = UploadOptions::new("avatar", &["image/png"], 8 << 20)
        .expect("options")
        .max_files(6);
    // Six large files: staging them takes much longer than the timeout.
    let big = [PNG, &vec![7_u8; 6 << 20]].concat();
    let parts: Vec<Part> = (0..6)
        .map(|_| file("avatar", "a.png", "image/png", &big))
        .collect();
    let request = upload_request(multipart(&parts));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let outcome = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(1),
            receive(&request, &u.store, &options),
        )
        .await
    });
    assert!(
        outcome.is_err(),
        "the upload was expected to be cut off by the timeout"
    );
    // Waits for the blocking job to finish.
    drop(runtime);
    assert_eq!(
        u.blobs(),
        0,
        "a cancelled upload left files behind (blobs, metadata or staging)"
    );
}

/// A failed overwrite must not destroy the record of the blob that is still there.
#[tokio::test]
async fn a_failed_overwrite_keeps_the_previous_metadata() {
    let u = uploads();
    let root = u.dir.path().join("root");
    // `blobs/a` is a directory, so a blob named "a" can never be stored; its
    // metadata file exists from an earlier life.
    u.store
        .put(
            &key("a/b"),
            Bytes::from_static(b"x"),
            PutOptions::new(10).expect("o"),
        )
        .await
        .expect("setup");
    let record = r#"{"content_type":"text/plain","sha256":"00","size":1}"#;
    std::fs::write(root.join("meta/a.json"), record).expect("record");
    let result = u
        .store
        .put(
            &key("a"),
            Bytes::from_static(b"new"),
            PutOptions::new(10).expect("o"),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(root.join("meta/a.json")).expect("kept"),
        record
    );
    assert_eq!(std::fs::read_dir(root.join("tmp")).expect("tmp").count(), 0);
}

#[tokio::test]
async fn an_empty_text_field_does_not_swallow_the_file_after_it() {
    let u = uploads();
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n").as_bytes());
    body.extend_from_slice(PNG);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let got = receive(
        &upload_request(Bytes::from(body)),
        &u.store,
        &default_options(),
    )
    .await
    .expect("receive");
    assert_eq!(
        got.files.len(),
        1,
        "the file after an empty part was swallowed"
    );
    assert_eq!(got.fields["note"], vec![""]);
}

#[tokio::test]
async fn too_many_parts_are_refused_before_the_rest_is_examined() {
    let u = uploads();
    let options = default_options().text_fields(2, 100);
    // max parts = 1 file + 2 fields = 3; send four, then garbage.
    let mut body = multipart(&[
        text("a", "1"),
        text("b", "2"),
        text("c", "3"),
        text("d", "4"),
    ])
    .to_vec();
    body.truncate(body.len() - format!("--{BOUNDARY}--\r\n").len());
    body.extend_from_slice(format!("--{BOUNDARY}\r\nbroken").as_bytes());
    let error = receive(&upload_request(Bytes::from(body)), &u.store, &options)
        .await
        .expect_err("too many");
    assert!(
        error.to_string().contains("too many multipart parts"),
        "{error}"
    );
}
