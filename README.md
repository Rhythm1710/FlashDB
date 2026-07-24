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

## Protocol notes

The RESP parser is incremental: a frame split across multiple TCP segments
is reassembled, and a pipelined batch of commands sent in one segment is
answered in order, one reply per command. Malformed input gets a
Redis-style `-ERR Protocol error` reply before the connection is closed.

## Development

```sh
cargo test              # run the test suite
cargo clippy -- -D warnings
cargo fmt
```

## License

MIT — see [LICENSE](LICENSE).
