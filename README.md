# FlashDB

A Redis-compatible, in-memory database written from scratch in Rust.

FlashDB speaks the [RESP protocol](https://redis.io/docs/latest/develop/reference/protocol-spec/),
so you can talk to it with any Redis client (`redis-cli`, language bindings, `redis-benchmark`).

## Build & run

FlashDB has a small dependency set (async runtime + buffers). Run it directly:

```sh
cargo run --release
```

This starts the server on `127.0.0.1:6379`. Connect with any Redis client:

```sh
redis-cli -p 6379 ping
# PONG
```

Or build a release binary and run it:

```sh
cargo build --release
./target/release/flashdb
```

## Implemented commands

- `PING`
- `ECHO`
- `SET`
- `GET`
- `DEL`

## Development

```sh
cargo test              # run the test suite
cargo clippy -- -D warnings
cargo fmt
```

## License

MIT — see [LICENSE](LICENSE).
