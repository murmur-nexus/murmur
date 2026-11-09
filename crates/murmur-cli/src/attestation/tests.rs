use super::*;

use super::minting::{self, signer_for, BundleSpec, Minter};

/// The attestation GitHub's `actions/attest-build-provenance` made for the `cli/cli` v2.103.3
/// release; `testdata/README.md` records where it came from.
const REAL_BUNDLE: &[u8] = include_bytes!("testdata/cli-cli-v2.103.3-linux-amd64.sigstore.json");
/// sha256 of `gh_2.103.3_linux_amd64.tar.gz`, one of the bundle's 21 subjects.
const REAL_DIGEST: &str = "4c806978ffce1cec70853302a56d70ac6da4f19bae7adca4e1a8ab00633dd91d";
const REAL_REPO: &str = "cli/cli";
const REAL_OWNER_ID: u64 = 59_704_711;
const REAL_SIGNER: &str = "https://github.com/cli/cli/.github/workflows/deployment.yml";

const REPO: &str = "ana/artifacts";
const OWNER: u64 = 1111;

fn digest_of(bytes: &[u8]) -> String {
    minting::sha256_hex(bytes)
}

fn expected(sha256: &str) -> Expected<'_> {
    Expected {
        sha256,
        owner_id: Some(OWNER),
        repo: Some(REPO),
    }
}

/// Mints the bundle `edit` makes of a verifying one and verifies it against the minter's root.
fn verdict(edit: impl FnOnce(&mut BundleSpec)) -> Verdict {
    let minter = Minter::new();
    let root = TrustedRoot::from_json(&minter.trusted_root_json()).unwrap();
    let sha = digest_of(b"payload");
    let mut spec = BundleSpec::new(REPO, OWNER, &sha);
    edit(&mut spec);
    verify_bundle(&minter.bundle(&spec), &expected(&sha), &root)
}

fn refused(edit: impl FnOnce(&mut BundleSpec)) -> String {
    match verdict(edit) {
        Verdict::Refused(reason) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn unverifiable(edit: impl FnOnce(&mut BundleSpec)) -> String {
    match verdict(edit) {
        Verdict::Unverifiable(reason) => reason,
        other => panic!("expected unverifiable, got {other:?}"),
    }
}

#[test]
fn the_built_in_root_verifies_a_real_github_attestation() {
    let root = trusted_root::built_in();
    let real = Expected {
        sha256: REAL_DIGEST,
        owner_id: Some(REAL_OWNER_ID),
        repo: Some(REAL_REPO),
    };
    match verify_bundle(REAL_BUNDLE, &real, &root) {
        Verdict::Verified { signer, signed_at } => {
            assert_eq!(signer, REAL_SIGNER);
            assert_eq!(signed_at.timestamp(), 1_793_042_295);
        }
        other => panic!("the real bundle did not verify: {other:?}"),
    }

    let changed = "5".repeat(64);
    let digest = Expected {
        sha256: &changed,
        ..real
    };
    assert!(
        matches!(verify_bundle(REAL_BUNDLE, &digest, &root), Verdict::Refused(reason) if reason.contains(&changed)),
    );

    let owner = Expected {
        owner_id: Some(REAL_OWNER_ID + 1),
        ..real
    };
    assert!(
        matches!(verify_bundle(REAL_BUNDLE, &owner, &root), Verdict::Refused(reason) if reason.contains("59704711")),
    );

    let rewritten = String::from_utf8(REAL_BUNDLE.to_vec()).unwrap().replacen(
        "application/vnd.dev.sigstore.bundle.v0.3+json",
        "application/vnd.dev.sigstore.bundle.v9.9+json",
        1,
    );
    assert!(matches!(
        verify_bundle(rewritten.as_bytes(), &real, &root),
        Verdict::Unverifiable(reason) if reason.contains("v9.9")
    ));
}

#[test]
fn the_real_attestation_does_not_verify_against_a_test_root() {
    let minter = Minter::new();
    let root = TrustedRoot::from_json(&minter.trusted_root_json()).unwrap();
    let real = Expected {
        sha256: REAL_DIGEST,
        owner_id: Some(REAL_OWNER_ID),
        repo: Some(REAL_REPO),
    };
    assert!(matches!(
        verify_bundle(REAL_BUNDLE, &real, &root),
        Verdict::Refused(reason) if reason.contains("log key the trusted root does not hold")
    ));
}

#[test]
fn timing_of_one_verify_bundle_on_the_real_fixture() {
    let root = trusted_root::built_in();
    let real = Expected {
        sha256: REAL_DIGEST,
        owner_id: Some(REAL_OWNER_ID),
        repo: Some(REAL_REPO),
    };
    let _ = verify_bundle(REAL_BUNDLE, &real, &root);
    let rounds = 20u32;
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        let _ = verify_bundle(REAL_BUNDLE, &real, &root);
    }
    let parse = std::time::Instant::now();
    let _ = trusted_root::built_in();
    eprintln!(
        "verify_bundle on the real fixture: {:?} per bundle; built-in root parse: {:?}",
        start.elapsed() / rounds,
        parse.elapsed()
    );
}

#[test]
fn a_minted_bundle_verifies_and_names_its_workflow() {
    match verdict(|_| {}) {
        Verdict::Verified { signer, signed_at } => {
            assert_eq!(signer, signer_for(REPO, "release.yml"));
            assert_eq!(signed_at.timestamp(), minting::NOT_BEFORE + 60);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_signer_falls_back_to_the_san_uri() {
    let verdict = verdict(|spec| {
        spec.build_signer_uri = None;
        spec.san_uri = format!("{}@refs/heads/main", signer_for(REPO, "san.yml"));
    });
    assert!(
        matches!(&verdict, Verdict::Verified { signer, .. } if *signer == signer_for(REPO, "san.yml")),
        "{verdict:?}"
    );
}

#[test]
fn a_v02_bundle_with_a_certificate_chain_is_read() {
    let minter = Minter::new();
    let root = TrustedRoot::from_json(&minter.trusted_root_json()).unwrap();
    let sha = digest_of(b"payload");
    let bundle = minter.bundle(&BundleSpec::new(REPO, OWNER, &sha));
    let mut json: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
    json["mediaType"] = "application/vnd.dev.sigstore.bundle+json;version=0.2".into();
    let material = json["verificationMaterial"].as_object_mut().unwrap();
    let certificate = material.remove("certificate").unwrap();
    material.insert(
        "x509CertificateChain".to_string(),
        serde_json::json!({ "certificates": [certificate] }),
    );
    let bundle = serde_json::to_vec(&json).unwrap();
    assert!(matches!(
        verify_bundle(&bundle, &expected(&sha), &root),
        Verdict::Verified { .. }
    ));
}

// Step 1: form.

#[test]
fn forms_this_mur_does_not_read_are_unverifiable_and_name_what() {
    assert!(unverifiable(
        |spec| spec.media_type = "application/vnd.dev.sigstore.bundle.v9.9+json".into()
    )
    .contains("media type 'application/vnd.dev.sigstore.bundle.v9.9+json'"));
    assert!(unverifiable(|spec| spec.leaf_p384 = true).contains("not ECDSA P-256"));
    assert!(
        unverifiable(|spec| spec.kind = ("intoto".into(), "0.0.2".into()))
            .contains("Rekor entry kind intoto 0.0.2")
    );
    assert!(unverifiable(|spec| spec.inclusion_promise = false).contains("inclusion promise"));

    let minter = Minter::new();
    let root = TrustedRoot::from_json(&minter.trusted_root_json()).unwrap();
    let sha = digest_of(b"payload");
    let bundle = minter.bundle(&BundleSpec::new(REPO, OWNER, &sha));
    let mut json: serde_json::Value = serde_json::from_slice(&bundle).unwrap();
    json.as_object_mut().unwrap().remove("dsseEnvelope");
    let reason = match verify_bundle(&serde_json::to_vec(&json).unwrap(), &expected(&sha), &root) {
        Verdict::Unverifiable(reason) => reason,
        other => panic!("{other:?}"),
    };
    assert!(reason.contains("no DSSE envelope"), "{reason}");

    assert!(matches!(
        verify_bundle(b"not json", &expected(&sha), &root),
        Verdict::Unverifiable(reason) if reason.contains("not a JSON Sigstore bundle")
    ));
    assert!(matches!(
        verify_bundle(b"{}", &expected(&sha), &root),
        Verdict::Unverifiable(_)
    ));
}

// Step 2: envelope.

#[test]
fn the_pae_matches_the_dsse_specification() {
    assert_eq!(
        pae("http://example.com/HelloWorld", b"hello world"),
        b"DSSEv1 29 http://example.com/HelloWorld 11 hello world".to_vec()
    );
}

#[test]
fn a_flipped_signature_byte_is_refused() {
    assert!(refused(|spec| spec.flip_signature = true).contains("DSSE signature does not verify"));
}

#[test]
fn another_payload_type_is_refused() {
    assert!(refused(|spec| spec.payload_type = "text/plain".into()).contains("payload type"));
}

// Step 3: transparency log.

#[test]
fn a_flipped_signed_entry_timestamp_is_refused() {
    assert!(refused(|spec| spec.flip_set = true).contains("signed entry timestamp"));
}

#[test]
fn a_log_key_outside_the_trusted_root_is_refused() {
    assert!(refused(|spec| spec.foreign_log_key = true).contains("log key the trusted root"));
}

#[test]
fn an_inclusion_proof_to_another_root_is_refused() {
    assert!(refused(|spec| spec.wrong_proof_root = true).contains("inclusion proof"));
}

#[test]
fn a_checkpoint_signed_by_another_key_is_refused() {
    assert!(refused(|spec| spec.foreign_checkpoint_key = true).contains("checkpoint"));
}

#[test]
fn a_bundle_without_an_inclusion_proof_rests_on_its_promise() {
    assert!(matches!(
        verdict(|spec| spec.inclusion_proof = false),
        Verdict::Verified { .. }
    ));
}

#[test]
fn a_log_body_that_binds_another_payload_or_signature_is_refused() {
    assert!(
        refused(|spec| spec.body_payload_hash = Some("ab".repeat(32)))
            .contains("different DSSE payload")
    );
    assert!(refused(|spec| spec.body_other_signature = true)
        .contains("different signature or signing certificate"));
}

#[test]
fn rfc6962_inclusion_proofs_check_their_length() {
    let leaf = vec![0u8; 32];
    assert_eq!(
        root_from_inclusion_proof(0, 1, leaf.clone(), &[]),
        Some(leaf.clone())
    );
    assert_eq!(root_from_inclusion_proof(1, 1, leaf.clone(), &[]), None);
    assert_eq!(root_from_inclusion_proof(3, 7, leaf, &[vec![1; 32]]), None);
}

// Step 4: certificate.

#[test]
fn a_certificate_from_an_authority_outside_the_root_is_refused() {
    assert!(refused(|spec| spec.foreign_ca = true).contains("does not chain"));
}

#[test]
fn a_certificate_is_checked_at_the_signing_time() {
    let reason = refused(|spec| spec.integrated_after = minting::LEAF_LIFETIME + 60);
    assert!(reason.contains("does not chain"), "{reason}");
    assert!(reason.contains("2026-01-01 00:11:00 UTC"), "{reason}");
}

// Step 5: identity.

#[test]
fn another_issuer_is_refused() {
    assert!(
        refused(|spec| spec.issuer = Some("https://accounts.example".into()))
            .contains("issued by https://accounts.example")
    );
    // A certificate for a non-GitHub identity carries no repository extensions.
    assert!(refused(|spec| {
        spec.issuer = Some("https://accounts.example".into());
        spec.owner_id = None;
        spec.source_repository_uri = None;
    })
    .contains("issued by https://accounts.example"));
}

#[test]
fn another_owner_is_refused_naming_what_the_certificate_states() {
    let reason = refused(|spec| spec.owner_id = Some("2222".into()));
    assert!(reason.contains("owner id 2222"), "{reason}");
    assert!(
        reason.contains("github.com/ana/artifacts with owner id 1111"),
        "{reason}"
    );
}

#[test]
fn another_repository_is_refused_and_case_is_ignored() {
    let reason = refused(|spec| {
        spec.source_repository_uri = Some("https://github.com/mallory/artifacts".into())
    });
    assert!(
        reason.contains("https://github.com/mallory/artifacts"),
        "{reason}"
    );
    assert!(matches!(
        verdict(|spec| spec.source_repository_uri = Some("https://github.com/Ana/Artifacts".into())),
        Verdict::Verified { .. }
    ));
}

#[test]
fn an_unset_expectation_is_not_compared() {
    let minter = Minter::new();
    let root = TrustedRoot::from_json(&minter.trusted_root_json()).unwrap();
    let sha = digest_of(b"payload");
    let bundle = minter.bundle(&BundleSpec::new("mallory/artifacts", 2222, &sha));
    let open = Expected {
        sha256: &sha,
        owner_id: None,
        repo: None,
    };
    assert!(matches!(
        verify_bundle(&bundle, &open, &root),
        Verdict::Verified { .. }
    ));
    let stored = Expected {
        owner_id: Some(OWNER),
        ..open
    };
    assert!(matches!(
        verify_bundle(&bundle, &stored, &root),
        Verdict::Refused(reason) if reason.contains("owner id 1111")
    ));
}

#[test]
fn a_missing_identity_extension_is_unverifiable() {
    assert!(unverifiable(|spec| spec.issuer = None).contains("57264.1.8"));
    assert!(unverifiable(|spec| spec.owner_id = None).contains("57264.1.17"));
    assert!(unverifiable(|spec| spec.source_repository_uri = None).contains("57264.1.12"));
}

// Step 6: subject.

#[test]
fn a_subject_for_another_digest_is_refused_naming_the_payload_digest() {
    let other = "cd".repeat(32);
    let reason = refused(|spec| spec.subject_sha256 = other.clone());
    let payload = digest_of(b"payload");
    assert!(
        reason.contains(&format!(
            "the attestation does not cover the payload's sha256 {payload}"
        )),
        "{reason}"
    );
}

#[test]
fn another_statement_type_is_unverifiable() {
    assert!(
        unverifiable(|spec| spec.statement_type = "https://in-toto.io/Statement/v0.1".into())
            .contains("statement type")
    );
}

// The trusted root.

#[test]
fn an_unreadable_override_root_is_refused_naming_the_variable() {
    let dir = tempfile::tempdir().unwrap();
    let missing = load_override(&dir.path().join("absent.json")).unwrap_err();
    assert_eq!(missing.code, crate::error::E_IO_003);
    assert!(
        missing.message.contains("MUR_SIGSTORE_TRUSTED_ROOT="),
        "{}",
        missing.message
    );
    assert!(
        missing.message.contains("cannot be read"),
        "{}",
        missing.message
    );

    let garbage = dir.path().join("garbage.json");
    std::fs::write(&garbage, "{\"tlogs\": 7}").unwrap();
    let error = load_override(&garbage).unwrap_err();
    assert!(
        error.message.contains("is not a Sigstore trusted root"),
        "{}",
        error.message
    );
}

#[test]
fn the_built_in_root_holds_the_public_good_log_and_authorities() {
    let root = trusted_root::built_in();
    assert!(root
        .tlogs
        .iter()
        .any(|log| log.key_details == "PKIX_ECDSA_P256_SHA_256"));
    assert!(root.authorities.len() >= 2);
}

#[test]
fn assess_folds_verdicts_into_states() {
    let minter = Minter::new();
    let root = TrustedRoot::from_json(&minter.trusted_root_json()).unwrap();
    let sha = digest_of(b"payload");
    assert_eq!(
        assess(None, &expected(&sha), &root),
        Ok(AttestationState::Absent)
    );
    let bundle = minter.bundle(&BundleSpec::new(REPO, OWNER, &sha));
    assert_eq!(
        assess(Some(&bundle), &expected(&sha), &root),
        Ok(AttestationState::Verified {
            signer: signer_for(REPO, "release.yml")
        })
    );
    assert!(matches!(
        assess(Some(b"{}"), &expected(&sha), &root),
        Ok(AttestationState::Unverifiable(_))
    ));
    let other = digest_of(b"other");
    assert!(assess(Some(&bundle), &expected(&other), &root).is_err());
}

use trusted_root::load_override;
