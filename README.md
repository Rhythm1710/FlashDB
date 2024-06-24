# FlashDB

A Redis clone written from scratch in Rust.

## Build

FlashDB doesn't have any external dependencies.
You can either run it directly:

```
cargo run --release
```

Or you can build it and use -p to specify the port and -t to specify a conenction timeout in milliseconds.

```
cargo build -- release
./target/debug/sider -p 3000 -t 10
```

## Implemented commands (so far):

- SET
- GET
- DEL
- ECHO
