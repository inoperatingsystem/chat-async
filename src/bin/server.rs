use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::{Sender, channel};
use tokio::sync::RwLock;
use tokio_websockets::{Message, ServerBuilder, WebSocketStream};

// ── JSON protocol types (must match YewChat client) ──────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MsgTypes {
    Users,
    Register,
    Message,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebSocketMessage {
    message_type: MsgTypes,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_array: Option<Vec<String>>,
}

#[derive(Serialize)]
struct ChatMessageData {
    from: String,
    message: String,
    time: u128,
}

// ── Shared application state ─────────────────────────────────────────

struct AppState {
    users: RwLock<HashMap<SocketAddr, String>>, // addr → nickname
    tx: Sender<String>,                         // broadcast channel
}

// ── Helper: build and broadcast the current user list ────────────────

async fn broadcast_user_list(state: &AppState) {
    let users = state.users.read().await;
    let user_list: Vec<String> = users.values().cloned().collect();
    let msg = WebSocketMessage {
        message_type: MsgTypes::Users,
        data: None,
        data_array: Some(user_list),
    };
    let _ = state.tx.send(serde_json::to_string(&msg).unwrap());
}

// ── Per-connection handler ───────────────────────────────────────────

async fn handle_connection(
    addr: SocketAddr,
    mut ws_stream: WebSocketStream<TcpStream>,
    state: Arc<AppState>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut bcast_rx = state.tx.subscribe();

    // A continuous loop for concurrently performing two tasks: (1) receiving
    // messages from `ws_stream` and processing them, and (2) receiving
    // messages on `bcast_rx` and sending them to the client.
    loop {
        tokio::select! {
            incoming = ws_stream.next() => {
                match incoming {
                    Some(Ok(msg)) => {
                        if let Some(text) = msg.as_text() {
                            println!("From client {addr:?} {text:?}");

                            // Parse the JSON message from the client
                            let ws_msg: WebSocketMessage = match serde_json::from_str(text) {
                                Ok(m) => m,
                                Err(e) => {
                                    eprintln!("Failed to parse message from {addr}: {e}");
                                    continue;
                                }
                            };

                            match ws_msg.message_type {
                                MsgTypes::Register => {
                                    // Store the user's nickname
                                    if let Some(nick) = ws_msg.data {
                                        println!("User registered: {nick} ({addr})");
                                        state.users.write().await.insert(addr, nick);
                                        broadcast_user_list(&state).await;
                                    }
                                }
                                MsgTypes::Message => {
                                    // Look up the sender's nickname
                                    let nick = {
                                        let users = state.users.read().await;
                                        users.get(&addr).cloned()
                                            .unwrap_or_else(|| addr.to_string())
                                    };

                                    // Build the inner message payload (double-serialized)
                                    let chat_data = ChatMessageData {
                                        from: nick,
                                        message: ws_msg.data.unwrap_or_default(),
                                        time: SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .unwrap()
                                            .as_millis(),
                                    };

                                    let outgoing = WebSocketMessage {
                                        message_type: MsgTypes::Message,
                                        data: Some(serde_json::to_string(&chat_data).unwrap()),
                                        data_array: None,
                                    };
                                    let _ = state.tx.send(
                                        serde_json::to_string(&outgoing).unwrap()
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Err(err)) => return Err(err.into()),
                    None => break, // client disconnected
                }
            }
            msg = bcast_rx.recv() => {
                ws_stream.send(Message::text(msg?)).await?;
            }
        }
    }

    // Client disconnected — remove from user list and notify everyone
    println!("Client disconnected: {addr}");
    state.users.write().await.remove(&addr);
    broadcast_user_list(&state).await;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let (tx, _) = channel(16);

    let state = Arc::new(AppState {
        users: RwLock::new(HashMap::new()),
        tx,
    });

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("listening on port 8080");

    loop {
        let (socket, addr) = listener.accept().await?;
        println!("New connection from {addr:?}");
        let state = state.clone();
        tokio::spawn(async move {
            // Wrap the raw TCP stream into a websocket.
            let (_req, ws_stream) = ServerBuilder::new().accept(socket).await?;

            handle_connection(addr, ws_stream, state).await
        });
    }
}