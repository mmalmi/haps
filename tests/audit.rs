use haps::{
    audit::Evidence,
    model::{PackageSpec, target},
    repository::Repository,
    trust::{Trust, attest, attest_at, attest_audit_at},
};
use nostr::{EventBuilder, Keys, Kind, Tag};
use std::{collections::BTreeMap, fs};

#[tokio::test]
async fn installation_requires_an_exact_current_audit_even_with_trust_overrides()
-> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let publisher = Keys::generate();
    let reviewer = Keys::generate();
    let reader = Keys::generate();
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("data"), "fixture")?;
    let repo = Repository::local(tmp.path().join("repo"))?;
    let spec = PackageSpec {
        name: "hello".into(),
        version: "1.0.0".parse()?,
        target: target().into(),
        description: "Audit fixture".into(),
        commands: BTreeMap::new(),
        dependencies: Vec::new(),
        source: None,
        app: None,
        desktop: None,
    };
    let release = repo.publish(&publisher, spec.clone(), &payload).await?;
    let mut trust = Trust::new(reader.public_key().to_hex());
    trust.ingest(
        EventBuilder::new(Kind::ContactList, "")
            .tags([
                Tag::public_key(publisher.public_key()),
                Tag::public_key(reviewer.public_key()),
            ])
            .sign_with_keys(&reader)?,
    )?;
    assert!(trust.authorize_install(&release, true, 0, true).is_err());
    trust.ingest(attest(
        &reviewer,
        release.event.id,
        true,
        "Works for me".into(),
    )?)?;
    assert!(trust.authorize_install(&release, true, 0, true).is_err());
    let at = (nostr::Timestamp::now().as_secs() + 5) * 1000;
    let source_evidence = Evidence {
        schema: "haps.audit.v1".into(),
        method: "source-review".into(),
        provenance: "Reviewed pinned source; binary not verified".into(),
        binary_match: false,
        reviewer: None,
        build: None,
    };
    let source_review = attest_audit_at(
        &reviewer,
        release.event.id,
        "Source review".into(),
        &source_evidence,
        at - 1000,
    )?;
    // Explicitly false keeps pre-source-build clients from counting this as approval.
    assert!(!haps::trust::parse_attestation(&source_review)?.approved);
    assert!(haps::trust::parse_audit(&source_review).is_err());
    trust.ingest(source_review)?;
    assert!(trust.audits(&release).is_empty());
    assert!(trust.attestations(&release).is_empty());
    assert_eq!(trust.source_reviews(&release).len(), 1);
    assert!(trust.authorize_install(&release, true, 0, true).is_err());
    let evidence = Evidence::manual("Reviewed fixture bytes; rebuild not checked".into())?;
    trust.ingest(attest_audit_at(
        &publisher,
        release.event.id,
        "Self review".into(),
        &evidence,
        at,
    )?)?;
    assert!(trust.authorize_install(&release, true, 0, true).is_err());
    let approved = attest_audit_at(
        &reviewer,
        release.event.id,
        "Reviewed fixture".into(),
        &evidence,
        at,
    )?;
    trust.ingest(approved.clone())?;
    trust.authorize_install(&release, false, 0, false)?;
    assert!(trust.authorize_install(&release, true, 2, false).is_err());
    let mut next_spec = spec;
    next_spec.version = "1.1.0".parse()?;
    let next = repo.publish(&publisher, next_spec, &payload).await?;
    assert!(trust.authorize_install(&next, true, 0, true).is_err());
    trust.ingest(attest_at(
        &reviewer,
        release.event.id,
        false,
        "Withdrawn".into(),
        at + 1000,
    )?)?;
    trust.ingest(approved)?;
    assert!(trust.authorize_install(&release, true, 0, true).is_err());
    // A local review is usable by its owner, but never becomes an independent
    // publisher audit for someone else.
    trust.ingest(attest_audit_at(
        &reader,
        next.event.id,
        "My review".into(),
        &evidence,
        at,
    )?)?;
    trust.authorize_install(&next, false, 1, false)?;
    trust.ingest(
        EventBuilder::new(Kind::MuteList, "")
            .tags([Tag::public_key(publisher.public_key())])
            .sign_with_keys(&reader)?,
    )?;
    assert!(trust.authorize_install(&next, true, 0, true).is_err());
    Ok(())
}

#[tokio::test]
async fn recorded_output_must_match_before_it_can_approve_a_binary() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let keys = Keys::generate();
    let publisher = Keys::generate();
    let payload = tmp.path().join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("data"), "fixture")?;
    let repo = Repository::local(tmp.path().join("repo"))?;
    let spec = PackageSpec {
        name: "hello".into(),
        version: "1.0.0".parse()?,
        target: target().into(),
        description: "Audit identity fixture".into(),
        commands: BTreeMap::new(),
        dependencies: vec![],
        source: Some(haps::model::SourceInfo {
            git: "https://example.invalid/source".into(),
            rev: "a".repeat(40),
        }),
        app: None,
        desktop: None,
    };
    let release = repo.publish(&publisher, spec, &payload).await?;
    let mut trust = Trust::new(keys.public_key().to_hex());
    let mut evidence = Evidence::manual("Reviewed source and built output".into())?;
    evidence.method = "agent-review-and-rebuild".into();
    evidence.binary_match = true;
    evidence.build = Some(haps::audit::BuildIdentity {
        recipe_sha256: "a".repeat(64),
        manifest: repo.put_json(&"different output").await?,
    });
    trust.ingest(attest_audit_at(
        &keys,
        release.event.id,
        "Contradictory output".into(),
        &evidence,
        1000,
    )?)?;
    assert!(trust.audits(&release).is_empty());
    evidence.build.as_mut().unwrap().manifest = release.data.manifest.clone();
    trust.ingest(attest_audit_at(
        &keys,
        release.event.id,
        "Matching output".into(),
        &evidence,
        2000,
    )?)?;
    assert_eq!(trust.audits(&release).len(), 1);
    evidence.method = "source-review".into();
    evidence.binary_match = false;
    trust.ingest(attest_audit_at(
        &keys,
        release.event.id,
        "Source only".into(),
        &evidence,
        3000,
    )?)?;
    assert!(trust.audits(&release).is_empty());
    assert_eq!(trust.source_build_reviews(&release).len(), 1);
    trust.ingest(attest_at(
        &keys,
        release.event.id,
        false,
        "Withdrawn".into(),
        4000,
    )?)?;
    assert!(trust.source_build_reviews(&release).is_empty());
    evidence.build.as_mut().unwrap().recipe_sha256 = "not a hash".into();
    assert!(evidence.validate().is_err());
    Ok(())
}
