//! Scoped retrieval of signed release findings and package discussions.
use crate::{comments, model::Release, trust::parse_attestation};
use anyhow::Result;
use nostr::{Alphabet, Event, Filter, Kind, SingleLetterTag};
use nostr_identity::FACT_SNAPSHOT_KIND;
use nostr_pubsub::NostrEventSubscriber;
use nostr_pubsub_relay::RelayEventBus;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

const LIMIT: usize = 512;

pub fn valid(event: &Event) -> bool {
    serde_json::to_vec(event).is_ok_and(|bytes| bytes.len() <= 32_768)
        && event.created_at.as_secs() <= nostr::Timestamp::now().as_secs().saturating_add(300)
        && (parse_attestation(event).is_ok() || comments::verify(event).is_ok())
}

/// Keep listening past EOSE; a bounded observation cannot establish absence.
pub async fn refresh(bus: &RelayEventBus, releases: &[Release]) -> Result<Vec<Event>> {
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
            .kind(Kind::Comment)
            .custom_tags(SingleLetterTag::uppercase(Alphabet::E), ids)
            .limit(LIMIT),
        Filter::new()
            .kind(Kind::Comment)
            .custom_tags(SingleLetterTag::uppercase(Alphabet::A), packages)
            .limit(LIMIT),
    ];
    let (sender, mut receiver) = tokio::sync::mpsc::channel(LIMIT);
    let subscription = bus
        .subscribe(
            filters.clone(),
            Arc::new(move |event| {
                let _ = sender.try_send(event.event.into_event());
            }),
        )
        .await?;
    let deadline = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(deadline);
    let mut events = BTreeMap::new();
    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = receiver.recv() => match event {
                Some(event) => {
                    if valid(&event) && filters.iter().any(|f| f.match_event(&event, Default::default())) {
                        events.insert(event.id, event);
                        if events.len() >= LIMIT { break; }
                    }
                }
                None => break,
            }
        }
    }
    drop(subscription);
    Ok(events.into_values().collect())
}
