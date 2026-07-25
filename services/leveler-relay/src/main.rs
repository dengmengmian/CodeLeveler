//! Runs the relay.
//!
//! Binds plain HTTP. TLS is expected to be terminated in front of this process
//! by a reverse proxy — which is also where the operator's certificate already
//! lives. Note that until Phase 2's AEAD lands, whatever terminates TLS can
//! read session traffic in the clear; that is why the design makes self-hosting
//! the privacy-default path, so the machine that can read it is the user's own.

use leveler_relay::{RelayState, build_router};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let address =
        std::env::var("LEVELER_RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_string());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!(%address, "leveler-relay listening");

    axum::serve(listener, build_router(RelayState::new())).await?;
    Ok(())
}
