# ADR 0029: File storage

- Status: Accepted
- Date: 2026-10-19
- Refines: [ADR 0022](0022-server-rendered-views.md) (full-stack applications handle uploads)
- Ported from: Forge-Go ADR 0033 (same contracts and negative tests)

## Context

Rails' Active Storage attaches files to records, stores them on a service
(disk, S3, ...) and serves them through signed, expiring URLs. The dangerous
parts are well known: path traversal in the store, serving a user-supplied HTML
or SVG file from the application origin (stored XSS), trusting the client's
content type, unbounded uploads, partial files after a crash, and URLs that
outlive the permission that created them. `forge-storage` is already named in
the workspace map ("object storage port and adapters").

## Decision

### Crate

`forge-storage` holds the port, the disk adapter, the signer, the serving
handler, the multipart parser and the upload helper. It has no database
dependency; its place in the rings is the HTTP/infrastructure edge. Application
code depends on the `Storage` trait (or its own narrower port), never on the
disk adapter.

### The port

```rust
trait Storage {
    async fn put(&self, key: &StorageKey, body: Bytes, options: PutOptions) -> Result<Info, StorageError>;
    async fn open(&self, key: &StorageKey, max_bytes: u64) -> Result<Blob, StorageError>;
    async fn stat(&self, key: &StorageKey) -> Result<Info, StorageError>;
    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError>;
}
```

`forge-http` buffers each request body (bounded by `FORGE_MAX_BODY_BYTES`) and
sends whole `Bytes` responses, so, unlike Forge-Go's `io.Reader` port, this
port moves `Bytes` and bounds reads explicitly: `PutOptions::new(max_bytes)` is
required (there is no unbounded `put`), and `open` takes the largest blob the
caller is willing to hold in memory. A failed or oversized `put` leaves no blob
and does not replace an existing one. An object-store adapter implements the
four methods and reuses the signer, handler and upload helper unchanged.

**No S3 adapter ships.** A correct SigV4 implementation is a signing protocol
(canonical requests, regional scopes, streaming and multipart edge cases,
presigned URLs, vendor endpoints) that cannot be verified here against a real
service, and a placeholder adapter is forbidden by the project rules.

### Keys are restricted by construction

`StorageKey::parse` is the only way to make a key: slash-separated segments of
`[A-Za-z0-9._-]`, at most 512 bytes, 8 segments and 128 bytes per segment, no
empty segment and **no segment starting with a dot**. `..`, `.`, hidden files,
absolute paths, backslashes, drive letters, percent escapes, NUL and non-ASCII
therefore cannot be expressed, in every adapter, before any file-system call:
traversal needs no check at a call site because an unsafe key does not exist as
a value. Keys for user content are UUIDv7 (`StorageKey::unique`); the original
file name is metadata, never part of the key.

### Disk adapter (Unix)

- **No symlink escapes, no race.** Every path below the root is resolved with
  `openat`-style calls relative to directory file descriptors, one component at
  a time, with `O_NOFOLLOW` on every component, so the kernel refuses a link
  anywhere below the root with no check-then-use window. The descriptors of
  `blobs/`, `meta/` and `tmp/` are opened once (a link in their place at startup
  is refused) so replacing them later changes nothing. This is the behavior of
  Go's `os.Root`; it is implemented with `rustix` (already in the lockfile
  through `tempfile`; safe API, no `unsafe` in Forge code) because `cap-std` is
  not admitted and a canonicalize-and-compare would be racy.
- Writes are atomic: `tmp/<uuid>` created with `O_EXCL`, digest computed,
  `fsync`, then `rename` onto `blobs/<key>` (metadata first, the blob is the
  commit point), then the directories are flushed. Readers see the old file or
  the new one. A crash leaves at most an orphan in `tmp/`, which
  `prune_temp(older_than)` removes (refusing ages under a minute so it cannot
  delete uploads in progress).
- Content type, file name, size and SHA-256 live in `meta/<key>.json`, trusted
  only while its recorded size equals the file's size and read through the same
  link-refusing walk with an 8 KiB cap; otherwise the blob is
  `application/octet-stream` (a download).
- Files are `0600`, directories `0750`. Windows is not supported by this
  adapter (the crate compiles there without it).
- Blocking file work runs on Tokio's blocking pool and is awaited by the caller;
  it is never detached. Dropping the future abandons the result, not a syscall in
  flight, so a multi-file upload goes through `Storage::put_all`: the disk adapter
  runs it as one blocking job that first **stages** every file in `tmp/` and only
  then **publishes** them all (metadata first, blob last). The awaiting future
  raises a flag when it is dropped; the job checks it before publishing and, if
  set, discards the staged files, so a request timeout cannot leave the first
  files of an upload committed. A failure while publishing removes what was
  already published. Once publishing has started the job completes (the same as a
  client that times out after the server committed). A crash leaves at most staged
  files in `tmp/`, which `prune_temp` removes.
- A single `put` keeps the previous metadata aside while it replaces it, so a
  failed overwrite restores the existing record instead of deleting it.
- Disk storage is for one host (or hosts sharing a volume with working
  `rename`/`fsync`, not NFS).

### Signed links

`Signer` signs `HMAC-SHA256(key, "forge.storage.url.v1\n" key "\n" expires "\n"
disposition "\n" binding)` with the RustCrypto `hmac` and `sha2` crates (no
custom cryptography). The computed MAC is compared with `subtle`'s constant-time
equality against **every** configured key (no early exit), and the signature is
checked **before** the store is consulted. A link carries `expires` (unix
seconds), `sig` (unpadded base64url) and optionally `disposition=inline`.
Parsing is strict: exactly one `expires` (canonical decimal, no leading zeros) and one `sig` (canonical
43-character signature), at most one `disposition`, no percent-decoding: there
is no second spelling of a valid link. `ExpiredLink` is only returned for an
*authentic* expired link, so expiry cannot be used to learn anything about
forged ones.

- Lifetime defaults to 15 minutes, at most 7 days (`FORGE_STORAGE_LINK_TTL_SECS`
  is the application's default to pass to `SignOptions::ttl`).
- Keys are at least 32 bytes; rotation is `Signer::new(new, &[old])`: signs with
  the first, accepts all.
- `SignOptions::binding` ties a link to the principal (session or user ID): the
  handler recomputes it with `HandlerOptions::binding`, so a forwarded link stops
  working for anyone else.
- There is **no default key**: `Signer::from_config` fails without
  `FORGE_STORAGE_SIGNING_KEY`, and `forge-config` makes it mandatory in
  production whenever a storage root is configured.

### Handler

`StorageHandler` is a `forge_http::Middleware` mounted at a path (`/files`):
`forge-http` routes whole segments only, so a layer is the way to serve keys of
any depth. Order of work: method (`GET`/`HEAD`, otherwise `405`),
**signature, expiry, binding**, and only then the store, so an unauthenticated
caller cannot tell whether a key exists (the same `403` for missing and
existing keys). Responses:

- `Content-Type` is the stored, normalized type; `X-Content-Type-Options:
  nosniff`; `Content-Security-Policy: default-src 'none'; sandbox;
  frame-ancestors 'none'`; `Cross-Origin-Resource-Policy: same-site`;
  `Cache-Control: private, max-age=<remaining link life, at most 1h>`; `ETag`
  from the stored digest with `If-None-Match` support.
- **Inline only for raster images** (PNG, JPEG, GIF, WebP, AVIF, BMP, ICO) and
  only when the link asked for it. HTML, SVG, XML, scripts, PDF, plain text and
  everything unknown are `Content-Disposition: attachment; filename=...`
  (sanitized; RFC 5987 `filename*` for non-ASCII). Applications can extend the
  inline set with types they know are inert, never `application/octet-stream`.
- Header values are built from validated or sanitized parts and a value that is
  not a legal header is dropped; stored metadata cannot inject headers.
- Blobs over `max_serve_bytes` (default 16 MiB) are `413`: responses are
  buffered. Byte ranges are not served (a deliberate difference from Forge-Go,
  whose responses stream).

### Uploads

`receive(request, store, UploadOptions)` is deliberately strict:

- the request `Content-Type` must be `multipart/form-data` with a valid boundary
  (415 otherwise);
- the body is capped at `max_files x max_file_bytes + max_fields x
  max_field_bytes + 64 KiB` before it is parsed, on top of the server cap that
  already bounded its buffering;
- the parser (`multipart.rs`) is stricter than the RFCs: no preamble or
  epilogue, no transport padding, no folded or bare-LF lines, at most 16 headers
  of 8 KiB per part, exactly one `Content-Disposition: form-data` with a name,
  no repeated parameter, identity transfer encodings only. File contents are
  ranges into the buffered body, never copied before `put`;
- one expected file field; files in other fields, extra files, empty files,
  unnamed parts and surplus text fields are rejected (400/413). Text fields are
  bounded in **count** (every repetition of a name counts, default 32) and in
  **bytes** (each at most `max_field_bytes`, total at most `max_fields x
  max_field_bytes`);
- `allowed_types` is **mandatory**, has no wildcard, and the decision uses the
  type **detected from the bytes** (`sniff_content_type`): a file whose content
  is recognizably an image, PDF, archive, HTML or XML must be one of the allowed
  types (the client's label is irrelevant, so an HTML page labelled `image/png`
  is refused); a file with no signature keeps its declared type if allowed,
  except that types that must carry a signature (images, audio, video, fonts,
  PDF, archives) and text types (which need text content) are refused when the
  content does not match;
- the part count is bounded while parsing (`max_files + max_fields`) and an empty
  part cannot swallow the next one;
- everything is validated before anything is stored; the files are then stored by
  `put_all` as one unit (all or none, also when the request is cancelled; see
  the disk adapter);
- keys are UUIDv7 under an optional validated prefix; the file name is sanitized
  (no directories, control or bidirectional characters, quotes, leading dots) and
  kept only as metadata.

### Multipart: own parser, no new dependency

Admitting `multer` (or similar) was weighed and rejected. `multer` is a mature
streaming parser, but (a) it would add `mime`, `encoding_rs` and `spin` and a
`futures` stream API to a hostile-input path, which the dependency policy
reserves for security-owner review that cannot be obtained in this change; and
(b) its streaming design solves a problem Forge does not have: `forge-http`
hands handlers the complete, already size-capped body, so a parser over a slice
is a few hundred lines of straight-line code with no state machine across
chunks, no buffering policy and no async. The parser is deliberately stricter
than `multer`, is covered by an exhaustive table of rejected inputs, header and
name budgets, and a deterministic differential/mutation test (random bodies
encoded by an independent encoder must parse back exactly; mutated bodies must
never panic or yield out-of-range slices). If `forge-http` later streams bodies,
the parser should be replaced by an admitted streaming crate rather than grown.

### Content sniffing: own table, no new dependency

`sniff_content_type` implements the signature families of the WHATWG MIME
Sniffing Standard that a file store must tell apart (images, PDF, archives,
audio/video, fonts, HTML tags with case and whitespace handling, XML) and
decides `text/plain` versus `application/octet-stream` by binary data bytes,
looking at the first 512 bytes like Go's `http.DetectContentType`. It is a
conservative classifier for an allow-list, not a general MIME database; an
unknown binary is `application/octet-stream` and is only accepted when that type
is explicitly allowed. `infer`/`mime_guess` were not admitted for the same
reason as `multer`.

### Configuration

`FORGE_STORAGE_ROOT`, `FORGE_STORAGE_SIGNING_KEY` (redacted, unpadded base64url
of 32 to 1024 bytes, never echoed in errors), `FORGE_STORAGE_MAX_UPLOAD_BYTES`
(default 512 KiB), `FORGE_STORAGE_LINK_TTL_SECS` (default 900). In production,
with a root: an absolute path and a signing key are required, and the upload
limit may not exceed `FORGE_MAX_BODY_BYTES`.

## Consequences

- A local-disk Active Storage equivalent with the dangerous defaults closed.
- Files are never an XSS vector from the application origin: active content is
  always a sandboxed download.
- Signed links are capabilities with a replay window equal to their lifetime;
  short ttls and bindings narrow it, they cannot close it (threat model).
- Disk storage is one host and Unix only; horizontal scale needs a future
  object-store adapter, which the port allows without core changes.
- Uploads and downloads are bounded by memory (`FORGE_MAX_BODY_BYTES`,
  `max_serve_bytes`) until `forge-http` can stream; that is why the defaults are
  small.
- There are no variants/transformations and no malware-scan hook; both are
  separate, dependency-heavy features.
- Differences from Forge-Go: `Bytes` port instead of `io.Reader`; no byte ranges;
  a layer instead of a `{key...}` route; `rustix` `openat` instead of
  `os.Root`; own sniffer and multipart parser; `FORGE_STORAGE_LINK_TTL_SECS`;
  `optional_file` instead of returning fields with `ErrNoFile`.
