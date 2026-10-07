//! Scoped retrieval of signed release findings and package discussions.
use crate::{comments, model::Release, trust::parse_attestation};
use anyhow::Result;
use nostr::{Alphabet, Event, Filter, Kind, SingleLetterTag};
use nostr_identity::FACT_SNAPSHOT_KIND;

const LIMIT: usize = 512;

pub fn valid(event: &Event) -> bool {
    serde_json::to_vec(event).is_ok_and(|bytes| bytes.len() <= 32_768)
        && event.created_at.as_secs() <= nostr::Timestamp::now().as_secs().saturating_add(300)
        && (parse_attestation(event).is_ok() || comments::verify(event).is_ok())
}

/// Query indexes and relays through the shared router for exact-release findings.
pub async fn refresh(lookup: &crate::lookup::Lookup, releases: &[Release]) -> Result<Vec<Event>> {
    let releases: Vec<_> = releases.iter().take(128).collect();
    if releases.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<_> = releases.iter().map(|r| r.event.id.to_hex()).collect();
    let packages: Vec<_> = releases
        .iter()
        .map(|r| format!("30078:{}:haps/package/{}", r.author(), r.data.package.name))
        .collect();
    let filters = vec![
        Filter::new()
            .kind(Kind::from(FACT_SNAPSHOT_KIND))
            .identifiers(ids.clone())
            .limit(LIMIT),
        Filter::new()
            .kind(crate::model::APP_KIND)
            .identifiers(ids.iter().map(|id| format!("haps/attestation/{id}")))
            .limit(LIMIT),
        Filter::new()
            .kind(Kind::Comment)
            .custom_tags(SingleLetterTag::uppercase(Alphabet::E), ids)
            .limit(LIMIT),
        Filter::new()
            .kind(Kind::Comment)
            .custom_tags(SingleLetterTag::uppercase(Alphabet::A), packages)
            .limit(LIMIT),
    ];
    Ok(lookup
        .query(filters)
        .await?
        .into_iter()
        .filter(valid)
        .collect())
}
