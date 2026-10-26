//! `multipart/form-data` uploads (`forge_storage::receive`, ADR 0036): the
//! boundary and header parsing, the part grammar, the limits and the
//! content-type decision, against an in-memory store.
//!
//! Input: one byte choosing the limits, the request `Content-Type` up to a
//! NUL (the default one without it), then the body.
//!
//! Properties: no panic on any body; a refused upload leaves nothing in the
//! store; an accepted one stores at most `max_files` non-empty files within the
//! size bound, each under a valid UUID key below the prefix, with a content
//! type that is allowed and, when the bytes are recognizable, the one the
//! bytes say; a stored file is a slice of the request body; a stored file name
//! is already sanitized; the text fields stay within their count and byte
//! bounds; and the same body gets the same verdict twice.
#![no_main]

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use forge_http::{HeaderMap, HeaderValue, Method, Request, Uri, header};
use forge_storage::MAX_KEY_LENGTH;
use forge_storage::{
    Blob, Info, PutOptions, Received, Storage, StorageError, StorageKey, UploadOptions,
    normalize_content_type, receive, sanitize_filename, sniff_content_type,
};
use libfuzzer_sys::fuzz_target;

const ALLOWED: [&str; 3] = ["image/png", "image/jpeg", "text/plain"];
const DEFAULT_CONTENT_TYPE: &str = "multipart/form-data; boundary=----forge-fuzz";
const MAX_NAME: usize = 256;

/// Blobs in memory; every future is ready at once.
#[derive(Default)]
struct Memory {
    blobs: Mutex<BTreeMap<String, (Bytes, Info)>>,
}

#[async_trait]
impl Storage for Memory {
    async fn put(
        &self,
        key: &StorageKey,
        body: Bytes,
        options: PutOptions,
    ) -> Result<Info, StorageError> {
        if body.len() as u64 > options.max_bytes() {
            return Err(StorageError::TooLarge);
        }
        let info = Info {
            key: key.clone(),
            size: body.len() as u64,
            content_type: options
                .declared_content_type()
                .map_or_else(|| "application/octet-stream".to_owned(), str::to_owned),
            filename: options.declared_filename().map(str::to_owned),
            sha256: None,
            modified: SystemTime::UNIX_EPOCH,
        };
        self.blobs
            .lock()
            .expect("blobs")
            .insert(key.as_str().to_owned(), (body, info.clone()));
        Ok(info)
    }

    async fn open(&self, key: &StorageKey, _max_bytes: u64) -> Result<Blob, StorageError> {
        let blobs = self.blobs.lock().expect("blobs");
        let (body, info) = blobs.get(key.as_str()).ok_or(StorageError::NotFound)?;
        Ok(Blob {
            info: info.clone(),
            body: body.clone(),
        })
    }

    async fn stat(&self, key: &StorageKey) -> Result<Info, StorageError> {
        let blobs = self.blobs.lock().expect("blobs");
        blobs
            .get(key.as_str())
            .map(|(_, info)| info.clone())
            .ok_or(StorageError::NotFound)
    }

    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        self.blobs.lock().expect("blobs").remove(key.as_str());
        Ok(())
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}

struct Limits {
    max_files: usize,
    max_file_bytes: u64,
    max_fields: usize,
    max_field_bytes: u64,
    optional_file: bool,
    prefix: Option<&'static str>,
}

impl Limits {
    fn from_byte(byte: u8) -> Self {
        Self {
            max_files: [1, 2, 3, 8][usize::from(byte & 3)],
            max_file_bytes: if byte & 32 == 0 { 1 << 20 } else { 32 },
            max_fields: if byte & 16 == 0 { 32 } else { 2 },
            max_field_bytes: if byte & 16 == 0 { 64 << 10 } else { 16 },
            optional_file: byte & 4 != 0,
            prefix: (byte & 8 != 0).then_some("uploads/fuzz"),
        }
    }

    fn options(&self) -> UploadOptions {
        let mut options = UploadOptions::new("file", &ALLOWED, self.max_file_bytes)
            .expect("valid options")
            .max_files(self.max_files)
            .text_fields(self.max_fields, self.max_field_bytes)
            .optional_file(self.optional_file);
        if let Some(prefix) = self.prefix {
            options = options.prefix(prefix);
        }
        options
    }
}

fn request(content_type: &str, body: &[u8]) -> Option<Request> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).ok()?,
    );
    Some(Request::new_for_test(
        Method::POST,
        Uri::from_static("/uploads"),
        headers,
        Bytes::copy_from_slice(body),
        SocketAddr::from(([127, 0, 0, 1], 9)),
        uuid::Uuid::nil(),
    ))
}

fn contained(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn check_accepted(limits: &Limits, body: &[u8], received: &Received, store: &Memory) {
    let blobs = store.blobs.lock().expect("blobs");
    assert_eq!(
        blobs.len(),
        received.files.len(),
        "stored and reported files differ"
    );
    assert!(received.files.len() <= limits.max_files);
    assert!(
        limits.optional_file || !received.files.is_empty(),
        "an upload without a file was accepted"
    );
    for info in &received.files {
        let key = info.key.as_str();
        assert!(key.len() <= MAX_KEY_LENGTH);
        assert_eq!(StorageKey::parse(key).ok().as_ref(), Some(&info.key));
        match limits.prefix {
            Some(prefix) => assert!(
                key.strip_prefix(prefix)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .is_some_and(|id| id.len() == 36 && !id.contains('/')),
                "{key:?} is not a UUID below {prefix:?}"
            ),
            None => assert_eq!(key.len(), 36, "{key:?}"),
        }
        let (content, stored) = blobs.get(key).expect("a reported file is stored");
        assert_eq!(stored, info);
        assert_eq!(info.size, content.len() as u64);
        assert!(
            !content.is_empty() && info.size <= limits.max_file_bytes,
            "{} bytes under a bound of {}",
            info.size,
            limits.max_file_bytes
        );
        assert!(
            contained(body, content),
            "a stored file is not part of the request"
        );
        assert!(
            ALLOWED.contains(&info.content_type.as_str()),
            "{}",
            info.content_type
        );
        assert_eq!(
            normalize_content_type(&info.content_type).ok().as_deref(),
            Some(info.content_type.as_str())
        );
        // What the bytes say wins over what the client labelled.
        let sniffed = sniff_content_type(content);
        if !matches!(
            sniffed,
            "text/plain" | "application/octet-stream" | "text/xml"
        ) {
            assert_eq!(info.content_type, sniffed, "the label overrode the bytes");
        }
        if let Some(name) = &info.filename {
            assert_eq!(
                &sanitize_filename(name),
                name,
                "an unsanitized file name {name:?}"
            );
            assert!(name.len() <= 255 && !name.starts_with('.'));
            assert!(!name.contains(['/', '\\', '"']) && !name.chars().any(char::is_control));
        }
    }
    let values: usize = received.fields.values().map(Vec::len).sum();
    assert!(values <= limits.max_fields, "{values} text fields");
    let mut total = 0_u64;
    for (name, list) in &received.fields {
        assert!(
            !name.is_empty() && name.len() <= MAX_NAME,
            "field name {name:?}"
        );
        for value in list {
            assert!(value.len() as u64 <= limits.max_field_bytes);
            total += value.len() as u64;
        }
    }
    assert!(total <= limits.max_fields as u64 * limits.max_field_bytes);
}

fuzz_target!(|data: &[u8]| {
    let Some((&choice, rest)) = data.split_first() else {
        return;
    };
    let limits = Limits::from_byte(choice);
    let (content_type, body) = match rest.iter().position(|&byte| byte == 0) {
        Some(end) => match std::str::from_utf8(&rest[..end]) {
            Ok(content_type) => (content_type, &rest[end + 1..]),
            Err(_) => return,
        },
        None => (DEFAULT_CONTENT_TYPE, rest),
    };
    let Some(request) = request(content_type, body) else {
        return;
    };
    let options = limits.options();

    let store = Memory::default();
    let verdict = block_on(receive(&request, &store, &options));
    match &verdict {
        Ok(received) => check_accepted(&limits, body, received, &store),
        Err(error) => {
            assert!(
                store.blobs.lock().expect("blobs").is_empty(),
                "a refused upload ({error}) left files behind"
            );
            assert!(
                matches!(error.status().as_u16(), 400 | 413 | 415),
                "{error}"
            );
        }
    }

    // The same request gets the same verdict from a fresh store.
    let again = Memory::default();
    let second = block_on(receive(&request, &again, &options));
    match (&verdict, &second) {
        (Ok(first), Ok(second)) => {
            assert_eq!(first.files.len(), second.files.len());
            assert_eq!(first.fields, second.fields);
        }
        (Err(first), Err(second)) => assert_eq!(first.code(), second.code()),
        _ => panic!("the same upload was accepted once and refused once"),
    }
});
