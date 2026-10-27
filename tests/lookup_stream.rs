use async_trait::async_trait;
use futures::StreamExt;
use haps::lookup::Lookup;
use nostr::{Event, EventBuilder, Filter, Keys, Kind};
use nostr_pubsub::*;
use std::{sync::Arc, time::Duration};

struct DelayedIndex {
    event: Event,
    ready: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl EventBus for DelayedIndex {
    async fn publish(&self, _: VerifiedEvent, _: EventSource) -> Result<PublishReport> {
        unreachable!()
    }
    async fn query(&self, _: Vec<Filter>, _: QueryOptions) -> Result<QueryReport> {
        self.ready.notified().await;
        Ok(QueryReport {
            events: vec![QueryEvent {
                event: self.event.clone().try_into()?,
                source: EventSource::local_index("fixture"),
                priority: 0,
            }],
        })
    }
}

#[tokio::test]
async fn independent_sources_deliver_before_slow_source_finishes_and_remain_additive()
-> anyhow::Result<()> {
    let keys = Keys::generate();
    let first = EventBuilder::new(Kind::TextNote, "first").sign_with_keys(&keys)?;
    let later = EventBuilder::new(Kind::TextNote, "later").sign_with_keys(&keys)?;
    let fast = Arc::new(tokio::sync::Notify::new());
    let slow = Arc::new(tokio::sync::Notify::new());
    let lookup = Lookup::default()
        .index(
            "fast",
            Arc::new(DelayedIndex {
                event: first.clone(),
                ready: fast.clone(),
            }),
        )?
        .index(
            "slow",
            Arc::new(DelayedIndex {
                event: later.clone(),
                ready: slow.clone(),
            }),
        )?;
    let mut updates = lookup.query_updates(vec![Filter::new().kind(Kind::TextNote)]);
    fast.notify_one();
    let batch = tokio::time::timeout(Duration::from_secs(1), updates.next())
        .await?
        .unwrap()?;
    assert_eq!(batch, vec![first]);
    // No EOSE or completion from the slow source was required for the first batch.
    slow.notify_one();
    let batch = tokio::time::timeout(Duration::from_secs(1), updates.next())
        .await?
        .unwrap()?;
    assert_eq!(batch, vec![later]);
    assert!(updates.next().await.is_none());
    Ok(())
}
