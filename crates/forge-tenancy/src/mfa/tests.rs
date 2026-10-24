use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;

use forge_auth::{Aes256GcmSecretCipher, MemoryLoginThrottle};

use super::testing::MemoryMfaStore;
use super::*;

const ISSUER: &str = "Acme";
const ACCOUNT: &str = "ada@example.com";
const START: u64 = 1_800_000_000;

fn at(seconds: u64) -> UnixTimestamp {
    UnixTimestamp::from_secs(seconds)
}

fn source(last: u8) -> Option<IpAddr> {
    Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)))
}

fn binding(byte: u8) -> MfaChallengeBinding {
    MfaChallengeBinding::from_bytes([byte; 32])
}

fn version(value: u64) -> PasswordCredentialVersion {
    PasswordCredentialVersion::new(value).expect("positive")
}

struct Harness {
    store: Arc<MemoryMfaStore>,
    service: MfaService,
    revoked: Arc<Mutex<Vec<PrincipalId>>>,
}

fn harness_with(key: [u8; 32]) -> Harness {
    let revoked = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&revoked);
    let store = Arc::new(MemoryMfaStore::new().with_session_revoker(Arc::new(
        move |principal, _| sink.lock().expect("lock").push(principal),
    )));
    let cipher: Arc<dyn SecretCipher> = Arc::new(Aes256GcmSecretCipher::new(&key).expect("key"));
    let throttle = Arc::new(MemoryLoginThrottle::new(mfa_throttle_config()).expect("config"));
    let service = MfaService::new(store.clone(), Some(cipher), throttle);
    Harness {
        store,
        service,
        revoked,
    }
}

/// A step-up proof of `principal` as `verify_step_up` returns for a TOTP
/// code, for tests that are not about the step-up.
fn trusted(principal: PrincipalId) -> StepUpProof {
    StepUpProof {
        principal,
        backup_code: None,
    }
}

fn harness() -> Harness {
    harness_with([7; 32])
}

/// Enrolls `principal` at `START` and returns its seed and backup codes.
async fn enrolled(harness: &Harness, principal: PrincipalId) -> (TotpSecret, Vec<MfaBackupCode>) {
    let enrollment = harness
        .service
        .begin(
            principal,
            MfaChangeRequest::Enrollment,
            ISSUER,
            ACCOUNT,
            at(START),
        )
        .await
        .expect("begin");
    let code = enrollment.secret.code_at(at(START));
    let codes = harness
        .service
        .confirm(principal, MfaChangeRequest::Enrollment, &code, at(START))
        .await
        .expect("confirm");
    (enrollment.secret, codes)
}

async fn challenge(
    harness: &Harness,
    principal: PrincipalId,
    tenant: TenantId,
    now: u64,
) -> MfaChallengeToken {
    harness
        .service
        .issue_challenge(
            principal,
            tenant,
            version(1),
            binding(1),
            at(now + 3600),
            at(now),
        )
        .await
        .expect("challenge")
        .token
}

#[tokio::test]
async fn enrollment_installs_the_factor_with_backup_codes_and_revokes_sessions() {
    let harness = harness();
    let ada = PrincipalId::new();
    assert_eq!(harness.service.status(ada).await, Ok(None));
    let enrollment = harness
        .service
        .begin(
            ada,
            MfaChangeRequest::Enrollment,
            ISSUER,
            ACCOUNT,
            at(START),
        )
        .await
        .expect("begin");
    assert_eq!(
        enrollment.expires_at,
        at(START + MFA_ENROLLMENT_LIFETIME_SECS)
    );
    assert!(
        enrollment
            .provisioning_uri
            .expose()
            .contains(&enrollment.secret.base32())
    );
    // Not active until confirmed; a wrong code changes nothing.
    assert_eq!(harness.service.status(ada).await, Ok(None));
    assert_eq!(
        harness
            .service
            .confirm(ada, MfaChangeRequest::Enrollment, "000000", at(START))
            .await
            .err()
            .filter(|_| enrollment.secret.code_at(at(START)) != "000000"),
        Some(MfaError::InvalidCode)
    );
    // The pending seed can be shown again after a wrong code.
    let again = harness
        .service
        .pending(ada, MfaChange::Enrollment, ISSUER, ACCOUNT, at(START))
        .await
        .expect("pending")
        .expect("still pending");
    assert_eq!(again.secret.digest(), enrollment.secret.digest());

    let code = enrollment.secret.code_at(at(START));
    let codes = harness
        .service
        .confirm(ada, MfaChangeRequest::Enrollment, &code, at(START))
        .await
        .expect("confirm");
    assert_eq!(codes.len(), MFA_BACKUP_CODE_COUNT);
    assert_eq!(
        harness.service.status(ada).await,
        Ok(Some(MfaStatus {
            remaining_backup_codes: 10
        }))
    );
    assert_eq!(*harness.revoked.lock().expect("lock"), vec![ada]);
    // A second enrollment is refused, and so is confirming the used one.
    assert_eq!(
        harness
            .service
            .begin(
                ada,
                MfaChangeRequest::Enrollment,
                ISSUER,
                ACCOUNT,
                at(START)
            )
            .await
            .err(),
        Some(MfaError::AlreadyEnabled)
    );
    assert_eq!(
        harness
            .service
            .confirm(ada, MfaChangeRequest::Enrollment, &code, at(START))
            .await
            .err(),
        Some(MfaError::AlreadyEnabled),
        "an account with a factor has no enrollment to confirm"
    );
}

#[tokio::test]
async fn an_expired_enrollment_cannot_be_confirmed() {
    let harness = harness();
    let ada = PrincipalId::new();
    let enrollment = harness
        .service
        .begin(
            ada,
            MfaChangeRequest::Enrollment,
            ISSUER,
            ACCOUNT,
            at(START),
        )
        .await
        .expect("begin");
    let late = START + MFA_ENROLLMENT_LIFETIME_SECS;
    assert_eq!(
        harness
            .service
            .confirm(
                ada,
                MfaChangeRequest::Enrollment,
                &enrollment.secret.code_at(at(late)),
                at(late)
            )
            .await
            .err(),
        Some(MfaError::EnrollmentInvalid)
    );
    assert_eq!(harness.service.status(ada).await, Ok(None));
}

#[tokio::test]
async fn a_challenge_completes_once_and_its_code_is_never_replayed() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    let (secret, _) = enrolled(&harness, ada).await;
    let now = START + 60;

    // The confirmation's period is recorded: its code cannot sign in.
    let token = challenge(&harness, ada, tenant, now).await;
    let confirmation_code = secret.code_at(at(START));
    let replay = harness
        .service
        .complete(
            token.expose(),
            &binding(1),
            &confirmation_code,
            source(1),
            at(START + 29),
        )
        .await
        .err()
        .map(|rejection| rejection.error);
    assert_eq!(replay, Some(MfaError::InvalidCode));

    let code = secret.code_at(at(now));
    let signed_in = harness
        .service
        .complete(token.expose(), &binding(1), &code, source(1), at(now))
        .await
        .expect("completes");
    assert_eq!(signed_in.principal_id, ada);
    assert_eq!(signed_in.tenant_id, tenant);
    assert_eq!(signed_in.factor, SecondFactor::Totp);
    assert_eq!(signed_in.session_expires_at, at(now + 3600));

    // The challenge is used up.
    let used = harness
        .service
        .complete(token.expose(), &binding(1), &code, source(1), at(now))
        .await
        .err();
    assert_eq!(used.map(|r| r.error), Some(MfaError::ChallengeExpired));
    // The same code on a fresh challenge is a replay.
    let fresh = challenge(&harness, ada, tenant, now).await;
    let replayed = harness
        .service
        .complete(fresh.expose(), &binding(1), &code, source(1), at(now))
        .await
        .err();
    assert_eq!(
        replayed,
        Some(MfaRejection {
            error: MfaError::InvalidCode,
            principal_id: Some(ada),
            tenant_id: Some(tenant),
        })
    );
    // The next period's code works.
    harness
        .service
        .complete(
            fresh.expose(),
            &binding(1),
            &secret.code_at(at(now + 30)),
            source(1),
            at(now + 30),
        )
        .await
        .expect("next period");
}

#[tokio::test]
async fn wrong_codes_expired_challenges_and_foreign_browsers_are_refused() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    let (secret, _) = enrolled(&harness, ada).await;
    let now = START + 120;
    let token = challenge(&harness, ada, tenant, now).await;
    let code = secret.code_at(at(now));
    let wrong = if code == "123456" { "654321" } else { "123456" };
    let complete = |token: &str, binding: MfaChallengeBinding, code: &str, when: u64| {
        let service = harness.service.clone();
        let (token, code) = (token.to_owned(), code.to_owned());
        async move {
            service
                .complete(&token, &binding, &code, source(1), at(when))
                .await
                .err()
                .map(|rejection| rejection.error)
        }
    };

    assert_eq!(
        complete(token.expose(), binding(1), wrong, now).await,
        Some(MfaError::InvalidCode)
    );
    assert_eq!(
        complete(token.expose(), binding(1), "12345", now).await,
        Some(MfaError::InvalidCode)
    );
    // Another browser session holding the challenge cookie.
    assert_eq!(
        complete(token.expose(), binding(2), &code, now).await,
        Some(MfaError::InvalidCode)
    );
    // A code two periods away.
    assert_eq!(
        complete(
            token.expose(),
            binding(1),
            &secret.code_at(at(now + 60)),
            now
        )
        .await,
        Some(MfaError::InvalidCode)
    );
    // Malformed and unknown tokens.
    assert_eq!(
        complete("not-a-token", binding(1), &code, now).await,
        Some(MfaError::ChallengeExpired)
    );
    let unknown = MfaChallengeToken::generate().expect("random");
    assert_eq!(
        complete(unknown.expose(), binding(1), &code, now).await,
        Some(MfaError::ChallengeExpired)
    );
    // Expired: at and after its expiry.
    let expiry = now + MFA_CHALLENGE_LIFETIME_SECS;
    assert_eq!(
        complete(
            token.expose(),
            binding(1),
            &secret.code_at(at(expiry)),
            expiry
        )
        .await,
        Some(MfaError::ChallengeExpired)
    );
    // Still valid before it.
    assert_eq!(complete(token.expose(), binding(1), &code, now).await, None);
}

#[tokio::test]
async fn a_new_challenge_voids_the_old_one_and_password_changes_void_both() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    let (secret, _) = enrolled(&harness, ada).await;
    let now = START + 300;
    let first = challenge(&harness, ada, tenant, now).await;
    let second = challenge(&harness, ada, tenant, now).await;
    let code = secret.code_at(at(now));
    let first_result = harness
        .service
        .complete(first.expose(), &binding(1), &code, source(1), at(now))
        .await
        .err()
        .map(|rejection| rejection.error);
    assert_eq!(first_result, Some(MfaError::ChallengeExpired));

    harness.store.set_credential_version(ada, version(2));
    let stale = harness
        .service
        .complete(second.expose(), &binding(1), &code, source(1), at(now))
        .await
        .err()
        .map(|rejection| rejection.error);
    assert_eq!(stale, Some(MfaError::ChallengeExpired));
    assert_eq!(
        harness
            .service
            .issue_challenge(ada, tenant, version(1), binding(1), at(now + 60), at(now))
            .await
            .err(),
        Some(MfaError::NotEnabled)
    );
}

#[tokio::test]
async fn attempts_are_throttled_per_account_and_per_source_before_the_code_is_checked() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    let (secret, _) = enrolled(&harness, ada).await;
    let now = START + 600;
    let token = challenge(&harness, ada, tenant, now).await;
    let code = secret.code_at(at(now));
    let wrong = if code == "000001" { "000002" } else { "000001" };
    for _ in 0..5 {
        let error = harness
            .service
            .complete(token.expose(), &binding(1), wrong, source(1), at(now))
            .await
            .err()
            .map(|rejection| rejection.error);
        assert_eq!(error, Some(MfaError::InvalidCode));
    }
    // The right code is refused now, from any source: the account's budget is spent.
    for last in [1, 2] {
        let throttled = harness
            .service
            .complete(token.expose(), &binding(1), &code, source(last), at(now))
            .await
            .err()
            .map(|rejection| rejection.error);
        assert!(
            matches!(throttled, Some(MfaError::Throttled { retry_after }) if retry_after > Duration::ZERO),
            "{throttled:?}"
        );
    }

    // One source guessing at many accounts stops at 60.
    let harness = harness_with([9; 32]);
    let mut throttled = 0;
    for _ in 0..13 {
        let (principal, tenant) = (PrincipalId::new(), TenantId::new());
        enrolled(&harness, principal).await;
        let token = challenge(&harness, principal, tenant, now).await;
        for _ in 0..5 {
            let error = harness
                .service
                .complete(token.expose(), &binding(1), "999999x", source(9), at(now))
                .await
                .err()
                .map(|rejection| rejection.error);
            if matches!(error, Some(MfaError::Throttled { .. })) {
                throttled += 1;
            }
        }
    }
    assert_eq!(throttled, 13 * 5 - 60);
}

#[tokio::test]
async fn backup_codes_complete_a_challenge_once() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    let (_, codes) = enrolled(&harness, ada).await;
    let now = START + 900;
    let token = challenge(&harness, ada, tenant, now).await;
    let typed = codes[0].expose().to_lowercase();
    let signed_in = harness
        .service
        .complete(token.expose(), &binding(1), &typed, source(1), at(now))
        .await
        .expect("backup code");
    assert_eq!(signed_in.factor, SecondFactor::BackupCode);
    assert_eq!(
        harness.service.status(ada).await,
        Ok(Some(MfaStatus {
            remaining_backup_codes: 9
        }))
    );
    let again = challenge(&harness, ada, tenant, now).await;
    let reused = harness
        .service
        .complete(
            again.expose(),
            &binding(1),
            codes[0].expose(),
            source(1),
            at(now),
        )
        .await
        .err()
        .map(|rejection| rejection.error);
    assert_eq!(reused, Some(MfaError::InvalidCode));
    // An unknown well-formed code too.
    let (unknown, _) = MfaBackupCode::generate_set(1).expect("codes");
    let unknown_result = harness
        .service
        .complete(
            again.expose(),
            &binding(1),
            unknown[0].expose(),
            source(1),
            at(now),
        )
        .await
        .err()
        .map(|rejection| rejection.error);
    assert_eq!(unknown_result, Some(MfaError::InvalidCode));
    harness
        .service
        .complete(
            again.expose(),
            &binding(1),
            codes[1].expose(),
            source(1),
            at(now),
        )
        .await
        .expect("another code");
}

#[tokio::test]
async fn step_up_codes_are_single_use_and_regeneration_replaces_every_backup_code() {
    let harness = harness();
    let ada = PrincipalId::new();
    let (secret, codes) = enrolled(&harness, ada).await;
    let now = START + 1200;
    let code = secret.code_at(at(now));
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, &code, at(now))
            .await,
        Ok(SecondFactor::Totp)
    );
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, &code, at(now))
            .await,
        Err(MfaError::InvalidCode)
    );
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, codes[0].expose(), at(now))
            .await,
        Ok(SecondFactor::BackupCode)
    );
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, codes[0].expose(), at(now))
            .await,
        Err(MfaError::InvalidCode)
    );
    let fresh = harness
        .service
        .regenerate_backup_codes(ada, &trusted(ada), at(now))
        .await
        .expect("regenerated");
    assert_eq!(fresh.len(), MFA_BACKUP_CODE_COUNT);
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, codes[1].expose(), at(now))
            .await,
        Err(MfaError::InvalidCode)
    );
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, fresh[0].expose(), at(now))
            .await,
        Ok(SecondFactor::BackupCode)
    );
    let bob = PrincipalId::new();
    assert_eq!(
        harness
            .service
            .verify_second_factor(bob, &code, at(now))
            .await,
        Err(MfaError::NotEnabled)
    );
}

#[tokio::test]
async fn rotation_keeps_the_old_factor_until_confirmed_and_disable_removes_it() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    let (old, _) = enrolled(&harness, ada).await;
    let now = START + 1500;
    let stranger = PrincipalId::new();
    assert_eq!(
        harness
            .service
            .begin(
                stranger,
                MfaChangeRequest::Rotation(&trusted(stranger)),
                ISSUER,
                ACCOUNT,
                at(now)
            )
            .await
            .err(),
        Some(MfaError::NotEnabled)
    );
    let rotation = harness
        .service
        .begin(
            ada,
            MfaChangeRequest::Rotation(&trusted(ada)),
            ISSUER,
            ACCOUNT,
            at(now),
        )
        .await
        .expect("rotation");
    let pending_challenge = challenge(&harness, ada, tenant, now).await;
    // The old factor still works until the new one is confirmed.
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, &old.code_at(at(now)), at(now))
            .await,
        Ok(SecondFactor::Totp)
    );
    harness
        .service
        .confirm(
            ada,
            MfaChangeRequest::Rotation(&trusted(ada)),
            &rotation.secret.code_at(at(now + 30)),
            at(now + 30),
        )
        .await
        .expect("rotated");
    assert_eq!(harness.revoked.lock().expect("lock").len(), 2);
    // Outstanding challenges died with the old factor.
    let stale = harness
        .service
        .complete(
            pending_challenge.expose(),
            &binding(1),
            &rotation.secret.code_at(at(now + 60)),
            source(1),
            at(now + 60),
        )
        .await
        .err()
        .map(|rejection| rejection.error);
    assert_eq!(stale, Some(MfaError::ChallengeExpired));
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, &old.code_at(at(now + 60)), at(now + 60))
            .await,
        Err(MfaError::InvalidCode)
    );

    harness
        .service
        .disable(ada, &trusted(ada), at(now + 90))
        .await
        .expect("disabled");
    assert_eq!(harness.service.status(ada).await, Ok(None));
    assert_eq!(harness.revoked.lock().expect("lock").len(), 3);
    assert_eq!(
        harness
            .service
            .disable(ada, &trusted(ada), at(now + 90))
            .await,
        Err(MfaError::NotEnabled)
    );
}

/// A step-up backup code is checked by `verify_step_up` and used up only by
/// the change it authorizes, in the same store operation: a change that
/// fails keeps the code, and a code used meanwhile refuses the change.
#[tokio::test]
async fn a_failed_change_keeps_the_step_up_backup_code() {
    let harness = harness();
    let ada = PrincipalId::new();
    let (old, codes) = enrolled(&harness, ada).await;
    let now = START + 1500;
    let remaining = || async { harness.service.status(ada).await.ok().flatten() };
    let step_up = |code: String, at_: u64| {
        let service = harness.service.clone();
        async move { service.verify_step_up(ada, &code, at(at_)).await }
    };

    // Checking a backup code uses nothing up; a wrong one is refused.
    let proof = step_up(codes[0].expose().to_owned(), now)
        .await
        .expect("backup code");
    assert_eq!(proof.factor(), SecondFactor::BackupCode);
    assert!(step_up(codes[0].expose().to_owned(), now).await.is_ok());
    assert_eq!(
        step_up("AAAA-AAAA-AAAA-AAAA-AAAA-AAAA".to_owned(), now).await,
        Err(MfaError::InvalidCode)
    );
    assert_eq!(
        remaining().await.map(|s| s.remaining_backup_codes),
        Some(10)
    );

    // Starting a rotation uses the proof's code up with the pending seed.
    let rotation = harness
        .service
        .begin(
            ada,
            MfaChangeRequest::Rotation(&proof),
            ISSUER,
            ACCOUNT,
            at(now),
        )
        .await
        .expect("rotation");
    assert_eq!(remaining().await.map(|s| s.remaining_backup_codes), Some(9));
    assert_eq!(
        harness
            .service
            .begin(
                ada,
                MfaChangeRequest::Rotation(&proof),
                ISSUER,
                ACCOUNT,
                at(now)
            )
            .await
            .err(),
        Some(MfaError::StepUpInvalid),
        "a used code authorizes nothing else"
    );

    // A wrong code of the new key, and an expired rotation, keep the
    // step-up code.
    let proof = step_up(codes[1].expose().to_owned(), now)
        .await
        .expect("backup code");
    let wrong = if rotation.secret.code_at(at(now)) == "000000" {
        "111111"
    } else {
        "000000"
    };
    assert_eq!(
        harness
            .service
            .confirm(ada, MfaChangeRequest::Rotation(&proof), wrong, at(now))
            .await
            .err(),
        Some(MfaError::InvalidCode)
    );
    let expired = now + MFA_ENROLLMENT_LIFETIME_SECS;
    assert_eq!(
        harness
            .service
            .confirm(
                ada,
                MfaChangeRequest::Rotation(&proof),
                &rotation.secret.code_at(at(expired)),
                at(expired)
            )
            .await
            .err(),
        Some(MfaError::EnrollmentInvalid)
    );
    assert_eq!(remaining().await.map(|s| s.remaining_backup_codes), Some(9));
    assert!(
        harness
            .store
            .has_backup_code(
                ada,
                &MfaBackupCode::parse(codes[1].expose()).expect("digest")
            )
            .await
            .expect("store")
    );

    // A code used elsewhere after the check refuses every change, which
    // then changes nothing.
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, codes[1].expose(), at(now))
            .await,
        Ok(SecondFactor::BackupCode)
    );
    assert_eq!(
        harness
            .service
            .confirm(
                ada,
                MfaChangeRequest::Rotation(&proof),
                &rotation.secret.code_at(at(now + 30)),
                at(now + 30)
            )
            .await
            .err(),
        Some(MfaError::StepUpInvalid)
    );
    assert_eq!(
        harness.service.disable(ada, &proof, at(now + 30)).await,
        Err(MfaError::StepUpInvalid)
    );
    assert_eq!(
        harness
            .service
            .regenerate_backup_codes(ada, &proof, at(now + 30))
            .await
            .err(),
        Some(MfaError::StepUpInvalid)
    );
    assert_eq!(remaining().await.map(|s| s.remaining_backup_codes), Some(8));
    assert!(
        harness
            .service
            .pending(ada, MfaChange::Rotation, ISSUER, ACCOUNT, at(now + 30))
            .await
            .expect("pending")
            .is_some(),
        "the rotation is still pending"
    );
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, &old.code_at(at(now + 60)), at(now + 60))
            .await,
        Ok(SecondFactor::Totp),
        "the old factor is still the active one"
    );

    // A TOTP step-up records its period at once.
    let totp = old.code_at(at(now + 90));
    assert_eq!(
        step_up(totp.clone(), now + 90)
            .await
            .map(|proof| proof.factor()),
        Ok(SecondFactor::Totp)
    );
    assert_eq!(step_up(totp, now + 90).await, Err(MfaError::InvalidCode));

    // A valid proof's change goes through and uses its code up.
    let proof = step_up(codes[2].expose().to_owned(), now + 90)
        .await
        .expect("backup code");
    let fresh = harness
        .service
        .regenerate_backup_codes(ada, &proof, at(now + 90))
        .await
        .expect("regenerated");
    let proof = step_up(fresh[0].expose().to_owned(), now + 90)
        .await
        .expect("backup code");
    harness
        .service
        .disable(ada, &proof, at(now + 90))
        .await
        .expect("disabled");
    assert_eq!(harness.service.status(ada).await, Ok(None));
    assert_eq!(
        step_up(fresh[1].expose().to_owned(), now + 90).await,
        Err(MfaError::InvalidCode)
    );
}

/// A step-up proof is bound to the account it was given for: every change
/// refuses another account's proof, and nothing changes or is used up.
#[tokio::test]
async fn a_step_up_proof_of_another_account_is_refused() {
    let harness = harness();
    let (ada, bob) = (PrincipalId::new(), PrincipalId::new());
    let (_, ada_codes) = enrolled(&harness, ada).await;
    let (bob_secret, _) = enrolled(&harness, bob).await;
    let now = START + 1500;
    // Ada's backup code and Bob's TOTP code, each proven for its own account.
    let ada_backup = harness
        .service
        .verify_step_up(ada, ada_codes[0].expose(), at(now))
        .await
        .expect("ada's proof");
    let bob_totp = harness
        .service
        .verify_step_up(bob, &bob_secret.code_at(at(now)), at(now))
        .await
        .expect("bob's proof");
    assert_eq!(bob_totp.principal(), bob);
    for proof in [&ada_backup, &bob_totp] {
        let victim = if proof.principal() == ada { bob } else { ada };
        assert_eq!(
            harness
                .service
                .begin(
                    victim,
                    MfaChangeRequest::Rotation(proof),
                    ISSUER,
                    ACCOUNT,
                    at(now)
                )
                .await
                .err(),
            Some(MfaError::StepUpInvalid)
        );
        assert_eq!(
            harness
                .service
                .confirm(victim, MfaChangeRequest::Rotation(proof), "000000", at(now))
                .await
                .err(),
            Some(MfaError::StepUpInvalid)
        );
        assert_eq!(
            harness.service.disable(victim, proof, at(now)).await,
            Err(MfaError::StepUpInvalid)
        );
        assert_eq!(
            harness
                .service
                .regenerate_backup_codes(victim, proof, at(now))
                .await
                .err(),
            Some(MfaError::StepUpInvalid)
        );
    }
    for principal in [ada, bob] {
        assert_eq!(
            harness
                .service
                .status(principal)
                .await
                .map(|status| status.map(|status| status.remaining_backup_codes)),
            Ok(Some(10)),
            "nothing changed and no code was used up"
        );
        assert!(
            harness
                .store
                .pending(principal, MfaChange::Rotation, at(now))
                .await
                .expect("pending")
                .is_none()
        );
    }
    // Ada's own proof still works for Ada.
    harness
        .service
        .disable(ada, &ada_backup, at(now))
        .await
        .expect("disabled");
}

/// A completed sign-in names the factor it used; a factor change that
/// commits before the session is stored makes it stale.
#[tokio::test]
async fn a_sign_in_is_stale_once_the_factor_changes() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    enrolled(&harness, ada).await;
    for change in ["rotation", "disable"] {
        let now = START + if change == "rotation" { 600 } else { 1200 };
        let token = challenge(&harness, ada, tenant, now).await;
        let current = harness
            .store
            .active(ada)
            .await
            .expect("active")
            .expect("factor");
        let code = TotpSecret::open(
            &Aes256GcmSecretCipher::new(&[7; 32]).expect("key"),
            &current.sealed,
        )
        .expect("seed")
        .code_at(at(now));
        let signed_in = harness
            .service
            .complete(token.expose(), &binding(1), &code, source(1), at(now))
            .await
            .expect("completed");
        assert_eq!(signed_in.credential_version, version(1));
        assert_eq!(&signed_in.factor_digest, current.sealed.digest());
        assert_eq!(
            harness.service.sign_in_is_current(&signed_in).await,
            Ok(true)
        );
        if change == "rotation" {
            let rotation = harness
                .service
                .begin(
                    ada,
                    MfaChangeRequest::Rotation(&trusted(ada)),
                    ISSUER,
                    ACCOUNT,
                    at(now),
                )
                .await
                .expect("rotation");
            harness
                .service
                .confirm(
                    ada,
                    MfaChangeRequest::Rotation(&trusted(ada)),
                    &rotation.secret.code_at(at(now + 30)),
                    at(now + 30),
                )
                .await
                .expect("rotated");
        } else {
            harness
                .service
                .disable(ada, &trusted(ada), at(now))
                .await
                .expect("disabled");
        }
        assert_eq!(
            harness.service.sign_in_is_current(&signed_in).await,
            Ok(false),
            "{change}"
        );
    }
}

/// The pending seed is an enrollment's while the account has no factor and
/// a rotation's while it has one: a confirmation or a second look of the
/// other kind never checks or shows it.
#[tokio::test]
async fn a_pending_seed_is_never_checked_or_shown_as_the_other_kind() {
    let harness = harness();
    let ada = PrincipalId::new();
    enrolled(&harness, ada).await;
    let now = START + 1500;
    let rotation = harness
        .service
        .begin(
            ada,
            MfaChangeRequest::Rotation(&trusted(ada)),
            ISSUER,
            ACCOUNT,
            at(now),
        )
        .await
        .expect("rotation");
    for code in [
        "000000",
        "111111",
        rotation.secret.code_at(at(now)).as_str(),
    ] {
        assert_eq!(
            harness
                .service
                .confirm(ada, MfaChangeRequest::Enrollment, code, at(now))
                .await
                .err(),
            Some(MfaError::AlreadyEnabled),
            "never InvalidCode, which would show the pending seed again"
        );
    }
    assert!(
        harness
            .service
            .pending(ada, MfaChange::Enrollment, ISSUER, ACCOUNT, at(now))
            .await
            .expect("pending")
            .is_none()
    );
    assert!(
        harness
            .store
            .pending(ada, MfaChange::Rotation, at(now))
            .await
            .expect("pending")
            .is_some()
    );

    let bob = PrincipalId::new();
    let enrollment = harness
        .service
        .begin(bob, MfaChangeRequest::Enrollment, ISSUER, ACCOUNT, at(now))
        .await
        .expect("enrollment");
    assert!(
        harness
            .service
            .pending(bob, MfaChange::Rotation, ISSUER, ACCOUNT, at(now))
            .await
            .expect("pending")
            .is_none()
    );
    for code in ["000000", enrollment.secret.code_at(at(now)).as_str()] {
        assert_eq!(
            harness
                .service
                .confirm(
                    bob,
                    MfaChangeRequest::Rotation(&trusted(bob)),
                    code,
                    at(now)
                )
                .await
                .err(),
            Some(MfaError::NotEnabled)
        );
    }
}

/// The pending seed of a rotation sits where an enrollment's does: an
/// enrollment confirmation of an account with a factor is refused
/// explicitly, leaving the factor, the pending rotation and the sessions
/// untouched.
#[tokio::test]
async fn an_enrollment_confirmation_of_an_account_with_a_factor_is_refused() {
    let harness = harness();
    let ada = PrincipalId::new();
    let (old, _) = enrolled(&harness, ada).await;
    let now = START + 1500;
    let rotation = harness
        .service
        .begin(
            ada,
            MfaChangeRequest::Rotation(&trusted(ada)),
            ISSUER,
            ACCOUNT,
            at(now),
        )
        .await
        .expect("rotation");
    let revoked = harness.revoked.lock().expect("lock").len();
    assert_eq!(
        harness
            .service
            .confirm(
                ada,
                MfaChangeRequest::Enrollment,
                &rotation.secret.code_at(at(now)),
                at(now)
            )
            .await
            .err(),
        Some(MfaError::AlreadyEnabled)
    );
    assert_eq!(harness.revoked.lock().expect("lock").len(), revoked);
    assert_eq!(
        harness
            .service
            .verify_second_factor(ada, &old.code_at(at(now)), at(now))
            .await,
        Ok(SecondFactor::Totp),
        "the old factor is still the active one"
    );
    // The pending rotation is still confirmable as a rotation.
    harness
        .service
        .confirm(
            ada,
            MfaChangeRequest::Rotation(&trusted(ada)),
            &rotation.secret.code_at(at(now + 30)),
            at(now + 30),
        )
        .await
        .expect("rotated");

    // And a rotation of an account without a factor is refused as such.
    let bob = PrincipalId::new();
    let enrollment = harness
        .service
        .begin(bob, MfaChangeRequest::Enrollment, ISSUER, ACCOUNT, at(now))
        .await
        .expect("begin");
    assert_eq!(
        harness
            .service
            .confirm(
                bob,
                MfaChangeRequest::Rotation(&trusted(bob)),
                &enrollment.secret.code_at(at(now)),
                at(now)
            )
            .await
            .err(),
        Some(MfaError::NotEnabled)
    );
    assert_eq!(harness.service.status(bob).await, Ok(None));
}

#[tokio::test]
async fn a_wrong_key_or_tampered_seed_fails_closed() {
    let harness = harness();
    let (ada, tenant) = (PrincipalId::new(), TenantId::new());
    let (secret, _) = enrolled(&harness, ada).await;
    let now = START + 1800;
    let token = challenge(&harness, ada, tenant, now).await;

    // The same store read with another key: the seed does not open.
    let other: Arc<dyn SecretCipher> = Arc::new(Aes256GcmSecretCipher::new(&[8; 32]).expect("key"));
    let throttle = Arc::new(MemoryLoginThrottle::new(mfa_throttle_config()).expect("config"));
    let wrong_key = MfaService::new(harness.store.clone(), Some(other), throttle);
    let error = wrong_key
        .complete(
            token.expose(),
            &binding(1),
            &secret.code_at(at(now)),
            source(1),
            at(now),
        )
        .await
        .err()
        .map(|rejection| rejection.error);
    assert_eq!(error, Some(MfaError::Mechanism));
    assert_eq!(
        wrong_key
            .verify_second_factor(ada, &secret.code_at(at(now)), at(now))
            .await,
        Err(MfaError::Mechanism)
    );

    // A tampered ciphertext under the right key.
    let active = harness
        .store
        .active(ada)
        .await
        .expect("store")
        .expect("factor");
    let mut ciphertext = active.sealed.ciphertext().to_vec();
    let last = ciphertext.len() - 1;
    ciphertext[last] ^= 0x80;
    let tampered = SealedTotpSecret::new(ciphertext, *active.sealed.digest());
    let cipher = Aes256GcmSecretCipher::new(&[7; 32]).expect("key");
    assert!(TotpSecret::open(&cipher, &tampered).is_err());
}

#[tokio::test]
async fn without_a_key_every_operation_but_status_fails_closed() {
    let harness = harness();
    let ada = PrincipalId::new();
    enrolled(&harness, ada).await;
    let throttle = Arc::new(MemoryLoginThrottle::new(mfa_throttle_config()).expect("config"));
    let unconfigured = MfaService::new(harness.store.clone(), None, throttle);
    assert!(!unconfigured.is_configured());
    assert!(matches!(unconfigured.status(ada).await, Ok(Some(_))));
    let now = at(START + 2000);
    assert_eq!(
        unconfigured
            .issue_challenge(
                ada,
                TenantId::new(),
                version(1),
                binding(1),
                at(START + 9000),
                now
            )
            .await
            .err(),
        Some(MfaError::NotConfigured)
    );
    assert_eq!(
        unconfigured
            .complete("x", &binding(1), "123456", source(1), now)
            .await
            .err()
            .map(|rejection| rejection.error),
        Some(MfaError::NotConfigured)
    );
    assert_eq!(
        unconfigured.disable(ada, &trusted(ada), now).await,
        Err(MfaError::NotConfigured)
    );
    assert_eq!(
        unconfigured
            .begin(
                PrincipalId::new(),
                MfaChangeRequest::Enrollment,
                ISSUER,
                ACCOUNT,
                now
            )
            .await
            .err(),
        Some(MfaError::NotConfigured)
    );
    assert_eq!(
        unconfigured.verify_second_factor(ada, "123456", now).await,
        Err(MfaError::NotConfigured)
    );
    assert_eq!(
        unconfigured.verify_step_up(ada, "123456", now).await,
        Err(MfaError::NotConfigured)
    );
    assert!(format!("{unconfigured:?}").contains("configured: false"));
}

#[tokio::test]
async fn labels_are_validated_before_anything_is_stored() {
    let harness = harness();
    let ada = PrincipalId::new();
    assert_eq!(
        harness
            .service
            .begin(ada, MfaChangeRequest::Enrollment, "", ACCOUNT, at(START))
            .await
            .err(),
        Some(MfaError::InvalidLabel)
    );
    assert_eq!(
        harness
            .store
            .pending(ada, MfaChange::Enrollment, at(START))
            .await,
        Ok(None)
    );
}
