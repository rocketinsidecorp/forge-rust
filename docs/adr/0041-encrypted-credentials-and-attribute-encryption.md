# ADR 0041: Encrypted credentials and attribute encryption

- **Status:** Accepted
- **Date:** 2026-10-20
- **Port of:** Forge-Go ADR 0036 (encrypted credentials and attribute encryption)

Guides: [credentials](../credentials.md) and [encryption](../encryption.md).
Threat analysis: the
[credentials and attribute encryption review](../threat-model.md#credentials-and-attribute-encryption-review).

## Context

Rails keeps per-environment secrets in `config/credentials/*.yml.enc`, edited
with `credentials:edit`, and encrypts model attributes with Active Record
Encryption. Forge-Go ships both (`forge credentials`, packages `credentials`
and `encryption`), fixing their weak spots: an encrypted file from one
environment can be dropped into another, the edit flow leaves plaintext in
temporary and swap files, deterministic encryption leaks equality, and
ciphertexts are not tied to their row. Rails parity lists the feature Planned
in Forge-Rust. The port must keep Forge-Go's commands, environment variable,
file names and formats (owner decision 2026-10-20: feature parity) under
Forge-Rust's rules: no custom cryptography, no new crates without admission,
hexagonal rings, secrets redacted and zeroized.

## Decision

1. **Two crates, Forge-Go's two packages.** `forge-encryption`
   (`forge::encryption`): AES-256-GCM, HKDF-SHA256 and HMAC-SHA256 from
   `ring`; key rings; `Cipher`; blind indexes; typed `Attribute` values;
   resumable re-encryption behind the `Rewriter` port with SQLx
   `EncryptedColumn` adapters for PostgreSQL, MySQL/MariaDB and SQLite (behind
   the backend features). The PostgreSQL adapter rotates row-level-security
   tables per tenant (`.tenant(id)`, `set_config('forge.tenant_id', id, true)`
   in every page and update transaction) and refuses an unscoped run when the
   policies apply to the connected role; completion is proven by a count of
   rows not on the active key, never by a report. `forge-credentials` (`forge::credentials`): the file
   format, key loading, atomic writes, `init` and the redacted `Store`. The
   `forge` facade re-exports both; the CLI uses `forge-credentials` directly.
2. **Byte-compatible formats.** The credentials file is Forge-Go's JSON
   envelope (`version` 1, `algorithm` `AES-256-GCM`, `key_id`, `nonce`,
   `ciphertext`, two-space indentation, final newline). The AEAD key is
   HKDF-SHA256(master, no salt, `forge/v1/credentials/aead`); `key_id` is the
   hex of the first 8 bytes of HKDF(`forge/v1/credentials/key-id`). Associated
   data is the length-prefixed tuple `("forge/credentials", version,
   environment, key_id)`; the environment comes from the caller, so a staging
   file installed as production's fails. Attribute values are
   `fg1.<key id>.<base64url(nonce || ciphertext || tag)>` with associated data
   `("forge/attribute/v1", key id, table, column, record id)` and per-purpose
   HKDF subkeys (`attributes/aead`, `attributes/blind-index`); the blind index
   is HMAC-SHA256 over `("forge/blind-index/v1", table, column) || plaintext`.
   Fixtures written by Forge-Go's packages are opened by the tests, and values
   written by Rust were opened by Forge-Go.
3. **Keys.** `FORGE_MASTER_KEY` (wins) or `config/credentials/<env>.key`:
   canonical unpadded base64url keys separated by commas or newlines, the
   first active, at most 8. A key file must be a regular file, not a symbolic
   link (`O_NOFOLLOW`), with no group or other permission bits. Key files are
   written 0600 in a 0700 directory with a temporary file, `fsync` and
   `rename` (or a hard link when the file must not exist yet).
4. **Documents** are one JSON object without duplicate keys at any depth, at
   most 1 MiB; errors name a line and column, never content.
5. **`forge credentials init|edit|show|rotate-key`** with Forge-Go's flags,
   output and refusals: the edit flow decrypts into a 0600 file in a fresh 0700
   directory (`/dev/shm` on Linux), runs `$VISUAL`/`$EDITOR` as an argument
   vector without a shell and without `FORGE_*` variables (Vim-family editors
   without swap, backup, undo or viminfo), catches `SIGINT`, `SIGTERM` and
   `SIGHUP` for the whole command (one signal thread, joined on exit) and acts
   on them at checkpoints, validates, refuses a file changed meanwhile, writes
   atomically, and zeroes and removes the directory on every exit through a
   drop guard. `show` redacts unless `--reveal` on a terminal. `rotate-key`
   re-encrypts only when the new key has a destination and orders its writes
   so the on-disk pair is always readable.
6. **Fail closed.** Outside `development` and `test` a missing file, key or
   required path is an error at `open`; in every environment a file that does
   not decrypt is an error.
7. **No deterministic mode** (Forge-Go has none): equality lookups use the
   opt-in blind index in a separate column.
8. **Redaction and wiping.** `KeyMaterial`, `Key`, `KeyRing`, `Cipher`,
   `Aead` and `Store` print no secret. Key material, derived keys, decrypted
   plaintexts, key-file text and the store's strings are `zeroize`d on drop.
9. **Generated applications.** `forge new` writes the development key and file
   and the ignore entries, and the README documents the commands and column
   encryption. Like Forge-Go, bootstrap does not load credentials by default:
   the image does not carry the encrypted file, and loading it in production
   would stop every existing deployment. Applications call
   `forge::credentials::open` in their composition root when they adopt it.

## Alternatives considered

- **RustCrypto `aes-gcm`, `hkdf`.** `hkdf` is locked but `aes-gcm` is not;
  `ring` already provides all three primitives, is linked into every
  application for TLS and is the AES-GCM of the MFA seeds (ADR 0040).
- **YAML documents like Rails.** Forge-Go chose JSON; the encrypted file is
  opaque and compatibility wins.
- **`sqlx::Encode`/`Decode` for an encrypted value type (Forge-Go's
  `Value[T]`).** Encoding needs the cipher and the row id inside the bound
  value and would put SQLx types in a Forge API; `Attribute::seal`/`open`
  produce and consume the stored text instead.
- **Loading credentials in generated bootstrap.** Rejected for now, see
  decision 9; it is one call when an application wants it.
- **Deterministic encryption, per-record data keys through a KMS,
  `forge generate scaffold field:string:encrypted`.** Not in Forge-Go either;
  the scaffold flag and a KMS-backed `Cipher` remain on both roadmaps.

## Consequences

- A repository can hold production secrets: without the key an attacker learns
  file sizes and key ids.
- Losing every copy of a key makes the file, and data under keys stored only in
  it, unrecoverable, by design.
- `FORGE_MASTER_KEY` is as exposed as any environment variable; deployments that
  cannot accept that mount a key file instead.
- `zeroize` becomes a direct dependency (already locked through rustls); no new
  package enters `Cargo.lock`.
- `ring`'s expanded key schedules and copies the application makes are not
  wiped.
