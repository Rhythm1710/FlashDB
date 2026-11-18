# Public playground: one process serves the static page and gives every
# browser WebSocket its own in-memory FlashDB. Railway sets $PORT.
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN cargo build --release --locked --example ws_bridge

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --create-home --shell /usr/sbin/nologin flashdb \
    && mkdir -p /tmp/flashdb \
    && chown flashdb:flashdb /tmp/flashdb
COPY --from=build /src/target/release/examples/ws_bridge /usr/local/bin/ws_bridge
COPY playground /opt/flashdb/playground
USER flashdb
ENV FLASHDB_DATA_DIR=/tmp/flashdb
EXPOSE 8080
CMD ["ws_bridge", "--public", "--dir", "/opt/flashdb/playground"]
