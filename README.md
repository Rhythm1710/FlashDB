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

### Configuration

The server accepts a few Redis-style startup flags:

```sh
flashdb -p 7000 --dir /var/lib/flashdb --dbfilename snapshot.rdb
```

`-p` (or `--port`) sets the listening port (default `6379`). `--dir` and
`--dbfilename` name where the on-disk snapshot lives; they default to the
current directory and `dump.rdb`; they locate the on-disk snapshot FlashDB loads
at startup and writes on `SAVE` / `BGSAVE` (see [Persistence](#persistence-rdb)
below). An unknown flag or a flag missing its value prints a message and exits
non-zero rather than starting.

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
- `SAVE`
- `BGSAVE`

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

## Persistence (RDB)

FlashDB persists to disk in Redis's own binary RDB format, so snapshots move
freely in both directions between FlashDB and a real `redis-server`.

### Loading

On startup FlashDB looks for a Redis RDB snapshot at `<dir>/<dbfilename>` and,
if one is present, loads its keys into memory before accepting any clients — so
a restart recovers whatever a previous run (or a real Redis server) persisted. A
missing snapshot is a clean first boot with an empty keyspace; a snapshot that
can't be parsed aborts startup rather than silently discarding the data.

Redis's compact integer string encoding is understood on load (a numeric string
is stored as an integer and rendered back to text). Each key's expiry metadata
is honoured: a still-future deadline is restored as a live TTL, and a key whose
deadline has already passed is dropped on load, just as Redis does.

### Saving

`SAVE` writes the whole keyspace to `<dir>/<dbfilename>` synchronously and
replies `+OK` once the file is on disk. `BGSAVE` takes a consistent snapshot and
hands the file write to a background thread, replying `+Background saving
started` immediately so the client isn't blocked on the disk (real Redis forks a
child process for the same effect). Both write the file atomically — to a temp
file, then a rename — so a crash mid-write never leaves a half-written snapshot.

Strings, lists, and hashes are all serialized, each with its optional expiry.
Because the on-disk bytes are real RDB — right down to the CRC64 trailer Redis
verifies on load — a snapshot FlashDB writes loads straight back into FlashDB
*and* into an unmodified `redis-server`:

```sh
redis-cli -p 6379 SET greeting "hello world"
redis-cli -p 6379 RPUSH mylist a b c
redis-cli -p 6379 SAVE                 # FlashDB writes dump.rdb
# ...point a real redis-server at that directory...
redis-cli -p 6379 LRANGE mylist 0 -1   # 1) "a" 2) "b" 3) "c" — loaded by Redis
```

## Protocol notes

The RESP parser is incremental: a frame split across multiple TCP segments
is reassembled, and a pipelined batch of commands sent in one segment is
answered in order, one reply per command. Malformed input gets a
Redis-style `-ERR Protocol error` reply before the connection is closed.

## Development

The server logic lives in a library (`src/lib.rs`); `src/main.rs` is a thin
binary that binds the listener and calls `flashdb::run_with_config` (which loads
any RDB snapshot first). The RDB decoder lives in `src/rdb.rs`. Splitting it
this way lets the integration tests start a real server on an ephemeral port and
talk to it over a genuine TCP socket.

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
