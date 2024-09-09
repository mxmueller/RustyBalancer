use std::sync::Arc;
use tokio::sync::RwLock;
use log::info;

mod socket;
mod http;
mod queue;
mod client;
mod cache;

use crate::http::start_http_server;
use crate::socket::connect_socket;
use crate::client::UnboundedClient;
use crate::cache::SimpleCache;

#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() {
    // Initialize logging
    env_logger::init();

    info!("Starting load balancer");

    let shared_state = Arc::new(RwLock::new(None));

    // Create UnboundedClient
    let shared_client = UnboundedClient::new();

    let cache = Arc::new(SimpleCache::new(10000));

    let ws_state = shared_state.clone();
    tokio::spawn(async move {
        if let Err(e) = connect_socket(ws_state).await {
            log::error!("WebSocket connection error: {}", e);
        }
    });

    let http_state = shared_state.clone();
    let http_client = shared_client.clone();
    let http_cache = cache.clone();
    if let Err(e) = start_http_server(http_state, http_client, http_cache).await {
        log::error!("HTTP server error: {}", e);
    }
}