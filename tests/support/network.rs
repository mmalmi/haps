#[derive(Clone)]
pub struct RelayState {
    pub events: std::sync::Arc<tokio::sync::Mutex<Vec<nostr::Event>>>,
    pub catalog: std::path::PathBuf,
    pub accept: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
pub async fn relay(
    axum::extract::State(state): axum::extract::State<RelayState>,
    upgrade: axum::extract::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    upgrade.on_upgrade(move |mut socket| async move {
        use axum::extract::ws::Message;
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value[0] == "REQ" {
                let filters: Vec<nostr::Filter> = value.as_array().unwrap()[2..]
                    .iter()
                    .map(|filter| serde_json::from_value(filter.clone()).unwrap())
                    .collect();
                for event in state.events.lock().await.iter() {
                    if !filters
                        .iter()
                        .any(|filter| filter.match_event(event, Default::default()))
                    {
                        continue;
                    }
                    if socket
                        .send(Message::Text(
                            serde_json::json!(["EVENT", value[1], event]).to_string(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if socket
                    .send(Message::Text(
                        serde_json::json!(["EOSE", value[1]]).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            } else if value[0] == "EVENT" {
                let event: nostr::Event = serde_json::from_value(value[1].clone()).unwrap();
                event.verify().unwrap();
                let accepted = state.accept.load(std::sync::atomic::Ordering::Relaxed);
                if accepted {
                    let mut events = state.events.lock().await;
                    let same_address = |old: &nostr::Event| {
                        old.pubkey == event.pubkey
                            && old.kind == event.kind
                            && (event.kind.is_replaceable()
                                || (event.kind.is_addressable()
                                    && haps::model::tag_value(old, "d").ok()
                                        == haps::model::tag_value(&event, "d").ok()))
                    };
                    let newer = |old: &nostr::Event| {
                        event.created_at > old.created_at
                            || (event.created_at == old.created_at && event.id < old.id)
                    };
                    if !events
                        .iter()
                        .any(|old| old.id == event.id || (same_address(old) && !newer(old)))
                    {
                        events.retain(|old| !same_address(old));
                        events.push(event.clone());
                    }
                }
                if socket
                    .send(Message::Text(
                        serde_json::json!(["OK", event.id, accepted, ""]).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}
pub async fn catalog_file(
    axum::extract::State(state): axum::extract::State<RelayState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    if haps::model::safe_path(&path).is_err() {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    match std::fs::read(state.catalog.join(path)) {
        Ok(bytes) => bytes.into_response(),
        Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}
