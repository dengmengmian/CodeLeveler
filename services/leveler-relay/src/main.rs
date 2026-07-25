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

    // Required, with no default: the secret is what decides which machines may
    // become hosts on this relay. A generated or empty fallback would turn a
    // missing configuration into an open enrollment endpoint, which is precisely
    // the failure this refuses to have.
    let enrollment_secret = std::env::var("LEVELER_RELAY_ENROLLMENT_SECRET").map_err(|_| {
        "LEVELER_RELAY_ENROLLMENT_SECRET is required: it is the secret a developer machine \
         presents to enroll. Generate one with `openssl rand -base64 32`."
    })?;
    if enrollment_secret.len() < 16 {
        return Err("LEVELER_RELAY_ENROLLMENT_SECRET must be at least 16 characters".into());
    }

    let address =
        std::env::var("LEVELER_RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8443".to_string());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!(%address, "leveler-relay listening");

    let state = RelayState::with_enrollment_secret(&enrollment_secret);
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}
