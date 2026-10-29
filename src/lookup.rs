//! One Nostr filter interface for additive Hashtree indexes and relay observations.
use futures::StreamExt;
use nostr::{Event, Filter};
use nostr_pubsub::*;
use nostr_pubsub_relay::RelayEventBus;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

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

/// Independent sources are additive. Consumers can render each verified batch
/// immediately; a source finishing (including EOSE) never proves global absence.
#[derive(Default)]
pub struct Lookup {
    indexes: Vec<(String, Arc<dyn EventBus>)>,
    relay: Option<(Arc<RelayEventBus>, Duration)>,
}
impl Lookup {
    pub fn index(mut self, name: &str, reader: Arc<dyn EventBus>) -> Result<Self> {
        SourceRoute::local_index(name).with_dataset(format!("index:{name}"))?;
        self.indexes.push((name.to_owned(), reader));
        Ok(self)
    }
    pub fn relays(mut self, bus: Arc<RelayEventBus>, window: Duration) -> Result<Self> {
        self.relay = Some((bus, window));
        Ok(self)
    }

    /// Deliver cache/index batches and relay events as they arrive, without
    /// collecting all relays or waiting out a mandatory observation window.
    pub fn query_updates(
        &self,
        filters: Vec<Filter>,
    ) -> futures::stream::BoxStream<'static, Result<Vec<Event>>> {
        let mut streams: Vec<futures::stream::BoxStream<'static, Result<Vec<Event>>>> = Vec::new();
        for (name, reader) in &self.indexes {
            let name = name.clone();
            let reader = reader.clone();
            let filters = filters.clone();
            streams.push(
                futures::stream::once(async move {
                    let report = reader
                        .query(filters.clone(), QueryOptions { limit: Some(2048) })
                        .await
                        .map_err(|error| {
                            PubsubError::Storage(format!("event source {name}: {error}"))
                        })?;
                    Ok(report
                        .events
                        .into_iter()
                        .map(|event| event.event.into_event())
                        .filter(|event| {
                            filters
                                .iter()
                                .any(|f| f.match_event(event, Default::default()))
                        })
                        .collect())
                })
                .boxed(),
            );
        }
        if let Some((bus, window)) = &self.relay {
            for filter in filters {
                let bus = bus.clone();
                let window = *window;
                streams.push(
                    futures::stream::once(async move {
                        // Each relay joins independently within one deadline.
                        // Taking only already-connected relays loses updates from
                        // later connections; the pool's stream setup joins all
                        // relays before delivering any of their events.
                        let deadline = tokio::time::Instant::now() + window;
                        let streams: Vec<_> = bus
                            .client()
                            .relays()
                            .await
                            .into_iter()
                            .map(|(url, relay)| {
                                let filter = filter.clone();
                                futures::stream::once(async move {
                                    let response = tokio::time::timeout_at(deadline, async {
                                        // Subscribe before testing state so a
                                        // connection during setup is not missed.
                                        let mut notifications = relay.notifications();
                                        while !relay.is_connected() {
                                            notifications.recv().await.map_err(|error| {
                                                PubsubError::Storage(format!(
                                                    "relay {url}: {error}"
                                                ))
                                            })?;
                                        }
                                        relay
                                            .stream_events(
                                                filter.clone(),
                                                deadline.saturating_duration_since(
                                                    tokio::time::Instant::now(),
                                                ),
                                                Default::default(),
                                            )
                                            .await
                                            .map_err(|error| {
                                                PubsubError::Storage(format!(
                                                    "relay {url}: {error}"
                                                ))
                                            })
                                    })
                                    .await;
                                    match response {
                                        Ok(Ok(events)) => events
                                            .take_until(tokio::time::sleep_until(deadline))
                                            .map(move |event| {
                                                let event = event.map_err(|error| {
                                                    PubsubError::Storage(format!(
                                                        "relay {url}: {error}"
                                                    ))
                                                })?;
                                                let event =
                                                    VerifiedEvent::try_from(event)?.into_event();
                                                Ok(
                                                    if filter
                                                        .match_event(&event, Default::default())
                                                    {
                                                        vec![event]
                                                    } else {
                                                        Vec::new()
                                                    },
                                                )
                                            })
                                            .boxed(),
                                        Ok(Err(error)) => {
                                            futures::stream::once(async move { Err(error) }).boxed()
                                        }
                                        Err(_) => futures::stream::once(async move {
                                            Err(PubsubError::Storage(format!(
                                                "relay {url}: lookup deadline elapsed"
                                            )))
                                        })
                                        .boxed(),
                                    }
                                })
                                .flatten()
                                .boxed()
                            })
                            .collect();
                        futures::stream::select_all(streams).take(2048).boxed()
                    })
                    .flatten()
                    .boxed(),
                );
            }
        }
        futures::stream::select_all(streams).boxed()
    }

    pub async fn query(&self, filters: Vec<Filter>) -> Result<Vec<Event>> {
        let mut updates = self.query_updates(filters);
        let mut events = BTreeMap::new();
        while let Some(batch) = updates.next().await {
            match batch {
                Ok(batch) => {
                    for event in batch {
                        events.insert(event.id, event);
                    }
                }
                Err(error) => {
                    eprintln!("Event source unavailable; results may be incomplete: {error}")
                }
            }
        }
        Ok(events.into_values().take(2048).collect())
    }
}
