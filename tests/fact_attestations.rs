use haps::{
    model::{PackageSpec, sign},
    repository::Repository,
    trust::{Attestation, Trust, attest_at, parse_attestation, warn_at},
};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
use nostr_identity::{
    build_fact_snapshot_event_with_created_at_ms, fact, parse_fact_snapshot_event,
};
use std::collections::BTreeMap;

#[tokio::test]
async fn fact_snapshots_migrate_legacy_claims_without_double_counting_or_replay()
-> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let payload = tmp.path().join("stage");
    std::fs::create_dir(&payload)?;
    std::fs::write(payload.join("hello"), b"package bytes")?;
    let author = Keys::generate();
    let me = Keys::generate();
    let friend = Keys::generate();
    let repo = Repository::local(tmp.path().join("catalog"))?;
    let release = repo
        .publish(
            &author,
            PackageSpec {
                name: "hello".into(),
                version: "1.0.0".parse()?,
                target: haps::model::target().into(),
                description: "Demo".into(),
                commands: BTreeMap::from([("hello".into(), "hello".into())]),
                app: None,
                desktop: None,
                source: None,
            },
            &payload,
        )
        .await?;
    let follow = EventBuilder::new(Kind::ContactList, "")
        .tags([Tag::public_key(friend.public_key())])
        .sign_with_keys(&me)?;
    let legacy = sign(
        &friend,
        &format!("haps/attestation/{}", release.event.id),
        &Attestation {
            schema: "haps.attestation.v1".into(),
            release: release.event.id.to_hex(),
            approved: true,
            warning: false,
            note: "Legacy check".into(),
        },
    )?;
    let at = legacy.created_at.as_secs() * 1000;
    let revoked = attest_at(
        &friend,
        release.event.id,
        false,
        "Found a regression".into(),
        at + 1,
    )?;
    let approved = attest_at(
        &friend,
        release.event.id,
        true,
        "Rechecked the fix".into(),
        at + 2,
    )?;
    assert!(revoked.content.is_empty());
    assert_eq!(
        parse_fact_snapshot_event(&revoked)?.subject,
        release.event.id.to_hex()
    );
    // Import order and replay must not change the latest signer's decision.
    for events in [
        vec![legacy.clone(), revoked.clone()],
        vec![revoked.clone(), legacy.clone()],
    ] {
        let mut trust = Trust::new(me.public_key().to_hex());
        trust.ingest(follow.clone())?;
        for event in events {
            trust.ingest(event)?;
        }
        assert!(trust.authorize(&release, false, 1).is_err());
        trust.ingest(approved.clone())?;
        trust.ingest(legacy.clone())?;
        trust.ingest(revoked.clone())?;
        assert_eq!(
            trust.attesters(&release),
            vec![friend.public_key().to_hex()]
        );
        trust.authorize(&release, false, 1)?;
        assert!(trust.authorize(&release, false, 2).is_err());
        // An unrelated signer cannot revoke this friend's claim.
        trust.ingest(attest_at(
            &author,
            release.event.id,
            false,
            "Dispute".into(),
            at + 3,
        )?)?;
        trust.authorize(&release, false, 1)?;
        // A trusted warning supersedes approval, survives replay, and blocks even
        // when the caller accepts an otherwise untrusted publisher.
        let warning = warn_at(
            &friend,
            release.event.id,
            true,
            "Unexpected network access".into(),
            at + 4,
        )?;
        trust.ingest(warning.clone())?;
        trust.ingest(approved.clone())?;
        assert_eq!(trust.warnings(&release).len(), 1);
        assert!(trust.attesters(&release).is_empty());
        assert!(trust.authorize(&release, true, 0).is_err());
        trust.authorize_with_policy(&release, true, 0, true)?;
        // Withdrawing a warning is neutral, never a new endorsement.
        trust.ingest(warn_at(
            &friend,
            release.event.id,
            false,
            "Withdrawn after checking".into(),
            at + 5,
        )?)?;
        trust.ingest(warning.clone())?;
        assert!(trust.warnings(&release).is_empty());
        assert!(trust.attesters(&release).is_empty());
        trust.authorize(&release, true, 0)?;
        // Strangers and second-degree connections have no warning authority.
        let stranger = Keys::generate();
        trust.ingest(warn_at(
            &stranger,
            release.event.id,
            true,
            "Stranger claim".into(),
            at + 6,
        )?)?;
        trust.ingest(
            EventBuilder::new(Kind::ContactList, "")
                .tags([Tag::public_key(stranger.public_key())])
                .sign_with_keys(&friend)?,
        )?;
        assert_eq!(trust.distance(&stranger.public_key().to_hex()), Some(2));
        assert!(trust.warnings(&release).is_empty());
        trust.authorize(&release, true, 0)?;
        // Muting a direct connection excludes both its approval and its warning.
        trust.ingest(warn_at(
            &friend,
            release.event.id,
            true,
            "Another finding".into(),
            at + 7,
        )?)?;
        trust.ingest(
            EventBuilder::new(Kind::MuteList, "")
                .tags([Tag::public_key(friend.public_key())])
                .sign_with_keys(&me)?,
        )?;
        assert!(trust.warnings(&release).is_empty());
        trust.authorize(&release, true, 0)?;
    }
    // A followed publisher can recall its own release; self-endorsements still
    // never satisfy the reader's attestation threshold.
    let mut recall = Trust::new(me.public_key().to_hex());
    recall.ingest(
        EventBuilder::new(Kind::ContactList, "")
            .tags([Tag::public_key(author.public_key())])
            .sign_with_keys(&me)?,
    )?;
    recall.ingest(attest_at(
        &author,
        release.event.id,
        true,
        "Publisher approval".into(),
        at,
    )?)?;
    assert!(recall.attesters(&release).is_empty());
    recall.ingest(warn_at(
        &author,
        release.event.id,
        true,
        "Build recalled".into(),
        at + 1,
    )?)?;
    assert_eq!(recall.warnings(&release).len(), 1);
    assert!(recall.authorize(&release, false, 0).is_err());
    let contradictory = build_fact_snapshot_event_with_created_at_ms(
        &friend,
        release.event.id.to_hex(),
        [
            fact("type", &["haps_release_attestation"]),
            fact("schema", &["1"]),
            fact("approved", &["true"]),
            fact("warning", &["true"]),
            fact("note", &["Contradictory"]),
        ],
        [],
        at / 1000,
        at,
    )?;
    assert!(parse_attestation(&contradictory).is_err());
    // A forged claim is rejected even if its profile tags are otherwise valid.
    let mut forged = approved;
    forged.pubkey = author.public_key();
    assert!(parse_attestation(&forged).is_err());
    for (subject, kind, approved) in [
        (release.event.id.to_hex(), "identity_link", "true"),
        (release.event.id.to_hex(), "haps_release_attestation", "yes"),
        ("not-a-release".into(), "haps_release_attestation", "true"),
    ] {
        let event = build_fact_snapshot_event_with_created_at_ms(
            &friend,
            subject,
            [
                fact("type", &[kind]),
                fact("schema", &["1"]),
                fact("approved", &[approved]),
                fact("note", &["Claim"]),
            ],
            [],
            at / 1000,
            at,
        )?;
        assert!(parse_attestation(&event).is_err());
    }
    // A later legacy revocation is still accepted during migration.
    let mut trust = Trust::new(me.public_key().to_hex());
    trust.ingest(follow)?;
    trust.ingest(attest_at(
        &friend,
        release.event.id,
        true,
        "Fact check".into(),
        at,
    )?)?;
    let revoked = sign(
        &friend,
        &format!("haps/attestation/{}", release.event.id),
        &Attestation {
            schema: "haps.attestation.v1".into(),
            release: release.event.id.to_hex(),
            approved: false,
            warning: false,
            note: "Revoke".into(),
        },
    )?;
    trust.ingest(
        EventBuilder::new(revoked.kind, revoked.content)
            .tags(revoked.tags)
            .custom_created_at(Timestamp::from(at / 1000 + 1))
            .sign_with_keys(&friend)?,
    )?;
    trust.ingest(legacy)?;
    assert!(trust.authorize(&release, false, 1).is_err());
    Ok(())
}
