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
- `TYPE key`
- `RPUSH key value [value ...]`
- `LPUSH key value [value ...]`
- `RPOP key`
- `LPOP key`
- `LLEN key`
- `LRANGE key start stop`
- `HSET key field value [field value ...]`
- `HGET key field`
- `HGETALL key`
- `HDEL key field [field ...]`

## Value types

Every key holds a typed value — a `string`, a `list`, or a `hash` today, with
sets to follow. Values are modelled as an enum, so a command that meets the
wrong type (say `GET` on a list, or `LPUSH` on a string) replies with a
`WRONGTYPE` error instead of misbehaving. `TYPE key` reports the kind of value
stored: `string`, `list`, `hash`, or `none` if the key is missing or has
expired.

## Lists

A list is an ordered sequence of strings you can grow and shrink from either
end. `RPUSH` appends to the tail and `LPUSH` prepends to the head; both create
the list on first use, accept several values at once, and return the list's new
length. `RPOP` and `LPOP` remove and return one element from the tail or head
(a null reply if the key is missing or empty) — and when a pop empties the
list, the key is deleted, so an empty list never lingers.

`LLEN key` returns the length (`0` for a missing key), and `LRANGE key start
stop` returns the elements between `start` and `stop` inclusive. Indices are
zero-based and may be negative to count back from the end, so `LRANGE mylist 0
-1` returns the whole list; an inverted or out-of-range span yields an empty
array rather than an error.

## Hashes

A hash maps string fields to string values under a single key — handy for
representing an object without a key per attribute. `HSET key field value
[field value ...]` sets one or more pairs (creating the hash on first use) and
returns how many fields were *newly added*, so overwriting an existing field
counts as zero. `HGET key field` returns a single field's value (a null reply
if the field or key is missing), and `HGETALL key` returns every field and
value flattened into one array. `HDEL key field [field ...]` removes fields and
returns how many were actually present; when the last field is removed the key
is deleted, so an empty hash never lingers.

Fields are stored in a `HashMap`, so `HGETALL` returns pairs in an unspecified
order — sort client-side if you need a stable order, exactly as you would with
Redis.

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
