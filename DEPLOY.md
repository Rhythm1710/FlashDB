# Deploy the public playground

The page is a static site. The database is not shared: `ws_bridge --public` gives every browser WebSocket its own FlashDB and deletes that session when the socket closes.

```text
browser  ->  https://flashdb.rhythmarora.dev   (Vercel, files in playground/)
         ->  wss://<railway-host>/ws           (this container)
```

## Railway

The container listens on `$PORT` and answers `GET /healthz`. Set the Vercel origin before the process will accept a WebSocket.

| Variable | Required | Meaning |
| --- | --- | --- |
| `PORT` | set by Railway | Bind `0.0.0.0:$PORT`. |
| `FLASHDB_ALLOW_ORIGIN` | yes | Comma-separated browser origins allowed to open `/ws`. Use `https://flashdb.rhythmarora.dev`. A missing or other `Origin` is rejected. |
| `FLASHDB_PUBLIC` | no | `1` turns on public mode. The image already passes `--public`. |
| `FLASHDB_DATA_DIR` | no | Ephemeral directory for per-session files. Default `/tmp/flashdb`. Also `--data-dir`, or `FLASHDB_DIR` / `DATA_DIR`. |
| `FLASHDB_MAX_SESSIONS` | no | Concurrent sessions. Default `32`. |
| `FLASHDB_MAX_MEMORY` | no | Payload bytes per session. Default `1048576`. |
| `FLASHDB_MAX_KEY_BYTES` | no | Default `1024`. |
| `FLASHDB_MAX_VALUE_BYTES` | no | Default `8192`. |
| `FLASHDB_MAX_COMMANDS_PER_SEC` | no | Default `30`. |
| `FLASHDB_IDLE_SECS` | no | Close a quiet session. Default `120`. |

`--dir` is the static files directory, not the data directory. The image sets it to `/opt/flashdb/playground`.

Commands that would persist, replicate, subscribe, flush, or block are refused with `ERR command not allowed in the public playground`. `FLUSHALL` is one of them, so one visitor cannot wipe another session (sessions do not share a keyspace anyway).

## Vercel

Point the project at this repository. `vercel.json` builds `playground/config.js` from `FLASHDB_WS_URL` and publishes the `playground/` directory.

| Variable | Required | Meaning |
| --- | --- | --- |
| `FLASHDB_WS_URL` | yes | WebSocket URL of the Railway service, for example `wss://flashdb.up.railway.app/ws`. |

A page opened as `https://flashdb.rhythmarora.dev/?ws=wss://other.example/ws` uses that socket instead of the build-time value. An empty value uses `wss://` or `ws://` on the page's own host, which is what local `cargo run --example ws_bridge` expects.

The Railway allowlist must include the Vercel origin. The page and the socket are different hosts, so this is a cross-origin WebSocket; the bridge checks `Origin` and does not fall back to "no header".
