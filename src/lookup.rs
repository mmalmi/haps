//! One Nostr filter interface for additive Hashtree indexes and relay observations.
use async_trait::async_trait;
use nostr::{Event, Filter};
use nostr_pubsub::*;
use nostr_pubsub_relay::RelayEventBus;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

struct VerifiedSources;
#[async_trait]
impl PubsubPolicy for VerifiedSources {
    async fn check_event(&self, _: EventPolicyContext<'_>) -> Result<PolicyDecision> {
        Ok(PolicyDecision::allow_with_priority(0))
    }
    async fn check_source(&self, context: SourcePolicyContext<'_>) -> Result<PolicyDecision> {
        Ok(PolicyDecision::allow_with_priority(
            context.candidate.priority,
        ))
    }
}

/// Unlike a one-shot relay query, retain the subscription past EOSE. A completed
/// observation is a time bound, not evidence that all matching events exist here.
pub async fn observe(
    bus: &dyn NostrEventSubscriber,
    filters: Vec<Filter>,
    window: Duration,
) -> Result<Vec<Event>> {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(2048);
    let subscription = bus
        .subscribe(
            filters.clone(),
            Arc::new(move |event| {
                let _ = sender.try_send(event.event.into_event());
            }),
        )
        .await?;
    let deadline = tokio::time::sleep(window);
    tokio::pin!(deadline);
    let mut events = BTreeMap::new();
    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = receiver.recv() => match event {
                Some(event) => {
                    if filters.iter().any(|f| f.match_event(&event, Default::default())) {
                        events.insert(event.id, event);
                        if events.len() >= 2048 { break; }
                    }
                }
                None => break,
            }
        }
    }
    subscription.close().await?;
    Ok(events.into_values().collect())
}

struct RelayObservation {
    bus: Arc<RelayEventBus>,
    window: Duration,
}
#[async_trait]
impl EventBus for RelayObservation {
    async fn publish(&self, _: VerifiedEvent, _: EventSource) -> Result<PublishReport> {
        Err(PubsubError::Validation("lookup is read-only".into()))
    }
    async fn query(&self, filters: Vec<Filter>, options: QueryOptions) -> Result<QueryReport> {
        let events = observe(self.bus.as_ref(), filters, self.window).await?;
        Ok(QueryReport {
            events: events
                .into_iter()
                .take(options.limit.unwrap_or(2048))
                .map(|event| {
                    Ok(QueryEvent {
                        event: event.try_into()?,
                        source: EventSource::relay("configured-relays"),
                        priority: 0,
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}

pub struct Lookup {
    router: NostrPubsubRouter,
}
impl Default for Lookup {
    fn default() -> Self {
        Self {
            router: NostrPubsubRouter::new(Arc::new(VerifiedSources)),
        }
    }
}
impl Lookup {
    pub fn index(mut self, name: &str, reader: Arc<dyn EventBus>) -> Result<Self> {
        // Indexes have independent coverage: do not mark them as ordered replicas.
        let route = SourceRoute::local_index(name).with_dataset(format!("index:{name}"))?;
        self.router = self
            .router
            .with_query_source(RouterQuerySource::from_reader(route, reader));
        Ok(self)
    }
    pub fn relays(mut self, bus: Arc<RelayEventBus>, window: Duration) -> Result<Self> {
        let route = SourceRoute::relay("configured-relays").with_dataset("relays")?;
        self.router = self.router.with_query_source(RouterQuerySource::new(
            route,
            Arc::new(RelayObservation { bus, window }),
        ));
        Ok(self)
    }
    pub async fn query(&self, filters: Vec<Filter>) -> Result<Vec<Event>> {
        let report = self
            .router
            .query_with_context(
                filters,
                RoutedQueryOptions {
                    query: QueryOptions { limit: Some(2048) },
                },
                None,
                None,
            )
            .await?;
        for attempt in report.attempts {
            if let RouteAttemptOutcome::Failure { message } = attempt.outcome {
                eprintln!(
                    "Event source {} unavailable; results may be incomplete: {message}",
                    attempt.route.id
                );
            }
        }
        Ok(report
            .events
            .into_iter()
            .map(|event| event.event.into_event())
            .collect())
    }
}
