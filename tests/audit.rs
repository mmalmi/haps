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
