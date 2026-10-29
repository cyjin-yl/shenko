//! Source-backed loopback OneBot smoke recipe plus a guarded real-service
//! read-only probe — SPEC §15.2, §16, §35.6.
//!
//! * The loopback part always runs offline against a real TCP/WebSocket socket:
//!   request → echo-matched raw response → typed classification.
//! * `live_napcat_readonly_probe` only runs when `SHENKO_NAPCAT_WS_URL` (and
//!   optionally `SHENKO_NAPCAT_ACCOUNT`) point at a real NapCat instance the
//!   operator deliberately selected. It issues *read-only* `get_*` actions
//!   only — it never sends messages, never touches contacts, and asserts
//!   nothing about live data except that the observed identity binding is
//!   reported truthfully. Without the env vars it skips, keeping `cargo test`
//!   deterministic and offline.
use futures::{SinkExt, StreamExt};
use serde_json::json;
use shenko_channel_napcat::{
    ContactQuery, NapCatClient, NapCatConfig, QueryReceipt, SendError,
};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

fn client_for(url: String, account: u64) -> (NapCatClient, tokio::sync::mpsc::Receiver<shenko_channel_napcat::OneBotEvent>, tokio::sync::mpsc::Receiver<shenko_channel_napcat::OneBotAction>) {
    NapCatClient::new(NapCatConfig {
        ws_url: url,
        bot_qq: account,
        reconnect_interval_ms: 50,
        ..Default::default()
    })
}

#[tokio::test]
async fn loopback_query_smoke() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (client, _events, actions) = client_for(url.clone(), 9);
    let cancel = tokio_util::sync::CancellationToken::new();
    let connector = tokio::spawn(NapCatClient::run_connection_loop(
        NapCatConfig {
            ws_url: url,
            bot_qq: 9,
            reconnect_interval_ms: 50,
            ..Default::default()
        },
        client.event_sender(),
        actions,
        client.pending_actions.clone(),
        client.spool.clone(),
        cancel.clone(),
    ));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let echo = request["echo"].as_str().unwrap_or_default().to_string();
            let response = match request["action"].as_str().unwrap_or_default() {
                "get_login_info" => json!({
                    "status": "ok", "retcode": 0, "echo": echo, "self_id": 9,
                    "data": {"user_id": 9, "nickname": "smoke-bot"},
                }),
                "get_group_member_info" => json!({
                    "status": "ok", "retcode": 0, "echo": echo, "self_id": 9,
                    "data": {"group_id": 55, "user_id": 77, "nickname": "成员", "role": "member"},
                }),
                _ => json!({"status": "failed", "retcode": 1404, "echo": echo, "self_id": 9, "data": null}),
            };
            ws.send(Message::Text(response.to_string().into())).await.unwrap();
        }
    });
    let identity: QueryReceipt = tokio::time::timeout(Duration::from_secs(10), async {
        client.query(ContactQuery::LoginInfo).await
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(identity.observed_self_id(), Some(9));
    assert_eq!(identity.raw["data"]["nickname"], "smoke-bot");
    let member = tokio::time::timeout(Duration::from_secs(10), async {
        client
            .query(ContactQuery::GroupMemberInfo {
                group_id: 55,
                user_id: 77,
                no_cache: false,
            })
            .await
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(member.raw["data"]["role"], "member");
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), connector)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}

/// Guarded real-service probe: read-only `get_*` actions against an actual
/// NapCat chosen by the operator. It never contacts people and never sends.
/// Run:
///   SHENKO_NAPCAT_WS_URL=ws://127.0.0.1:3001 SHENKO_NAPCAT_ACCOUNT=<bot qq> \
///   cargo test -p shenko-channel-napcat --test contact_query_smoke -- --nocapture
#[tokio::test]
async fn live_napcat_readonly_probe() {
    let Ok(url) = std::env::var("SHENKO_NAPCAT_WS_URL") else {
        eprintln!("skip: SHENKO_NAPCAT_WS_URL not set — no real NapCat selected");
        return;
    };
    let account: u64 = std::env::var("SHENKO_NAPCAT_ACCOUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let (client, _events, actions) = client_for(url.clone(), account);
    let cancel = tokio_util::sync::CancellationToken::new();
    let config = NapCatConfig {
        ws_url: url.clone(),
        bot_qq: account,
        ..Default::default()
    };
    let connector = tokio::spawn(NapCatClient::run_connection_loop(
        config,
        client.event_sender(),
        actions,
        client.pending_actions.clone(),
        client.spool.clone(),
        cancel.clone(),
    ));
    let probe = tokio::time::timeout(Duration::from_secs(20), async {
        let identity = client.query(ContactQuery::LoginInfo).await;
        let friends = client.query(ContactQuery::FriendList).await;
        let groups = client.query(ContactQuery::GroupList).await;
        (identity, friends, groups)
    })
    .await;
    cancel.cancel();
    let _ = connector.await;
    let (identity, friends, groups) = match probe {
        Ok(results) => results,
        Err(_) => panic!("probe deadline: no exchange completed against {url}"),
    };
    for (label, result) in [
        ("get_login_info", &identity),
        ("get_friend_list", &friends),
        ("get_group_list", &groups),
    ] {
        match result {
            Ok(receipt) => {
                eprintln!(
                    "{label}: matched echo={} self_id={:?} retcode={} configured_account={}",
                    receipt.echo,
                    receipt.observed_self_id(),
                    receipt.receipt().retcode,
                    receipt.configured_account,
                );
                if account != 0 {
                    assert_eq!(
                        receipt.observed_self_id().unwrap_or(account),
                        account,
                        "{label}: response bound to a different account — not this bot's data"
                    );
                }
            }
            Err(SendError::Disconnected) => {
                eprintln!("{label}: connector unavailable — typed, not empty");
            }
            Err(SendError::Unknown(detail)) => {
                eprintln!("{label}: outcome unknown ({detail}) — typed, not empty");
            }
        }
    }
}
