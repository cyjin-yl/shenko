//! WebSocket handler for real-time subject timeline streaming.
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade, Message},
    response::Response,
};
use tracing::{info, warn};

pub async fn ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    info!("WebSocket client connected");

    // Send welcome
    let _ = socket.send(Message::Text(
        serde_json::json!({
            "role": "subject",
            "content": "Shenko 已连接。这是你的持续思考空间。"
        }).to_string().into()
    )).await;

    while let Some(msg) = socket.recv().await {
        match msg {
            Ok(Message::Text(text)) => {
                info!("ws recv: {} bytes", text.len());
                // Echo back for now — full implementation routes to subject inbox
                let reply = serde_json::json!({
                    "role": "subject",
                    "content": format!("[收到，正在处理]")
                });
                let _ = socket.send(Message::Text(reply.to_string().into())).await;
            }
            Ok(Message::Close(_)) => {
                info!("WebSocket client disconnected");
                break;
            }
            Err(e) => {
                warn!("ws error: {e}");
                break;
            }
            _ => {}
        }
    }
}
