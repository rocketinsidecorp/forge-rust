# Credentials

Encrypted, per-environment secrets that are safe to commit: Forge's equivalent
of Rails' `credentials:edit`, with the same commands, files and format as
Forge-Go. Design: [ADR 0041](adr/0041-encrypted-credentials-and-attribute-encryption.md);
threats: [threat model](threat-model.md#credentials-and-attribute-encryption-review).
For encrypting database columns see [encryption](encryption.md).

```text
config/credentials/
  development.json.enc   committed, AES-256-GCM
  development.key        NOT committed, mode 0600, git- and docker-ignored
  production.json.enc    committed
  production.key         kept on the machine that edits production, never deployed
```

`forge new` creates the development pair. For another environment:

```sh
forge credentials init --environment production
```

`init` writes a random 256-bit key (operating-system CSPRNG) to
`config/credentials/production.key` with mode 0600 in a 0700 directory, an
encrypted skeleton holding a data-encryption key ring, and adds
`/config/credentials/*.key` to `.gitignore` and `config/credentials/*.key` to
`.dockerignore` (creating either file when missing). It never overwrites a key
or an existing file, and it never prints the key.

## Daily workflow

```sh
forge credentials edit                                     # development (or $FORGE_ENV)
forge credentials edit --environment production            # opens $VISUAL, else $EDITOR
forge credentials show --environment production            # structure only, values redacted
forge credentials show --environment production --reveal   # terminal only
git add config/credentials/production.json.enc && git commit
```

`-e` is short for `--environment`; without it the commands use `$FORGE_ENV`,
else `development`. Environment names are 1-32 lowercase letters, digits, `-`
or `_`, starting with a letter.

The document is one JSON object:

```json
{
  "stripe": { "api_key": "sk_live_..." },
  "database": { "url": "postgres://app:...@db.internal:5432/app?sslmode=verify-full" },
  "encryption": { "active": "k1", "keys": { "k1": "<base64url 32 bytes>" } }
}
```

`edit` behaves as follows:

- the plaintext exists only in a file of mode 0600 in a fresh 0700 directory
  (memory-backed `/dev/shm` on Linux), which is overwritten with zeros and
  removed on every exit path: saving, an editor error, invalid JSON, a failed
  write, `SIGTERM`, `SIGHUP`. `SIGKILL` or a power loss can leave it behind; on
  Linux it is in memory, elsewhere look for `forge-credentials-*` in your
  temporary directory;
- the editor command is split on spaces and run without a shell
  (`EDITOR="code --wait"` works, quoting and pipelines do not). The editor does
  not inherit any `FORGE_*` variable, so `FORGE_MASTER_KEY` stays out of its
  plugins and subprocesses. Vim-family editors are recognized by name and by
  what the name resolves to (`vi`, `editor`, `vim.basic`, `vim.nox`, `gvim`,
  `nvim`, `view`, symlinks to them) and started without swap, backup, undo and
  viminfo files. Other editors (a busybox `vi`, nano, Emacs, GUI editors) are
  started unchanged and may keep their own copies; check their backup settings;
- `SIGINT`, `SIGTERM` and `SIGHUP` are caught for the whole command and acted
  on at checkpoints, so the cleanup also covers validation and encryption. A
  Ctrl-C typed inside the editor belongs to the editor;
- invalid JSON, a duplicate key (at any depth) or a document that is not an
  object is rejected, your edits are discarded and the file is untouched; the
  error gives a line and column, never the content;
- if the encrypted file changed while the editor was open (a merge, another
  person), nothing is overwritten;
- unchanged content does not rewrite the file, so there is no spurious diff;
- the new file replaces the old one atomically (temporary file, `fsync`,
  `rename`).

`show --reveal` refuses to run when standard output is not a terminal, so values
do not end up in pipes, CI logs or files.

## Reading credentials in the application

Call it in the composition root (`src/bootstrap`) and pass what each component
needs; domain and application code never see the store.

```rust
use forge::credentials::Options;

let credentials = forge::credentials::open(
    &environment, // "production", "development", ...
    Options::default().with_required(["stripe.api_key", "encryption"]),
)?; // missing file, missing key, wrong key, missing credential: fail at startup
let stripe_key = credentials.secret(&["stripe", "api_key"])?; // SecretString, prints [REDACTED]
let client = stripe::Client::new(stripe_key.expose());

#[derive(serde::Deserialize)]
struct Database { url: String }
let database: Database = credentials.decode(&["database"])?;

let ring = credentials.key_ring(&["encryption"])?; // for forge::encryption::Cipher
```

`forge::credentials::load(env)` is `open` with default options. Rules:

| Situation | development, test | any other environment (`production`, `staging`, ...) |
| --------- | ----------------- | ---------------------------------------------------- |
| no file | empty `Store`, `exists()` is false | `CredentialsError::NotFound` |
| file, no key | `CredentialsError::KeyMissing` | `CredentialsError::KeyMissing` |
| wrong key, modified file or another environment's file | `Decrypt` / `NoMatchingKey` | same |
| `with_required` path missing or empty | ignored | `MissingRequired`, naming every missing path |

A `Store` prints as `credentials.Store(production, [REDACTED])` through
`Debug`, `Display` and serde. `string`, `secret` and `decode` errors name the
path, never the value. The decrypted document's strings are overwritten when
the last clone of the store is dropped. `forge::config` does not read
credentials.

To ship the encrypted file inside the binary instead of copying it into the
image:

```rust
static PRODUCTION: &[u8] = include_bytes!("../../config/credentials/production.json.enc");

let credentials = forge::credentials::open(
    "production",
    forge::credentials::Options::default().with_embedded(PRODUCTION),
)?;
```

The key must then come from `FORGE_MASTER_KEY`; `*.key` files are never
embedded. Otherwise copy `config/credentials/production.json.enc` into the
image (the generated Dockerfile copies only the binary) at the same path
relative to the working directory.

## Deploying

The server needs the key and nothing else of the credentials system:

```sh
cat config/credentials/production.key          # copy into the secret store
docker run --rm -p 3000:3000 \
  -e FORGE_ENV=production \
  -e FORGE_MASTER_KEY="$(cat config/credentials/production.key)" \
  shop
```

`FORGE_MASTER_KEY` is canonical unpadded base64url of 32 bytes, optionally
several separated by commas or newlines (the first one is active). It wins
over a key file. A key file must be a regular file (not a symbolic link) of
mode 0600 or stricter; one that group or others can access is refused.

In CI, store the key as a secret and expose it only to the job that needs it
(`FORGE_MASTER_KEY: ${{ secrets.FORGE_MASTER_KEY_PRODUCTION }}`). Tests and
builds do not need production credentials.

## Rotating the key

Rotate when someone who had the key leaves, when a key may have leaked, or on
a schedule.

```sh
forge credentials rotate-key --environment production --keep-old --print-key
```

It decrypts the file, generates a new key, re-encrypts, and writes the new key
list to `production.key` (and prints it with `--print-key`, for a secret
manager). With `FORGE_MASTER_KEY` as the source there is no key file, so
`--print-key` or `--write-key-file` is required, and nothing is re-encrypted
before the new key has a destination. The key file first gains the new key
beside the old ones, then the encrypted file changes, then the old keys are
dropped (unless `--keep-old`), so the pair on disk is readable at every
instant. A safe sequence without downtime:

1. `rotate-key --keep-old --print-key`; set `FORGE_MASTER_KEY` in the secret
   store to the printed `new,old` list. Deployments with the old file keep
   working (old key present), deployments with the new file work (new key
   present).
2. Commit and deploy the re-encrypted `production.json.enc`.
3. When no deployment or branch uses the old file, drop the old key: set
   `FORGE_MASTER_KEY` to the new key alone and delete the second line of
   `production.key`.

Without `--keep-old` the old key is discarded at once, which is right after a
leak once the new file is deployed. Rotating the master key does not change
the values inside; after a leak also rotate the secrets themselves (`edit`).
The data-encryption keys inside the document (`encryption.keys`) rotate
separately: [encryption](encryption.md#rotating-keys).

## Losing a key

If every copy of `production.key` and every `FORGE_MASTER_KEY` is gone, the
file cannot be decrypted. There is no recovery path by design. Keep the key in
a password manager or secret store. To start over: delete
`production.json.enc`, run `init` again and re-enter the secrets. Anything
encrypted under data-encryption keys that lived only in that file is lost with
it.

## Compatibility with Forge-Go

The file format, key text, key ids, environment variable, file names and
permissions are Forge-Go's: a file written by `forge credentials` of either
framework opens in the other. `crates/forge-credentials/tests/fixtures` holds a
file and attribute ciphertexts written by Forge-Go's own packages, which the
tests open; the reverse direction is checked by running

```sh
FORGE_INTEROP_DIR=/tmp/interop cargo test -p forge-credentials --test go_interop -- --ignored
```

and opening `/tmp/interop/rust-production.json.enc` and
`/tmp/interop/rust-values.json` with Forge-Go's `credentials.Unseal` and
`encryption.Cipher`. Differences: Forge-Rust refuses a key or credentials file
that is a symbolic link (Forge-Go allows links that stay inside the
directory), and parses the envelope strictly (exact field names, no duplicate
fields).

## Not covered

- Secrets are decrypted into process memory at startup. Forge-Rust zeroizes
  key material and the decrypted document on drop, but copies made by the
  application (`String`s from `string`, exposed `SecretString`s, `ring`'s
  internal key schedules) are not wiped; core dumps and swap can contain them.
- Whoever can run the application with the key can read every credential.
- An older but valid `production.json.enc` (for example from git history)
  decrypts under the same key. Rotate the key if you must invalidate it.
