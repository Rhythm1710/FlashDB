# FlashDB

[![CI](https://github.com/Rhythm1710/FlashDB/actions/workflows/ci.yml/badge.svg)](https://github.com/Rhythm1710/FlashDB/actions/workflows/ci.yml)

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
- `SET key value [EX seconds | PX milliseconds]`
- `GET`
- `DEL`
- `EXPIRE key seconds`
- `TTL key`
- `PERSIST key`

## Key expiry

Keys can be given a time to live. `SET` takes an optional `EX <seconds>` or
`PX <milliseconds>` to set a lifetime up front, and `EXPIRE key seconds` sets
one on an existing key. `TTL key` returns the seconds remaining (`-1` if the
key has no expiry, `-2` if it doesn't exist), and `PERSIST key` removes an
expiry so the key lives forever again.

Expiry is *passive*: a key past its deadline stays in memory until something
touches it, at which point the read drops it and reports it as missing — so an
expired key is indistinguishable from one that was never set.

## Protocol notes

The RESP parser is incremental: a frame split across multiple TCP segments
is reassembled, and a pipelined batch of commands sent in one segment is
answered in order, one reply per command. Malformed input gets a
Redis-style `-ERR Protocol error` reply before the connection is closed.

## Development

The server logic lives in a library (`src/lib.rs`); `src/main.rs` is a thin
binary that binds the listener and calls `flashdb::run`. Splitting it this way
lets the integration tests start a real server on an ephemeral port and talk to
it over a genuine TCP socket.

```sh
cargo test              # parser unit tests + end-to-end integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Tests come in two layers: unit tests in `src/resp.rs` that prove single RESP
frames parse and serialize correctly (including partial and malformed input),
and integration tests in `tests/integration.rs` that drive a live server over
TCP and assert on the raw bytes it sends back.

## Continuous integration

Every push and pull request against `master` runs the same gate through
[GitHub Actions](.github/workflows/ci.yml): `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, and `cargo test`. A red build
means one of those failed.

## License

MIT — see [LICENSE](LICENSE).
