use axum::{
    extract::{Path, State, WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    sync::{Arc},
};
use axum::extract::ws::Utf8Bytes;
use tokio::sync::{broadcast, mpsc};
use tower_http::trace::TraceLayer;
use tracing::{info, error};
use tokio_util::sync::CancellationToken;

// Type for transaction ID
type TxId = String;

// Type for callback data
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CallbackData {
    tx_id: TxId,
    timestamp: u64,
    payload: serde_json::Value,
}

// Application shared state
#[derive(Clone)]
struct AppState {
    // Store pending callbacks that haven't been consumed yet
    pending_callbacks: Arc<DashMap<TxId, CallbackData>>,
    // Channels to notify connected clients about new callbacks
    notification_channels: Arc<DashMap<TxId, broadcast::Sender<CallbackData>>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            pending_callbacks: Arc::new(DashMap::new()),
            notification_channels: Arc::new(DashMap::new()),
        }
    }

    // Get or create a notification channel for a tx_id
    fn get_or_create_channel(&self, tx_id: &TxId) -> broadcast::Sender<CallbackData> {
        if let Some(sender) = self.notification_channels.get(tx_id) {
            sender.clone()
        } else {
            let (sender, _) = broadcast::channel(100); // Buffer size of 100
            self.notification_channels.insert(tx_id.clone(), sender.clone());
            sender
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()>  {

    // Create a cancellation token for a graceful shutdown
    let cancel_token = CancellationToken::new();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // Create a shared state
    let state = AppState::new();

    // Build our application with routes
    let app = Router::new()
        // Callback endpoint
        .route("/api/callback/{tx_id}", post(receive_callback))
        // WebSocket endpoint
        .route("/ws/callback/{tx_id}", get(websocket_handler))
        // Add a shared state
        .with_state(state)
        // Add tracing
        .layer(TraceLayer::new_for_http());

    // Create a future for a graceful shutdown
    let server_shutdown = {
        let token = cancel_token.clone();
        async move {
            token.cancelled().await;
            info!("Server shutting down gracefully...");
        }
    };

    // Run it
    let addr = SocketAddr::from(([0, 0, 0, 0], 5000));
    let listener = tokio::net::TcpListener::bind(addr.clone()).await?;

    info!("Listening on {}", addr.clone());

    // Run the server with graceful shutdown
    tokio::select! {
        result = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(server_shutdown) => {
            if let Err(e) = result {
                error!("Server error: {:?}", e);
                return Err(anyhow::anyhow!("Server error: {:?}", e));
            }
        }
        _ = cancel_token.cancelled() => {
            info!("Received shutdown signal");
        }
    }

    Ok(())
}

// Handler for receiving callbacks
async fn receive_callback(
    Path(tx_id): Path<TxId>,
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    info!("Received callback for tx_id: {}", tx_id);

    // Create callback data
    let callback_data = CallbackData {
        tx_id: tx_id.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        payload,
    };

    // Store in pending callbacks
    state.pending_callbacks.insert(tx_id.clone(), callback_data.clone());

    // Notify any connected clients
    if let Some(sender) = state.notification_channels.get(&tx_id) {
        let _ = sender.send(callback_data);
    }

    StatusCode::OK
}

// Handler for WebSocket connections
async fn websocket_handler(
    Path(tx_id): Path<TxId>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    info!("WebSocket connection attempt for tx_id: {}", tx_id);

    // Upgrade the connection to WebSocket
    ws.on_upgrade(move |socket| websocket_callback(socket, tx_id, state))
}

// Modify the websocket_callback function
async fn websocket_callback(
    socket: axum::extract::ws::WebSocket,
    tx_id: TxId,
    state: AppState,
) {
    info!("WebSocket connection established for tx_id: {}", tx_id.clone());

    let (mut sender, mut receiver) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel();

    // Get or create a channel for this tx_id
    let notification_sender = state.get_or_create_channel(&tx_id.clone());
    let mut notification_receiver = notification_sender.subscribe();

    let cloned_tx_id = tx_id.clone();
    // Spawn the send_task to handle notifications and outgoing messages
    let mut send_task = tokio::spawn(async move {
        // Check for pending callback first
        if let Some(callback) = state.pending_callbacks.get(&cloned_tx_id.clone()) {
            let callback_data = callback.clone();
            if let Ok(json) = serde_json::to_string(&callback_data) {
                if let Err(e) = sender
                    .send(axum::extract::ws::Message::Text(Utf8Bytes::from(json)))
                    .await
                {
                    error!("Error sending pending callback: {}", e);
                    return;
                }
            }
            state.pending_callbacks.remove(&cloned_tx_id.clone());
            info!("Sent pending callback for tx_id: {}", cloned_tx_id.clone());
        }

        loop {
            tokio::select! {
                // Handle notifications from the broadcast channel
                result = notification_receiver.recv() => {
                    match result {
                        Ok(callback_data) => {
                            if let Ok(json) = serde_json::to_string(&callback_data) {
                                if let Err(e) = sender.send(axum::extract::ws::Message::Text(Utf8Bytes::from(json))).await {
                                    error!("Error sending notification: {}", e);
                                    break;
                                }
                            }
                            state.pending_callbacks.remove(&callback_data.tx_id);
                            info!("Sent notification for tx_id: {}", callback_data.tx_id);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("Notification channel closed for tx_id: {}", cloned_tx_id.clone());
                            break;
                        }
                        Err(e) => {
                            error!("Error receiving notification: {}", e);
                            break;
                        }
                    }
                }
                // Handle outgoing messages from the receive_task
                outgoing_msg = outgoing_rx.recv() => {
                    match outgoing_msg {
                        Some(msg) => {
                            if let Err(e) = sender.send(msg).await {
                                error!("Error sending outgoing message: {}", e);
                                break;
                            }
                        }
                        None => {
                            break;
                        }
                    }
                }
            }
        }

        // Close the WebSocket connection gracefully
        let _ = sender.close().await;
        info!("Send task completed for tx_id: {}", cloned_tx_id.clone());
    });

    // Spawn the receive_task to handle incoming messages
    let cloned_tx = tx_id.clone();
    let cloned_outgoing_tx = outgoing_tx.clone();
    let receive_task = tokio::spawn(async move {
        while let Some(Ok(message)) = receiver.next().await {
            match message {
                axum::extract::ws::Message::Close(_) => {
                    info!("WebSocket closed for tx_id: {}", cloned_tx);
                    break;
                }
                axum::extract::ws::Message::Ping(ping) => {
                    // Send Pong response via the outgoing channel
                    let pong = axum::extract::ws::Message::Pong(ping);
                    if cloned_outgoing_tx.send(pong).is_err() {
                        error!("Failed to send pong for tx_id: {}", cloned_tx);
                        break;
                    }
                }
                _ => {}
            }
        }
        info!("Receive task completed for tx_id: {}", cloned_tx);
    });

    // Wait for either task to complete
    tokio::select! {
        _ = &mut send_task => {},
        _ = receive_task => {},
    }

    info!("WebSocket connection closed for tx_id: {}", tx_id.clone());
}