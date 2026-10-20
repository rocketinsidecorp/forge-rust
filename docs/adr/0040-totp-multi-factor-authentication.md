# ADR 0040: TOTP multi-factor authentication

- **Status:** Accepted
- **Date:** 2026-10-20
- **Port of:** Forge-Go ADR 0014 (TOTP MFA) and ADR 0015 (MFA factor lifecycle)

The guide is [docs/mfa.md](../mfa.md); the threat analysis is the
[two-factor authentication review](../threat-model.md#two-factor-authentication-review).

## Context

Forge-Go ships TOTP multi-factor authentication: encrypted seeds, one-time
backup codes, a fail-closed password-then-code sign-in and step-up-guarded
rotation and disable. Rails parity lists "TOTP MFA" as Done in Forge-Go and
Planned in Forge-Rust. The Rust port must keep the behaviour, the environment
variable (`FORGE_AUTH_MFA_KEY`), compatible storage and the security
properties, under Forge-Rust's rules: no custom cryptography, hexagonal rings,
secrets redacted everywhere, and sign-in pages generated into the application
(ADR 0026) rather than served by the framework.

## Decision

1. **Primitives in `forge_auth::mfa`, from audited crates.** RFC 6238 codes
   over HMAC-SHA-1 (`hmac`, `sha1`); 160-bit seeds, 6 digits, 30-second
   periods, the current period plus one before and after, exactly as
   Forge-Go. A code is accepted only for a period later than the last
   accepted one (replay protection). Seeds are sealed with AES-256-GCM
   (`ring`) as `nonce || ciphertext || tag` with no associated data, Forge-Go's
   format, beside a SHA-256 digest checked after decryption. Backup codes are
   120 random bits in Base32 (`XXXX-XXXX-...`) stored as
   `SHA-256("mfa-backup\0" || raw)`, challenge tokens 256 random bits stored as
   `SHA-256(raw)`, both as in Forge-Go. No TOTP crate is admitted (see the
   [dependency policy](../dependency-policy.md#two-factor-authentication-dependency-decision)).
2. **Key handling matches Forge-Go.** `FORGE_AUTH_MFA_KEY` is canonical
   unpadded base64url of exactly 32 bytes. Unset or empty, MFA is not
   configured; set but invalid, startup stops in every environment, and the
   value is never echoed. There is one key: like Forge-Go's built-in cipher,
   rotating it needs a custom `SecretCipher` (a key ring or a KMS); the port
   is public for that.
3. **Rules in `forge_tenancy::MfaService`, persistence behind `MfaStore`.**
   The service owns enrollment (a pending seed for ten minutes, installed only
   by a live code of it), rotation (the old factor stays active until the new
   one is confirmed), disable, backup-code regeneration, step-up code checks
   and sign-in challenges. Challenges live five minutes and are bound to the
   principal, the tenant, the password credential version the password step
   verified (`verify_password` returns it, closing the reset race Forge-Go
   closes with `session_version`), the factor's seed digest, the session
   expiry and the browser session. Attempts are throttled per account and per
   source (five and 60 in five minutes) before any code is checked; the
   challenge and the code (TOTP period or backup code) are consumed in one
   transaction. Without a cipher, every operation but `status` fails closed.
4. **Generated pages and PostgreSQL tables.** `forge new` (and `forge generate
   authentication`) write migration `00000000000000000004_forge_mfa`
   (`forge_totp_enrollments`, `forge_totp_factors`, `forge_mfa_backup_codes`,
   `forge_mfa_challenges`, Forge-Go's tables keyed by principal instead of
   user/membership), `PgMfaStore`, the code step `/session/mfa` and the
   settings pages `/mfa`. Authentication exists only for PostgreSQL in
   Forge-Rust (ADR 0021), so no other backend gets these tables.
5. **Browser binding.** The password step issues a fresh CSRF token and binds
   the challenge (cookie `__Host-forge_mfa`, Forge-Go's name) to its digest;
   the code step accepts the challenge only from the browser session holding
   that token.
6. **Step-up.** Turning MFA on, and confirming its key, needs the password
   again; replacing the authenticator and confirming the new key, new backup
   codes and turning it off need the password and a current code (a used-up
   TOTP period or backup code). The pending key is shown only after step-up,
   so a session alone can neither read it nor confirm it. A password-only
   step-up clears the step-up counters only while the account has no factor,
   so it cannot reset the throttle that guards the codes. Forge-Go leaves this
   policy to an application `MFAChangeAuthorizer`; Forge-Rust generates the
   policy into the pages, where the application can change it.
7. **Audit.** Forge-Go's event names: `auth.mfa.enrollment_started`,
   `auth.mfa.enabled`, `auth.mfa.rotation_started`, `auth.mfa.rotated`,
   `auth.mfa.disabled`, `auth.mfa.challenge_succeeded`,
   `auth.mfa.challenge_failed` (`denied` for wrong, replayed, foreign or
   throttled attempts, `failed` for infrastructure), `auth.mfa.backup_code_used`;
   plus `auth.mfa.backup_codes_regenerated` for the regeneration Forge-Go
   does not have. A refused step-up records the target action as `denied`.

## Consequences

- Seeds stored by Forge-Go open in Forge-Rust and the reverse with the same
  key; backup-code and challenge digests are computed the same way.
- Turning MFA on, replacing it and turning it off revoke every session of the
  account and void its pending challenges, as in Forge-Go.
- Forge-Rust's generated application has enrollment, rotation, regeneration
  and disable pages; Forge-Go's scaffold registers only the code step and
  leaves those routes to the application (parity follow-up in
  [rails-parity](../rails-parity.md#parity-follow-ups)).
- Forge-Rust has no JSON login API, so there is no `POST /auth/mfa/complete`
  bearer route; the browser code step is the only completion path.
- Counters are per process (`MemoryLoginThrottle`); horizontally scaled
  deployments need a shared `LoginThrottle`, as for sign-in.
- Expired enrollments and consumed challenges stay in their tables until a
  cleanup job removes them (Forge-Go's `PruneMFA` is not ported yet).
