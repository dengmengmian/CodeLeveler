# Self-hosted CodeLeveler relay.
#
# Build from the repository root so the workspace manifest and every path
# dependency are in context:
#
#   docker build -f services/leveler-relay/Dockerfile -t leveler-relay .
#
# The relay speaks plain HTTP; terminate TLS in front of it. Until Phase 2's
# AEAD lands, whatever terminates TLS can read session traffic, so the design
# treats self-hosting as the privacy-default path — that machine should be
# yours.

FROM rust:1.90-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p leveler-relay

FROM debian:bookworm-slim
# The relay stores nothing on disk (no queue, no transcript), so it needs no
# volume and runs unprivileged.
RUN useradd --system --create-home --uid 10001 relay
USER relay
COPY --from=build /src/target/release/leveler-relay /usr/local/bin/leveler-relay
ENV LEVELER_RELAY_BIND=0.0.0.0:8443
EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/leveler-relay"]
