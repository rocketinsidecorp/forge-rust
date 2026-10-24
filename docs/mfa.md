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
as the action with outcome `denied`. An authenticator code given as the
current code is used up at once; a backup code is only checked then, and is
used up by the change itself, in the transaction that applies it. A change
that fails (a wrong code of the new key, a rotation that expired, an account
changed meanwhile) therefore keeps the backup code, and a backup code used
elsewhere in the meantime refuses the change with the step-up message. The
counters are cleared only once a change succeeds (as in Forge-Go): wrong
codes of a new key keep counting. A backup code a change used up is audited
as `auth.mfa.backup_code_used`. `POST /mfa` and `POST /mfa/confirm` need
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
   The counters, like the sign-in ones, live in the memory of each process
   (`MemoryLoginThrottle`): with several replicas the limits multiply by the
   number of replicas, a restart clears them, and under pressure counters
   below their limit can be evicted. A deployment that scales out needs a
   shared `forge::auth::LoginThrottle` implementation; Forge-Rust does not
   ship one yet (Forge-Go has a PostgreSQL one).
3. The challenge and the code are consumed in one transaction: a code (its
   period) and a backup code work once, also under concurrent requests. Then
   the session and a new CSRF token are issued as for a password sign-in.

A factor turned on while a password sign-in of the same account is storing
its session signs out every session before that one exists. The sign-in
therefore checks for a factor again once its session is stored (on
PostgreSQL, the insert waits for the account row the change locks), ends
that session and asks for the code instead.

A factor change or a password reset that commits after the code step
accepted a code but before its session is stored ends that session too: the
session is checked against the factor and the password once it is stored,
and the browser is sent back to sign in.

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

In memory, seeds, provisioning links, backup codes and challenge tokens are
wiped (`zeroize`) when dropped, and so are the buffers that hold a seed while
it is sealed, opened or parsed. This is best effort: text a page renders
(such as `TotpSecret::base32()`) is the caller's copy.

A pending seed in `forge_totp_enrollments` is an enrollment's while the
account has no factor and a rotation's while it has one; the store reads the
two together, so a seed of one kind is never confirmed or shown as the other.

Every transaction that changes these tables locks the account's
`forge_principals` row first, then its MFA rows (factor changes `FOR UPDATE`,
issuing and completing a sign-in challenge `FOR NO KEY UPDATE`), so a sign-in
and a factor change of the same account wait for each other instead of
deadlocking. Custom `MfaStore` implementations should keep that order.

## In code

```rust
use std::sync::Arc;
use forge::auth::{Aes256GcmSecretCipher, MemoryLoginThrottle, TotpSecret};
use forge::tenancy::{MfaChangeRequest, MfaService, mfa_throttle_config};

let cipher = Aes256GcmSecretCipher::from_base64url(&key)?;
let mfa = MfaService::new(
    Arc::new(PgMfaStore::new(pool)),
    Some(Arc::new(cipher)),
    Arc::new(MemoryLoginThrottle::new(mfa_throttle_config())?),
);
let enrollment = mfa.begin(principal, MfaChangeRequest::Enrollment, "shop", "ada@example.com", now).await?;
let backup_codes = mfa.confirm(principal, MfaChangeRequest::Enrollment, "123456", now).await?;

// A change of an existing factor needs the proof of a current code:
let proof = mfa.verify_step_up(principal, current_code, now).await?;
mfa.disable(principal, &proof, now).await?; // a backup code is used up here, with the change
```

`verify_step_up` uses a TOTP code up at once but only checks a backup code,
and returns a `StepUpProof` bound to that account. The changes of an
existing factor require one: `MfaChangeRequest::Rotation(&proof)` for
`begin` and `confirm`, `&proof` for `disable` and `regenerate_backup_codes`.
They use the backup code up in the same transaction as the change, and
refuse with `MfaError::StepUpInvalid` (changing nothing) a proof of another
account or a code used meanwhile. `verify_second_factor` checks and uses a
code up at once, for a step-up that does not change the factor.

After `complete` accepts a code, store the session and then check
`MfaService::sign_in_is_current` and the credential version
(`MfaSignIn::credential_version`), revoking the session when either changed:
the generated `issue_second_factor_session` does this.

`forge::tenancy::testing::MemoryMfaStore` (feature `testing`) is the store's
contract in memory, for tests. `forge::auth::TotpSecret::from_base32(key)?.code_at(now)`
computes the code an authenticator app shows, for end-to-end tests.

To rotate `FORGE_AUTH_MFA_KEY`, implement `forge::auth::SecretCipher` with a
key ring (decrypt with old keys, encrypt with the new one) and re-encrypt
seeds in a job; the built-in cipher, like Forge-Go's, has one key.

## Upgrading an existing application

Release note (forge-rust#36, #10): the MFA contracts changed, and
applications generated before must port four generated files.

- `forge::tenancy::MfaService`: `begin` and `confirm` take an
  `MfaChangeRequest` (`Enrollment`, or `Rotation(&StepUpProof)`) instead of
  `MfaChange`; `disable` and `regenerate_backup_codes` take a `&StepUpProof`;
  `pending` takes the `MfaChange`; `verify_step_up` is new (use it instead of
  `verify_second_factor` before a change), and so are
  `MfaError::StepUpInvalid`, `sign_in_is_current`,
  `MfaSignIn::{credential_version, factor_digest}` and
  `MfaChallengeRecord::credential_version`.
- `forge::tenancy::MfaStore`: `begin`, `activate`, `disable` and
  `replace_backup_codes` take `step_up: Option<&MfaBackupCodeDigest>` (use
  the code up in the same transaction; `false` when it is not unused);
  `pending` takes the `MfaChange` (a rotation's seed exactly when the account
  has a factor, read in one statement); `has_backup_code` is new; `challenge`
  fills `credential_version`. Every MFA transaction must lock the account row
  first (see Storage).
- `forge::auth::PasswordResetNotifier::can_deliver` is new (default `true`).
- Generated files to port: `src/infrastructure/accounts/mod.rs` (the store,
  the lock order, the reset redemption that locks the account row first),
  `src/infrastructure/auth/mod.rs` (session rotation locks the account row
  first), `src/adapters/http/authentication.rs` (the step-up proofs, the
  throttle cleared after a successful change, `issue_second_factor_session`,
  no reset queued without a mailer) and `src/bootstrap/mod.rs` (the reset
  throttle). Generate a fresh application with the same options and compare:

```bash
(cd /tmp && forge new shop-new -d postgresql --skip-docker --skip-ci)
for file in src/infrastructure/accounts/mod.rs src/infrastructure/auth/mod.rs \
            src/adapters/http/authentication.rs src/bootstrap/mod.rs; do
  diff -u "$file" "/tmp/shop-new/$file"
done
```

  No migration changes.
- Reset links: requests and submissions now use Forge-Go's recovery limits
  (`forge::auth::password_reset_throttle_config`), per hour: 3 per login (or
  link) and source, 10 per login (or link), 30 per source. Three rejected new
  passwords on one link from one address lock that link for an hour (a new
  link can be requested); without a mailer a reset request queues nothing.

## Not included

- A QR code image: Forge-Go renders none either; the page shows the
  `otpauth://` link (which phones open in their authenticator app) and the
  Base32 key.
- A JSON (bearer) completion route: Forge-Rust has no JSON login API.
- Pruning expired enrollments and consumed challenges (Forge-Go's `PruneMFA`).
