//! Rescan feasibility spike (2026-09-04): can a **bot** session iterate
//! the chat history via `messages.getHistory` (`Client::iter_messages`)?
//!
//! Run against a COPY of the production session (never the original) from
//! any directory:
//!
//! ```text
//! CYDRIVE_SPIKE_SESSION=<abs session copy> CYDRIVE_BOT_TOKEN=... \
//! CYDRIVE_CHAT_ID=... CYDRIVE_PROXY_URL=socks5://127.0.0.1:7897 \
//! cargo run -p ck-telegram --example history_spike --release
//! ```
//!
//! Exit 0 + printed messages = history is readable (rescan viable);
//! a printed RPC error name (e.g. `USER_IS_BOT`) = the gate is closed.

use std::sync::Arc;

use grammers_client::sender::{ConnectionParams, SenderPool};
use grammers_client::Client;
use grammers_session::storages::SqliteSession;
use grammers_session::types::PeerId;
use grammers_session::Session;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let session_path = std::path::PathBuf::from(env("CYDRIVE_SPIKE_SESSION"));
    let bot_token = env("CYDRIVE_BOT_TOKEN");
    let chat_id: i64 = env("CYDRIVE_CHAT_ID").parse().expect("numeric chat id");
    let proxy_url = std::env::var("CYDRIVE_PROXY_URL").ok();

    let session = Arc::new(
        SqliteSession::open(&session_path)
            .await
            .expect("session copy opens"),
    );
    let pool = SenderPool::with_configuration(
        Arc::clone(&session),
        6,
        ConnectionParams {
            device_model: "cydrive-spike".to_string(),
            system_version: std::env::consts::OS.to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            system_lang_code: "en".to_string(),
            lang_code: "en".to_string(),
            proxy_url,
            use_ipv6: false,
            __non_exhaustive: (),
        },
    );
    let client = Client::new(pool.handle);
    tokio::spawn(pool.runner.run());

    if !client.is_authorized().await.expect("auth check") {
        client
            .bot_sign_in(&bot_token, "eb06d4abfb49dc3eeb1aeb98ae0f581e")
            .await
            .expect("bot sign in");
    }

    let peer_id = PeerId::from_bot_api_dialog_id(chat_id).expect("valid dialog id");
    let chat = session
        .peer_ref(peer_id)
        .await
        .expect("peer lookup")
        .unwrap_or_else(|| peer_id.to_ambient_ref());

    println!("spike: calling iter_messages (messages.getHistory) as a bot ...");
    let mut iter = client.iter_messages(chat).limit(5);
    let mut seen = 0;
    loop {
        match iter.next().await {
            Ok(Some(message)) => {
                seen += 1;
                let caption = message.text().trim();
                let preview: String = caption.chars().take(60).collect();
                println!(
                    "  msg {} document={:?} caption={:?}",
                    message.id(),
                    message.media().is_some(),
                    preview
                );
                if seen == 5 {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                println!("RESULT: REJECTED (iter_messages/getHistory) — rpc error: {error}");
                // Second probe: maybe messages.search (search_messages) is
                // treated differently? Close that loophole too.
                println!("spike: calling search_messages (messages.search) as a bot ...");
                match client.search_messages(chat).limit(5).next().await {
                    Ok(Some(message)) => {
                        println!("RESULT: SEARCH READABLE — msg {}", message.id());
                        std::process::exit(0);
                    }
                    Ok(None) => {
                        println!("RESULT: SEARCH EMPTY (no error) — method allowed");
                        std::process::exit(0);
                    }
                    Err(error) => {
                        println!("RESULT: SEARCH ALSO REJECTED — rpc error: {error}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
    println!("RESULT: READABLE — {seen} history message(s) iterated as a bot");
    std::process::exit(if seen > 0 { 0 } else { 2 });
}
