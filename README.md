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
below). `--replicaof <host> <port>` starts the server as a replica that syncs
from a master on boot (see [Replication](#replication) below).
`--requirepass <password>` demands `AUTH` before any other command (see
[Authentication](#authentication)). An unknown flag or a flag missing its
value prints a message and exits non-zero rather than starting.

## Authentication

`--requirepass <password>` makes the server refuse every command until that
connection sends `AUTH <password>` or `AUTH default <password>`. Any other
username is `WRONGPASS`. Until then the reply is
`-NOAUTH Authentication required.` `QUIT` is the exception: it closes the
connection without logging in. `AUTH` with the wrong password replies
`-WRONGPASS invalid username-password pair or user is disabled.` and leaves
the connection unauthenticated, including a connection that had already
logged in. The password compare does not stop at the first differing byte.
Authentication is per connection: one client logging in does not
unlock any other. With no `--requirepass`, `AUTH` replies with an error and
every other command runs as usual.

The built-in replica handshake does not send `AUTH`, so a master started with
`--requirepass` rejects a `--replicaof` replica until a master password is
wired up.

## Implemented commands

- `PING`
- `AUTH [username] password`
- `QUIT`
- `ECHO`
- `SET key value [EX seconds | PX milliseconds]`
- `GET`
- `DEL`
- `EXPIRE key seconds`
- `TTL key`
- `DBSIZE`
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
- `WAIT numreplicas timeout`
- `MULTI`
- `EXEC`
- `DISCARD`
- `WATCH key [key ...]`
- `UNWATCH`
- `SUBSCRIBE channel [channel ...]`
- `UNSUBSCRIBE [channel ...]`
- `PUBLISH channel message`
- `XADD key [MAXLEN [=|~] threshold] <id> field value [field value ...]`
- `XLEN key`
- `XRANGE key start end [COUNT count]`
- `XREVRANGE key end start [COUNT count]`
- `XDEL key id [id ...]`
- `XTRIM key MAXLEN [=|~] threshold`
- `XREAD [COUNT count] [BLOCK ms] STREAMS key [key ...] id [id ...]`
- `ZADD key score member [score member ...]`
- `ZSCORE key member`
- `ZRANK key member`
- `ZRANGE key start stop [WITHSCORES]`
- `ZCARD key`
- `ZREM key member [member ...]`
- `ZINCRBY key increment member`
- `ZRANGEBYSCORE key min max [WITHSCORES]`
- `ZCOUNT key min max`

## Transactions

`MULTI` opens a transaction on the connection. Every command sent after it is
**queued** rather than run — each is answered with `+QUEUED` — until `EXEC`
replays the whole batch in order and returns one array holding each command's
reply. `DISCARD` throws the queued batch away without running it.

Queuing is validated up front: an unknown command is rejected the moment it is
queued *and* taints the transaction, so the eventual `EXEC` aborts the whole
batch with `EXECABORT` and runs nothing. A command that only fails at run time
(for example a `WRONGTYPE` error) is not caught early — it runs, and its error
is simply one element of the `EXEC` reply array; the commands around it still
run, because a Redis transaction is a batch, not a rollback.

`WATCH key [key ...]` adds optimistic locking (check-and-set). It records the
current version of each named key; if any watched key is written by *any*
connection before `EXEC`, the transaction aborts — `EXEC` replies with the nil
array (`*-1`) and runs nothing — so the client can retry. Watching a key that
does not exist yet still guards it: if it springs into existence before `EXEC`,
that counts as a change. `UNWATCH` clears the watch set, and a completed `EXEC`
or `DISCARD` clears it automatically. `WATCH` is not allowed once `MULTI` has
opened a transaction.

Transaction state (the queue and the watch set) lives per-connection, not in
the shared keyspace, so two clients can run independent transactions at once.
Writes inside a transaction replicate to replicas exactly as they would outside
one.

## Pub/Sub

FlashDB supports Redis's publish/subscribe messaging. `SUBSCRIBE channel
[channel ...]` puts a connection into **subscribe mode** and registers it as a
listener on each channel; the server confirms each with a `[subscribe, channel,
count]` frame, where `count` is how many channels the connection now holds.
`PUBLISH channel message`, sent from any other connection, delivers `[message,
channel, payload]` to every subscriber of that channel and returns the number
that received it. `UNSUBSCRIBE [channel ...]` leaves the named channels, or
every channel when given none; once the last subscription is dropped the
connection returns to ordinary request/response mode.

While subscribed, a connection is half push, half request/response: the server
may send it a message at any moment, and it may still `SUBSCRIBE`,
`UNSUBSCRIBE`, or `PING`. Any other command is refused with an error, matching
Redis's RESP2 rule that a subscribed client can only manage its subscriptions.
The channel routing table lives on the shared server (channel → the set of
subscribed connections), while each connection tracks its own channel set on its
task — the same shared-registry-plus-per-connection-state split that replication
and transactions use.

Not yet implemented: pattern subscriptions (`PSUBSCRIBE`/`PUNSUBSCRIBE`),
`PUBSUB` introspection, and propagating `PUBLISH` across a replication link.

## Streams

A **stream** is an append-only log of entries. Each entry has a unique,
monotonically increasing ID of the form `<ms>-<seq>` (a millisecond timestamp
and a per-millisecond sequence counter) and a set of field/value pairs.

`XADD key [MAXLEN [=|~] threshold] <id> field value [field value ...]` appends
an entry and returns the ID it was stored under. The `<id>` may be:

- `*` — both parts auto-generated (the millisecond comes from the clock, or the
  last entry's millisecond if the clock hasn't advanced, and the sequence is the
  next free one);
- `<ms>-*` or a bare `<ms>` — the millisecond is fixed and the sequence is
  auto-generated;
- `<ms>-<seq>` — a fully explicit ID.

The resolved ID must be strictly greater than the stream's current top ID (and
greater than `0-0`), otherwise `XADD` errors and the stream is left untouched.

An optional leading `MAXLEN [=|~] threshold` clause trims the stream down to
`threshold` entries — using the same trim primitive as the standalone `XTRIM`
below — *after* the new entry is appended, so the just-added entry is never
the one trimmed away. As with `XTRIM`, the `=`/`~` exactness marker is parsed
but doesn't change FlashDB's behavior.

`XLEN key` returns the number of entries. `XRANGE key start end [COUNT count]`
returns the entries whose IDs fall in the inclusive range `[start, end]`; the
bounds accept `-` and `+` (the smallest and largest possible IDs), a bare
`<ms>` (which covers the whole millisecond), or a full `<ms>-<seq>`, and
`COUNT` caps how many entries come back. Each entry is returned as
`[id, [field, value, ...]]`. A bound may be made **exclusive** by prefixing it
with `(` — for example `XRANGE key (1-0 (3-0` returns everything strictly
between `1-0` and `3-0`, dropping both endpoints.

`XREVRANGE key end start [COUNT count]` returns the same window as `XRANGE` but
in reverse, highest ID first. Note the bound order is flipped — `end` is given
before `start` — matching Redis; `COUNT` keeps the highest IDs.

`XDEL key id [id ...]` removes the named entries and returns the number actually
deleted (IDs that weren't present don't count). Deleting entries never lowers the
stream's high-water mark, so a future `XADD` can't reuse the ID of a deleted
entry even if the whole stream is emptied.

`XTRIM key MAXLEN [=|~] threshold` drops the oldest entries until at most
`threshold` remain, returning the number actually removed. The optional `=`/`~`
exactness marker Redis accepts before the threshold is parsed but doesn't change
FlashDB's behavior — trimming is always exact. Like `XDEL`, trimming never
lowers the stream's high-water mark.

`XREAD [COUNT count] [BLOCK ms] STREAMS key [key ...] id [id ...]` reads entries
newer than a given ID from one or more streams at once: it returns, for each
stream that has anything, `[key, [entries...]]` with the entries whose IDs are
strictly greater than the paired `id`. An `id` of `$` means "only entries newer
than the current end". Without `BLOCK`, the read is non-blocking — if no stream
has new entries the reply is the nil array (so `XREAD` on `$` returns nothing).

With `BLOCK ms` the read instead *waits* for new entries: it resolves each `$`
against the stream's current end at the moment blocking begins, and then parks
until a matching `XADD` arrives (on any connection) or the timeout elapses.
`BLOCK 0` waits indefinitely. If the timeout passes with nothing new the reply
is the nil array. The wait is genuinely async — a blocked client uses no CPU and
never holds the store lock while waiting, and a single `XADD` wakes every client
blocked on that stream. Inside a `MULTI`/`EXEC` transaction a `BLOCK` is ignored
(the queued read runs as a non-blocking pass), matching Redis.

`SAVE` / `BGSAVE` persist a stream: the high-water `last_id`, then every
surviving entry's id and field/value pairs. The on-disk record is FlashDB's
own (value type `0xE0`) rather than Redis's listpack/radix-tree encoding,
which FlashDB still does not read or write. A snapshot that contains a stream
reloads here — including a high-water mark left behind by `XDEL` / `XTRIM`,
so a later `XADD` cannot reuse a deleted id — while a real `redis-server`
rejects that file. A snapshot with no stream keys stays loadable by Redis,
same as before. The consumer-group commands (`XACK`/`XCLAIM`/…) are still to
come.

## Sorted sets

A **sorted set** holds unique string members, each tagged with a
floating-point score, kept in ascending order by score (ties broken by
comparing the members themselves).

`ZADD key score member [score member ...]` sets one or more members' scores,
creating the set on first use, and returns how many members were *newly
added* — same counting rule as `HSET`: re-adding an existing member just
updates its score and doesn't count. Scores are parsed as doubles (`inf` and
`-inf` are accepted; `nan` is rejected, since a score has to compare against
every other score to have a place in the order).

`ZSCORE key member` returns a member's current score as a bulk string (a
whole-number score prints without a trailing `.0`), or null if the member or
the key is missing. `ZRANK key member` returns the member's 0-based rank in
ascending order, or null the same way.

`ZRANGE key start stop [WITHSCORES]` returns the members whose ranks fall in
the inclusive range `[start, stop]`. Indices are zero-based and may be
negative to count back from the end — `ZRANGE key 0 -1` returns every member
— with the same clamping rules as `LRANGE`: an out-of-range bound clamps to
the nearest valid one, and an inverted span (after clamping) yields an empty
array. Adding `WITHSCORES` interleaves each member with its formatted score.

`ZCARD key` returns how many members the set holds, or `0` for a missing key.
`ZREM key member [member ...]` removes the named members and returns how many
were actually present; like `HDEL`, an emptied set is removed from the
keyspace rather than left behind empty. `ZINCRBY key increment member` adds
`increment` to a member's score — treating an absent member as starting at
`0`, and creating the set on first use — and returns the new score; a bad
increment fails the same way a bad `ZADD` score does, and the rare case where
the arithmetic itself produces `NaN` (`inf` plus `-inf`) is rejected without
touching the member's existing score.

`ZRANGEBYSCORE key min max [WITHSCORES]` returns the members whose *scores*
(not ranks) fall in `[min, max]`, ascending. `min`/`max` accept `-inf`/`+inf`
for an open-ended scan, and either bound can be made exclusive with a leading
`(` (e.g. `ZRANGEBYSCORE key (1 3` excludes a member scored exactly `1`) —
the same `(`-prefix convention `XRANGE`/`XREVRANGE` use for stream IDs.
`ZCOUNT key min max` uses the identical bound grammar and just returns how
many members qualify, without building the member list.

Members are kept in two structures under the hood — a `HashMap` for O(1)
`ZSCORE`/`ZCARD` lookups, and a `Vec` sorted by `(score, member)` that
`ZRANK`/`ZRANGE` binary-search with `partition_point` and `ZREM` uses to find
the entry to remove, the same trick streams use for `XRANGE`.
`ZRANGEBYSCORE`/`ZCOUNT` share one more binary search on that same `Vec` —
`partition_point` on the score alone finds the boundary of a whole run of
tied scores in one pass, since ties are already grouped together by the
existing sort order. Sorted sets are persisted as Redis's `ZSET_2` record
(a count, then each member as a string and its score as a little-endian
binary double). FlashDB also reads the older `ZSET` encoding, where the score
is decimal text with the `253` / `254` / `255` sentinels for NaN and the
infinities. A snapshot FlashDB writes therefore loads back into FlashDB and
into an unmodified `redis-server`. A reverse `ZRANGE ... REV` is still to
come.

## Value types

Every key holds a typed value — a `string`, a `list`, a `hash`, a `stream`, or
a `zset` today, with a plain unordered `set` still to follow. Values are
modelled as an enum, so a command that meets the wrong type (say `GET` on a
list, or `LPUSH` on a string) replies with a `WRONGTYPE` error instead of
misbehaving. `TYPE key` reports the kind of value stored: `string`, `list`,
`hash`, `stream`, `zset`, or `none` if the key is missing or has expired.

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

Expiry is checked two ways. A read of a key past its deadline drops it and
reports it missing (passive expiry), so an expired key is indistinguishable
from one that was never set. A background sweep also samples keys that carry
a TTL — 20 at a time, and again while more than a quarter of the sample is
already expired, the same rule Redis uses — and deletes the ones it finds
due, so a key nobody reads still leaves memory. The sweep keeps a side
index of keys that have a TTL and samples that index (20 keys a pass) instead
of scanning the whole keyspace. It drops the store lock between passes and
runs about every 100ms. The task is aborted when the accept loop ends, which
is the only way this server stops. `DBSIZE` reports how many keys are stored,
including expired ones the sweep has not removed yet; calling it does not
expire anything itself.

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

Strings, lists, hashes, and sorted sets are serialized in the encodings Redis
itself uses (sorted sets as `ZSET_2`), each with its optional expiry. Streams
are serialized too, in the FlashDB record described under [Streams](#streams):
a snapshot that includes one reloads here, and is rejected by `redis-server`.
Because the on-disk bytes are real RDB — right down to the CRC64 trailer Redis
verifies on load — a snapshot of strings, lists, hashes, and sorted sets loads
straight back into FlashDB *and* into an unmodified `redis-server`:

```sh
redis-cli -p 6379 SET greeting "hello world"
redis-cli -p 6379 RPUSH mylist a b c
redis-cli -p 6379 SAVE                 # FlashDB writes dump.rdb
# ...point a real redis-server at that directory...
redis-cli -p 6379 LRANGE mylist 0 -1   # 1) "a" 2) "b" 3) "c" — loaded by Redis
```

## Replication

FlashDB speaks **both sides** of the replication link: it can start as a replica
of another server, and it can act as a master that feeds replicas.

Start a replica pointed at a master:

```sh
flashdb -p 6380 --replicaof 127.0.0.1 6379
```

On startup the replica dials the master and performs the Redis replication
handshake — `PING`, two `REPLCONF` rounds (announcing its own listening port and
its capabilities), then `PSYNC ? -1`. The master answers with a
`+FULLRESYNC <replid> <offset>` line followed by a full RDB snapshot of its
keyspace as a bulk payload. The replica frames that snapshot off the wire and
loads it through the same RDB reader used for on-disk startup loading, so strings,
lists, hashes, and expiry all transfer. The master can be another FlashDB
instance or a real `redis-server`.

After the snapshot the link stays open. The **master streams every write it
applies** (`SET`, `DEL`, `EXPIRE`, the list and hash mutations, …) down the
connection as ordinary RESP commands, and the **replica replays each one**
against its own keyspace, so a change on the master shows up on the replica a
moment later. Reads and rejected commands are never streamed. `WAIT numreplicas
timeout` reports how many replicas are currently connected. The whole sync runs
on a background task, so both servers keep serving their own clients throughout;
if the master is unreachable the replica logs it and serves whatever it had.

Not yet implemented: per-replica acknowledged offsets (so `WAIT` reports
connection count rather than blocking until writes are acked), replica reconnect
with backoff, a runtime `REPLICAOF` command, and partial resync.

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

`lib.rs` grew past four thousand lines as commands piled up in one file, so it's
being split into `src/commands/`, one module per self-contained command group.
`src/commands/persistence.rs` (`SAVE`/`BGSAVE`/`WAIT` and the RDB snapshot
helpers) was the first slice out; `src/commands/lists.rs`
(`RPUSH`/`LPUSH`/`RPOP`/`LPOP`/`LLEN`/`LRANGE`) and `src/commands/hashes.rs`
(`HSET`/`HGET`/`HGETALL`/`HDEL`) followed, then `src/commands/streams.rs` —
the biggest group yet — took `XADD`/`XLEN`/`XRANGE`/`XREVRANGE`/`XDEL`/`XTRIM`/
`XREAD` plus the async blocking `XREAD ... BLOCK` path with it, and
`src/commands/pubsub.rs` took `SUBSCRIBE`/`UNSUBSCRIBE`/`PUBLISH`, the
subscriber routing table, and the subscribe-mode connection loop
(`serve_subscriber`) with it, and `src/commands/transactions.rs` took
`MULTI`/`EXEC`/`DISCARD`/`WATCH`/`UNWATCH` and the per-connection `Session`
they dispatch through — the first group whose state belongs to one
connection rather than a shared, crate-wide registry. `lib.rs` is now under
1,400 lines, down from a peak past 4,300 — mostly the connection loop and the
`process_command` dispatch table. `src/commands/sorted_sets.rs`
(`ZADD`/`ZSCORE`/`ZRANK`/`ZRANGE`) is the first new command group written
directly in `src/commands/` rather than lifted out of `lib.rs`, following the
same pattern the split established.

```sh
cargo test              # parser unit tests + end-to-end integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Tests come in two layers: unit tests in `src/resp.rs` that prove single RESP
frames parse and serialize correctly (including partial and malformed input),
and integration tests in `tests/integration.rs` that drive a live server over
TCP and assert on the raw bytes it sends back.

## Web playground

`playground/` is a small static page for trying commands from a phone or a
desktop browser. The page and its bridge live outside the server binary:
`cargo build` produces `flashdb`, which speaks RESP only, and the playground
is the separate `ws_bridge` example. That example serves the static files and
accepts a browser WebSocket on `/ws`. Each text frame is one command line
(whitespace-separated, with double quotes for values that contain spaces); the
bridge writes it to a FlashDB socket as RESP and sends the reply back as text.

```sh
cargo run                       # terminal 1: FlashDB on 127.0.0.1:6379
cargo run --example ws_bridge   # terminal 2: page + bridge on 127.0.0.1:8080
```

Open `http://127.0.0.1:8080`. The page has four tabs: Home, a command
reference, a live Instance view that polls `INFO` and `DBSIZE`, and the
Console. It sets a mobile viewport, uses large type-and-tap controls so a
phone keyboard does not zoom the field, and keeps a touch-friendly command
history: tap an earlier command to put it back in the box. The composer and
tab bar follow `visualViewport`, so the iOS keyboard does not cover them. A
service worker caches the static files, so the shell still opens after the
first visit. The bridge listens on `127.0.0.1` unless you pass `--listen`,
and dials the database given by `--flashdb` (default `127.0.0.1:6379`).
`--dir` points at the static files when they are somewhere other than the
`playground/` directory next to the crate.

```sh
cargo run --example ws_bridge -- --listen 127.0.0.1:8080 --flashdb 127.0.0.1:6379
```

The bridge is a development proxy. Leave it on localhost unless you trust the
network it is bound to. To open the playground from a phone on the same LAN,
bind every interface and browse to this machine's LAN address:

```sh
cargo run --example ws_bridge -- --listen 0.0.0.0:8080
```

Then on the phone open `http://<host-LAN-IP>:8080`. `/ws` accepts the page's
own origin by default. Pass `--allow-origin https://example` (repeatable) to
allow another browser origin. Subscribe-mode commands (`SUBSCRIBE`) are a poor
fit for the one-reply-per-command bridge; ordinary request/response commands are
what the page is for.

A public demo is a different mode. `ws_bridge --public` gives each browser its
own database, and the static page can be hosted separately. See [DEPLOY.md](DEPLOY.md).

## Continuous integration

Every push and pull request against `master` runs the same gate through
[GitHub Actions](.github/workflows/ci.yml): `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, and `cargo test`. A red build
means one of those failed.

## License

MIT — see [LICENSE](LICENSE).
