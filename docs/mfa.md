# Two-factor authentication (TOTP)

Applications created with `forge new` (PostgreSQL, with views) can require a
code from an authenticator app at every sign-in. The design is
[ADR 0040](adr/0040-totp-multi-factor-authentication.md), a port of
Forge-Go's MFA; the threats are in the
[two-factor authentication review](threat-model.md#two-factor-authentication-review).

## Turn it on

```bash
# 32 random bytes as canonical unpadded base64url; keep it in a secret manager
export FORGE_AUTH_MFA_KEY=$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')
forge dev                     # or: cargo run
# sign in, open http://127.0.0.1:3000/mfa, enter your password,
# add the key to your authenticator app and confirm with its 6-digit code
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `FORGE_AUTH_MFA_KEY` | unset | AES-256-GCM key of the TOTP seeds, canonical unpadded base64url of 32 bytes (Forge-Go's format). Unset or empty: MFA is not configured. Invalid: startup stops. Redacted everywhere. |

With Docker Compose, export the key before `docker compose up`; the generated
`compose.yaml` passes `FORGE_AUTH_MFA_KEY` through.

## Pages

| Method | Path | Page |
| --- | --- | --- |
| `GET`, `POST` | `/session/mfa` | The code step of a sign-in: `303` signed in, `422` wrong code, `429` throttled (`Retry-After`) |
| `GET` | `/mfa` | Settings of the signed-in account: on or off, backup codes left |
| `POST` | `/mfa` | Turn on: the password again, then the key (`otpauth://` link and Base32 text) |
| `POST` | `/mfa/confirm` | Confirm with the password and a code of the new key: shows 10 backup codes once and signs out everywhere |
| `POST` | `/mfa/rotation`, `/mfa/rotation/confirm` | Replace the authenticator: password and a current code, then the password, a current code and a code of the new key |
| `POST` | `/mfa/backup_codes` | New backup codes (password and a current code); the old ones stop working |
| `DELETE` | `/mfa` | Turn off (password and a current code); signs out everywhere |

The pages are in `src/adapters/http/authentication.rs`, `templates/mfa/` and
`templates/sessions/mfa.html`; their text is under `sessions.mfa.*`,
`sessions.expired`, `sessions.invalid_code`, `throttled.code.*` (Forge-Go's
keys) and `mfa.*` in `config/locales/authentication.LOCALE.json` (`en`, `es`,
`pt-BR`). Change them freely; keep the security steps each handler documents.

Confirming a new key asks for the password again (and, for a rotation, a
current code), so a stolen session can neither read the pending key nor
confirm it. A TOTP code works once, so starting a rotation uses up the
current code of the authenticator app: to confirm it, wait for the next code
or use a backup code (the pages say so).

Every change counts one attempt on the second-factor throttle (five per
account in five minutes) before anything is checked, and is audited, refusals
as the action with outcome `denied`. `POST /mfa` and `POST /mfa/confirm` need
no code, so for an account that already has a factor they answer the same
way whatever the password (`303` back to `/mfa`, "changed meanwhile"): the
account is refused before the password is checked, so these routes cannot
tell a right password from a wrong one.

New backup codes (`POST /mfa/backup_codes`) replace the old ones at once, but,
as in Forge-Go, they neither sign out other sessions nor void a pending
sign-in challenge: neither depends on the backup codes, and a challenge can
no longer be completed with an old code because it is gone. If you think a
session or the password is compromised, reset the password or replace the
authenticator: both sign out everywhere.

## How sign-in works

1. The password form (`POST /session`) verifies the password. An account with
   a factor gets no session: a five-minute challenge cookie
   (`__Host-forge_mfa`) and a fresh CSRF token, then `303` to `/session/mfa`.
2. The code step accepts a code of the current period or one period before or
   after, or an unused backup code. It counts five attempts per account and 60
   per network address in five minutes before checking anything.
3. The challenge and the code are consumed in one transaction: a code (its
   period) and a backup code work once, also under concurrent requests. Then
   the session and a new CSRF token are issued as for a password sign-in.

A factor turned on while a password sign-in of the same account is storing
its session signs out every session before that one exists. The sign-in
therefore checks for a factor again once its session is stored (on
PostgreSQL, the insert waits for the account row the change locks), ends
that session and asks for the code instead.

A challenge stops working when it expires, when the account signs in again,
changes its password, is disabled, loses its membership of the tenant, or
changes its factor, and in any other browser session than the one it was
issued to. Without `FORGE_AUTH_MFA_KEY`, an account with a factor cannot sign
in at all (fail closed).

## Storage

Migration `00000000000000000004_forge_mfa` creates:

| Table | Holds |
| --- | --- |
| `forge_totp_enrollments` | the pending seed of an enrollment or rotation (ten minutes) |
| `forge_totp_factors` | the active seed and the last accepted period (`-1` before the first) |
| `forge_mfa_backup_codes` | `SHA-256("mfa-backup\0" \|\| code)` and when each was used |
| `forge_mfa_challenges` | `SHA-256(token)`, the account, tenant, credential version, factor digest, browser binding and expiries |

Seeds are AES-256-GCM ciphertext (`nonce || ciphertext || tag`) beside their
SHA-256 digest, compatible with Forge-Go: the same key opens seeds written by
either framework. The key never reaches the database. The runtime role can
read and change these tables but can only mark a backup code or a challenge
used, never rewrite its digest or binding.

## In code

```rust
use std::sync::Arc;
use forge::auth::{Aes256GcmSecretCipher, MemoryLoginThrottle, TotpSecret};
use forge::tenancy::{MfaChange, MfaService, mfa_throttle_config};

let cipher = Aes256GcmSecretCipher::from_base64url(&key)?;
let mfa = MfaService::new(
    Arc::new(PgMfaStore::new(pool)),
    Some(Arc::new(cipher)),
    Arc::new(MemoryLoginThrottle::new(mfa_throttle_config())?),
);
let enrollment = mfa.begin(principal, MfaChange::Enrollment, "shop", "ada@example.com", now).await?;
let backup_codes = mfa.confirm(principal, MfaChange::Enrollment, "123456", now).await?;
```

`forge::tenancy::testing::MemoryMfaStore` (feature `testing`) is the store's
contract in memory, for tests. `forge::auth::TotpSecret::from_base32(key)?.code_at(now)`
computes the code an authenticator app shows, for end-to-end tests.

To rotate `FORGE_AUTH_MFA_KEY`, implement `forge::auth::SecretCipher` with a
key ring (decrypt with old keys, encrypt with the new one) and re-encrypt
seeds in a job; the built-in cipher, like Forge-Go's, has one key.

## Not included

- A QR code image: Forge-Go renders none either; the page shows the
  `otpauth://` link (which phones open in their authenticator app) and the
  Base32 key.
- A JSON (bearer) completion route: Forge-Rust has no JSON login API.
- Pruning expired enrollments and consumed challenges (Forge-Go's `PruneMFA`).
