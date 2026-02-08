//! WebSocket handlers for dev and production chat.
//!
//! Each handler manages bidirectional communication between the web UI
//! and the platform agents, plus broadcast subscription for real-time updates.

use std::sync::Arc;

use axum::extract::ws::{self, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;

use crate::agent::developer::DeveloperAgent;
use crate::agent::production::ProductionAgent;
use crate::protocol::WsMessage;
use crate::state::PlatformState;

/// WebSocket upgrade handler for the developer chat.
pub async fn ws_dev_handler(
    ws: WebSocketUpgrade,
    State(state): State<PlatformState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_dev_socket(socket, state))
}

/// WebSocket upgrade handler for the production chat.
pub async fn ws_prod_handler(
    ws: WebSocketUpgrade,
    State(state): State<PlatformState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_prod_socket(socket, state))
}

/// Handle a developer WebSocket connection.
async fn handle_dev_socket(socket: WebSocket, state: PlatformState) {
    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    // Create the developer agent
    let agent = Arc::new(Mutex::new(DeveloperAgent::new(
        "default".into(),
        state.api_key.clone(),
    )));

    // Spawn broadcast listener
    let broadcast_sender = sender.clone();
    let mut broadcast_rx = state.subscribe();
    let broadcast_task = tokio::spawn(async move {
        while let Ok(msg) = broadcast_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                let ws_sender = &mut *broadcast_sender.lock().await;
                if ws_sender.send(ws::Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    });

    // Handle incoming messages
    let msg_sender = sender.clone();
    let msg_state = state.clone();
    let msg_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                ws::Message::Text(text) => {
                    let ws_msg: Result<WsMessage, _> = serde_json::from_str(&text);
                    match ws_msg {
                        Ok(WsMessage::Chat { content }) => {
                            let responses = agent.lock().await.handle_message(&content).await;
                            let ws_sender = &mut *msg_sender.lock().await;
                            for response in responses {
                                if let Ok(json) = serde_json::to_string(&response) {
                                    if ws_sender.send(ws::Message::Text(json.into())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(WsMessage::ApprovalResponse {
                            request_id,
                            approved,
                            rationale,
                        }) => {
                            crate::evolution::UnmetIntentCollector::handle_approval(
                                &request_id,
                                approved,
                                rationale.as_deref(),
                                &msg_state,
                            );
                        }
                        _ => {}
                    }
                }
                ws::Message::Close(_) => return,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = broadcast_task => {},
        _ = msg_task => {},
    }
}

/// Handle a production WebSocket connection.
async fn handle_prod_socket(socket: WebSocket, state: PlatformState) {
    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    let agent = Arc::new(Mutex::new(ProductionAgent::new(state)));

    let msg_sender = sender.clone();
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            ws::Message::Text(text) => {
                let ws_msg: Result<WsMessage, _> = serde_json::from_str(&text);
                if let Ok(WsMessage::Chat { content }) = ws_msg {
                    let responses = agent.lock().await.handle_message(&content).await;
                    let ws_sender = &mut *msg_sender.lock().await;
                    for response in responses {
                        if let Ok(json) = serde_json::to_string(&response) {
                            if ws_sender.send(ws::Message::Text(json.into())).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            ws::Message::Close(_) => return,
            _ => {}
        }
    }
}
