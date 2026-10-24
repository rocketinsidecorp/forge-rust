# File storage

`forge-storage` is the equivalent of Rails' Active Storage for local disk: a
`Storage` port, a disk adapter, HMAC-signed expiring links, an HTTP handler that
serves them safely, and a strict multipart upload helper. The design and its
trade-offs are in [ADR 0029](adr/0029-file-storage.md).

An S3-compatible adapter is **not** included; see
[Writing another adapter](#writing-another-adapter).

```toml
# Cargo.toml of the application (re-exported as `forge::storage`)
forge = { version = "0.1", features = ["storage"] }
```

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `FORGE_STORAGE_ROOT` | unset (storage not used) | Directory of the disk adapter. In `production`: an absolute path on a persistent volume. |
| `FORGE_STORAGE_SIGNING_KEY` | unset | HMAC key of signed links: unpadded base64url of 32 to 1024 random bytes. Redacted. **Required in `production` whenever `FORGE_STORAGE_ROOT` is set**; there is no default key. |
| `FORGE_STORAGE_MAX_UPLOAD_BYTES` | `524288` | Largest uploaded file; at most `1073741824`, and when storage is configured not above `FORGE_MAX_BODY_BYTES` |
| `FORGE_STORAGE_LINK_TTL_SECS` | `900` | Default lifetime of signed links; `1` to `604800` |

```sh
openssl rand -base64 32 | tr '+/' '-_' | tr -d '='   # a signing key
export FORGE_STORAGE_ROOT=/var/lib/shop/storage
export FORGE_STORAGE_SIGNING_KEY=$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')
```

`FORGE_MAX_BODY_BYTES` (default 1 MiB) caps **every** request body and an
upload route cannot raise it; raise it with the upload limit (multipart framing
adds a few hundred bytes per part) and remember that it applies to all routes.
`forge-http` buffers whole bodies and responses, so memory use is bounded by
these limits; keep them small.

## Wiring

```rust
use std::sync::Arc;
use forge::storage::{DiskStorage, HandlerOptions, Signer, Storage, StorageHandler};

let config = composition.config();
let store: Arc<dyn Storage> = Arc::new(DiskStorage::from_config(&config.storage)?); // creates blobs/, meta/, tmp/
let signer = Arc::new(Signer::from_config(&config.storage)?);                    // fails without a signing key
router.layer(StorageHandler::new(store.clone(), signer.clone(), HandlerOptions::default())?);
// GET /files/<key>?expires=..&sig=..  is now served; every other request passes on.
```

Hand out links:

```rust
use forge::storage::SignOptions;

let url = signer.url(
    "/files",
    &info.key,
    &SignOptions::default()
        .ttl(config.storage.link_ttl)
        .inline(true)                          // images only; everything else downloads
        .binding(principal.id().to_string()),  // optional: the link works for this user only
)?;
```

and, if you bind links, give the handler the same value on the request:

```rust
HandlerOptions::default().binding(|request| user_id_of(request))
```

A link is `/files/<key>?expires=<unix>&sig=<base64url>[&disposition=inline]`.
Verification is HMAC-SHA256 with constant-time comparison; a wrong key, tampered
parameter, extended expiry, duplicated parameter, percent-encoded value or
different binding is `403 link_invalid`, an authentic link past its time is
`403 link_expired`. Both are answered **before** the store is consulted, so the
handler does not reveal which keys exist.

### Rotating the signing key

`Signer::new(&new_key, &[&old_key])` signs with the first key and accepts both.
Deploy with both, wait for the longest link lifetime (at most 7 days), then
drop the old key. Rotation is the only way to revoke links before they expire.

## Uploading

```rust
use forge::storage::{UploadOptions, receive};

async fn create_avatar(request: Request) -> Response {
    // Authenticate, authorize and CSRF-check before reading the body.
    let options = match UploadOptions::new("avatar", &["image/png", "image/jpeg", "image/webp"], config.storage.max_upload_bytes) {
        Ok(options) => options.prefix("avatars"),
        Err(error) => return error.into_response(&request),
    };
    match receive(&request, store.as_ref(), &options).await {
        Ok(received) => {
            let info = &received.files[0]; // info.key is "avatars/<uuidv7>"; persist the key, not a URL
            Response::new(StatusCode::CREATED, info.key.to_string())
        }
        Err(error) => error.into_response(&request), // 415, 413, 400 in the JSON error format
    }
}
```

Multipart bodies are not read by the form CSRF layer (it reads urlencoded
forms): send the token in the `x-csrf-token` header from a `fetch`/Stimulus
submit, or check the field you get back in `received.fields`.

Files are written from the buffered body without an extra copy; text fields
(form values next to the file) are held in memory, so they are limited to
`max_fields` values in total, repeated names included (default 32), and
`max_fields x max_field_bytes` bytes (`text_fields`, default 2 MiB).

`receive` rejects, with a stable error and without storing anything:

| Case | Result |
| --- | --- |
| request is not `multipart/form-data` (or has no valid boundary) | `415 unsupported_media_type` |
| body, file or text field over its limit | `413 storage_too_large` |
| content whose detected type is not allowed, or a label that does not match the content | `415 unsupported_media_type` |
| file in another field, more files than `max_files`, empty file, unnamed part, non-identity `Content-Transfer-Encoding`, too many text fields, malformed or non-canonical body | `400 invalid_upload` |
| no file at all | `StorageError::NoFile` (`400 upload_missing`); use `optional_file(true)` to receive the fields of such a submission |

A multi-file upload is stored as one unit (`Storage::put_all`): if a store
failure interrupts it, or the request is cancelled (for example by the request
timeout) before publishing starts, no file of the request is kept; staged files
in `tmp/` are removed by the job or by `prune_temp`. A key the batch had
already overwritten gets its previous blob and metadata back (the disk adapter
keeps them as hard links in `tmp/` until the batch commits), so a failed batch
never deletes data that existed before it.

### How the content type is decided

The client's `Content-Type` and file name are claims, not facts. `receive`
sniffs the first 512 bytes (`sniff_content_type`):

1. If the bytes identify a specific type (image, PDF, archive, HTML, XML...),
   that type is stored and **must** be in the allow-list. The label is
   irrelevant: an HTML page labelled `image/png` is refused unless `text/html`
   is allowed, and a PDF labelled `image/png` is stored as a PDF only if PDF is
   allowed. A polyglot that starts as a GIF is a GIF.
2. If the bytes have no signature (plain text, CSV, JSON, SVG source...), the
   declared type is stored if it is allowed, except that types that must carry a
   signature (images, audio, video, fonts, PDF, archives) are refused, and text
   types (`text/*`, JSON, XML) need text content (binary data labelled
   `text/csv` is refused).
3. SVG with an XML prolog is sniffed as XML and stored as `image/svg+xml` only
   when you allow that type; it is always served as a download.

The allow-list is mandatory and has no wildcards. Office documents are
`application/zip` to the sniffer: allow `application/zip` explicitly if you
accept them (they are stored under that type).

The stored name is metadata only: the key is a UUIDv7, and `sanitize_filename`
removes directories, control and bidirectional characters, quotes and leading
dots before it is stored or put in a header.

## Serving: what the browser is allowed to do

Every response has `X-Content-Type-Options: nosniff`, a sandboxing
`Content-Security-Policy` (`default-src 'none'; sandbox; frame-ancestors
'none'`), `Cross-Origin-Resource-Policy: same-site` and `Cache-Control: private`.
**Only raster images** (`image/png|jpeg|gif|webp|avif|bmp|x-icon`) can be shown
inline, and only when the link was signed with `inline(true)`; HTML, SVG, XML,
JavaScript, PDF, text and unknown types are always `Content-Disposition:
attachment; filename="..."`. Extend the inline set with
`HandlerOptions::inline_types` only for types that cannot run code on your
origin. `HEAD`, `ETag` and `If-None-Match` work. **Byte ranges are not served**
and blobs over `max_serve_bytes` (default 16 MiB) answer `413`, because
`forge-http` sends whole buffered responses.

Serve user files from a separate origin (a subdomain or domain that has no
cookies for the application) when you can; the headers above are defense in
depth, not a substitute.

## Disk layout and operations

```text
<root>/blobs/<key>        content
<root>/meta/<key>.json    content type, file name, size, SHA-256
<root>/tmp/               uploads in progress
```

- Writes are atomic (temp file, `fsync`, `rename`); a failed or oversized write
  leaves the previous blob untouched. Paths are resolved with `openat` and
  `O_NOFOLLOW` per component, so symlink escapes are refused by the operating
  system as well as by key validation. Files are `0600`, directories `0750`: run
  the container as the user that owns the volume (the generated Dockerfile runs
  as `nonroot`).
- A crash can leave files in `tmp/`: run `store.prune_temp(Duration::from_secs(3600))`
  periodically (for example from a scheduled job).
- Metadata that is missing, unreadable or disagrees with the file's size is
  ignored and the blob is served as `application/octet-stream` (a download).
  Each record names the inode of its blob; a record for another inode (the
  instant between the two renames of an overwrite, or a restored backup) is
  trusted only when the content matches its SHA-256, which reads the blob once
  more; the verified record is then rewritten with the blob's inode, so later
  reads are cheap again.
- `prune_temp` ages a temporary file by the later of its modification time and
  the UUIDv7 its name starts with: an overwrite's backup (a hard link that
  keeps the old blob's modification time) is never pruned while the write is
  in progress.
- Back up `blobs/` and `meta/` together. Disk storage is for one host, or hosts
  sharing a volume with working `rename` and `fsync` (not NFS). The disk adapter
  is Unix only.
- Keys of user content are UUIDv7 (`receive`); keys you choose must satisfy
  `StorageKey::parse`: slash-separated segments of `[A-Za-z0-9._-]`, no empty
  segment, no segment starting with a dot, at most 512 bytes and 8 segments.

## Writing another adapter

The `Storage` trait has four methods (`put`, `open`, `stat`, `delete`) over
`StorageKey` and `Bytes`. An adapter for an object store implements them in its
own crate, enforces `PutOptions::max_bytes` (failing with
`StorageError::TooLarge`, leaving nothing) and the `open` limit, keeps content
type and file name as object metadata, maps "no such object" to
`StorageError::NotFound`, and passes `forge_storage::testing::run` (feature
`testing`). The signer, handler and upload helper work unchanged. Forge does not
ship an S3 adapter yet: signing S3 requests (SigV4) is a protocol of its own that
cannot be verified here against a real service, and a placeholder is not
acceptable.

## Testing

`forge_storage::testing::run(|| async { store })` is the conformance suite
(round trips, nested keys, overwrite atomicity under concurrency, size-limit
boundary, idempotent delete, malformed content types, binary content).
`crates/forge-storage/tests/` shows the disk adapter against planted symlinks and
traversal, and `receive`, the handler and the signer end to end through
`forge_http::Router`.
