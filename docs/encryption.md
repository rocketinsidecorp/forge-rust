# Attribute encryption

Encrypt sensitive columns (national ids, tokens, notes) with AES-256-GCM, with
key rotation and without deterministic encryption: Forge's equivalent of
Active Record Encryption, value-compatible with Forge-Go's `encryption`
package. Crate `forge-encryption` (`forge::encryption`), built on `ring`.
Design: [ADR 0041](adr/0041-encrypted-credentials-and-attribute-encryption.md).

## Setup

`forge new` and `forge credentials init` put a key ring in the
[credentials](credentials.md):

```json
{ "encryption": { "active": "k1", "keys": { "k1": "<base64url, 32 random bytes>" } } }
```

In `src/bootstrap`:

```rust
let credentials = forge::credentials::open(
    &environment,
    forge::credentials::Options::default().with_required(["encryption"]),
)?;
let cipher = std::sync::Arc::new(forge::encryption::Cipher::new(
    &credentials.key_ring(&["encryption"])?,
)?);
// hand `cipher` to the repository adapters that store encrypted columns
```

Without credentials, build a ring yourself (`generate_key`, `Key::new`,
`KeyRing::new`, `KeyRing::parse`). Keys are 32 bytes; their identifiers are 1-32
letters, digits, `_` or `-` and are stored in every ciphertext. Use names you
can read in a year: `2026-10`, not `1`.

## Encrypting a column

The column is `TEXT` (or `text`); the value looks like
`fg1.2026-10.<base64url>`. Each value is bound to its table, column and record
id. The record id is the row's primary key in its canonical text form (a
UUIDv7 generated in Rust before the insert, as every Forge repository does).
Encryption belongs in the infrastructure ring: the repository adapter seals
before it binds and opens after it reads, so entities keep plain values.

```rust
use forge::encryption::Json;

let ssn = cipher.attribute("people", "ssn");
let stored: String = ssn.seal(&id.to_string(), &person.ssn)?; // String
sqlx::query("INSERT INTO people (id, ssn) VALUES ($1, $2)")
    .bind(id)
    .bind(stored)
    .execute(pool)
    .await?;

let row = sqlx::query("SELECT id, ssn FROM people WHERE id = $1")
    .bind(id)
    .fetch_one(pool)
    .await?;
let plain: String = ssn.open(&id.to_string(), row.try_get::<&str, _>("ssn")?)?;

// Any serde type is stored as JSON; a nullable column uses the _optional pair.
let address = cipher.attribute("people", "address");
let text: Option<String> =
    address.seal_optional(&id.to_string(), person.address.clone().map(Json).as_ref())?;
```

`String` and `Vec<u8>` are stored as they are, `Json<T>` as JSON, exactly as
Forge-Go's `Value[T]` stores them, so either framework reads the other's
columns. A value copied to another row, column or table fails with
`EncryptionError::Decrypt`. Errors never include the plaintext; decrypted
bytes are wiped when dropped. The lower-level calls are
`cipher.encrypt(plaintext, &Aad::new(table, column, record_id))` and
`cipher.decrypt(text, &aad)`.

Encrypted columns cannot be searched, sorted or indexed by value. Do not
encrypt a column the database must filter on unless you add a blind index.
`forge generate scaffold` has no `:encrypted` field type (neither has
Forge-Go); encrypt in the generated repository adapter as above.

## Searching: blind index (opt-in)

Encryption is randomized: encrypting the same value twice gives different
text. For exact-match lookups add a second column holding a keyed digest:

```sql
ALTER TABLE people ADD COLUMN ssn_idx text;
CREATE INDEX people_ssn_idx ON people (ssn_idx);
```

```rust
let index = cipher.blind_index("people", "ssn", normalize(&ssn).as_bytes())?;
// INSERT ... (ssn, ssn_idx) VALUES ($1, $2)

let wanted = cipher.blind_indexes("people", "ssn", normalize(&input).as_bytes())?; // one per ring key
let rows = sqlx::query("SELECT id, ssn FROM people WHERE ssn_idx = ANY($1)")
    .bind(&wanted)
    .fetch_all(pool)
    .await?;
```

`normalize` (trim, lowercase, strip punctuation) is yours: the index matches
exact bytes. The index is HMAC-SHA256 under a key derived for this purpose and
bound to the table and column; without the key it reveals nothing about the
value, but anyone who can read the table sees which rows are equal and how
often each value occurs. Use it only for columns that need it, prefer
high-entropy values, and never for booleans or small enumerations. There is no
deterministic encryption mode.

## Rotating keys

1. Add the new key and make it active; keep the old one:

   ```sh
   forge credentials edit --environment production
   ```

   ```json
   { "encryption": { "active": "2026-10", "keys": { "2026-10": "<new>", "2025-03": "<old>" } } }
   ```

   Deploy. New writes use `2026-10`; old values still decrypt with `2025-03`.
   Lookups use `blind_indexes`, which returns a digest per key.

2. Re-encrypt existing rows, in bounded batches, safe to interrupt and rerun
   (from a job or an application command, one column at a time):

   ```rust
   use forge::core::Cancellation;
   use forge::encryption::{RotateOptions, SqlColumn, postgres::EncryptedColumn};

   let column = EncryptedColumn::new(
       database.clone(), // forge::db::PgDatabase
       SqlColumn::new("people", "ssn")?.with_index_column("ssn_idx")?, // index only with a blind index
   )?;
   let options = RotateOptions { batch_size: 500, blind_index: true, ..RotateOptions::default() };
   match cipher.reencrypt(&column, options.clone(), &cancellation).await {
       Ok(report) => tracing::info!(rotated = report.rotated, conflicts = report.conflicts, "rotated"),
       // On cancellation or error, error.report.last_id is the checkpoint:
       Err(error) => {
           let resume = RotateOptions { after: error.report.last_id.clone(), ..options };
           // ... cipher.reencrypt(&column, resume, &cancellation).await
       }
   }
   ```

   `forge::encryption::mysql::EncryptedColumn` and `sqlite::EncryptedColumn`
   work the same way on `MySqlDatabase` and `SqliteDatabase`. The key is a UUID
   by default (`uuid` on PostgreSQL, `BINARY(16)` on MySQL/MariaDB, `BLOB` on
   SQLite, as `forge generate` creates them); `SqlColumn::with_id_type` selects
   `IdType::Text` or `IdType::Integer`, and `with_id_column` another key column.
   Rows already on the active key are skipped; a row changed by the
   application during the run is left alone (counted in `report.conflicts`)
   and picked up by the next run, because every write is a compare-and-set on
   the value read. A row that cannot be decrypted stops the run with its id:
   investigate, do not skip.

3. When a second run reports `rotated == 0` and `conflicts == 0`, remove the
   old key from the credentials and deploy. A value still sealed with a
   removed key fails with `EncryptionError::UnknownKey`.

Rotate before about 2^32 values have been encrypted under one key (random
96-bit nonces; NIST SP 800-38D), and on any suspicion of exposure.

## Guarantees and limits

- Confidentiality and integrity per value (AES-256-GCM); the key id, table,
  column and record id are authenticated.
- Not hidden: that a value exists (NULL or not), its approximate length,
  equal values within a blind-indexed column, the key id.
- A ciphertext is not versioned against replay: an old valid ciphertext for
  the same row and column can be restored by someone who can write the table.
  Use a version column and optimistic locking if that matters.
- Whoever has both the database and the key reads everything. Keep the key out
  of the database host and its backups.
- MFA seeds (`forge_auth::mfa`, [mfa](mfa.md)) keep their own single-key format
  under `FORGE_AUTH_MFA_KEY`, built on the same `ring` AES-256-GCM.

## Differences from Forge-Go

- `Value[T]` (a `database/sql` scanner and valuer) is `Attribute::seal`/`open`
  with `Json<T>` here: Forge-owned functions that produce and consume the
  stored `String`, so no SQLx type appears in the API.
- The rewriters read UUID keys natively (`BINARY(16)`/`BLOB` on MySQL and
  SQLite, as Forge-Rust generates them); Forge-Go reads keys as text.
- `reencrypt` takes a `forge_core::Cancellation` and returns the report inside
  the error (`ReencryptError::report`) instead of a context and a report beside
  the error.
