//! Limits for the public playground.
//!
//! Each browser WebSocket gets its own [`flashdb::Server`]. This module decides
//! which commands that session may run, and how big or how fast it may get.
//! Dropping a [`SessionDir`] deletes that session's directory.

use flashdb::resp::Value;
use flashdb::Server;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Caps applied to every public session. Flags and environment variables fill
/// this in; the numbers here are the defaults.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_sessions: usize,
    pub max_memory: usize,
    pub max_key: usize,
    pub max_value: usize,
    pub max_per_sec: u32,
    pub idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sessions: 32,
            max_memory: 1024 * 1024,
            max_key: 1024,
            max_value: 8 * 1024,
            max_per_sec: 30,
            idle: Duration::from_secs(120),
        }
    }
}

/// How many public sessions are open. `try_enter` fails when the cap is hit,
/// and dropping the permit frees the slot even if the task panics.
pub struct Gate {
    current: AtomicUsize,
    max: usize,
}

pub struct Permit {
    gate: Arc<Gate>,
}

impl Gate {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            current: AtomicUsize::new(0),
            max: max.max(1),
        })
    }

    pub fn try_enter(self: &Arc<Self>) -> Option<Permit> {
        let mut current = self.current.load(Ordering::Relaxed);
        loop {
            if current >= self.max {
                return None;
            }
            match self.current.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(Permit {
                        gate: Arc::clone(self),
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.current.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A per-session directory removed when the socket closes.
pub struct SessionDir {
    path: PathBuf,
}

impl SessionDir {
    pub fn create(parent: &Path, id: u64) -> std::io::Result<Self> {
        let path = parent.join(format!("s-{id}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Counts commands inside a one-second window.
pub struct RateWindow {
    start: Instant,
    count: u32,
    max: u32,
}

impl RateWindow {
    pub fn new(max_per_sec: u32) -> Self {
        Self {
            start: Instant::now(),
            count: 0,
            max: max_per_sec.max(1),
        }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.start) >= Duration::from_secs(1) {
            self.start = now;
            self.count = 0;
        }
        if self.count >= self.max {
            return false;
        }
        self.count += 1;
        true
    }
}

/// Public mode accepts only origins on the allowlist. A missing `Origin` is
/// rejected, unlike the development bridge.
pub fn origin_allowed(origin: Option<&str>, allow: &[String]) -> bool {
    let Some(origin) = origin.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let origin = origin.trim_end_matches('/');
    allow.iter().any(|allowed| {
        allowed
            .trim()
            .trim_end_matches('/')
            .eq_ignore_ascii_case(origin)
    })
}

const BLOCKED: &[&str] = &[
    "save",
    "bgsave",
    "bgrewriteaof",
    "shutdown",
    "config",
    "debug",
    "replicaof",
    "slaveof",
    "sync",
    "psync",
    "subscribe",
    "psubscribe",
    "monitor",
    "flushall",
    "flushdb",
    "blpop",
    "brpop",
    "brpoplpush",
    "blmove",
    "blmpop",
    "bzpopmin",
    "bzpopmax",
    "bzmpop",
];

/// `Some` when this command must not run in the public playground.
pub fn blocked_reason(parts: &[String]) -> Option<&'static str> {
    let name = parts.first()?.to_ascii_lowercase();
    if BLOCKED.contains(&name.as_str()) {
        return Some("ERR command not allowed in the public playground");
    }
    if name == "xread" && parts.iter().any(|part| part.eq_ignore_ascii_case("block")) {
        return Some("ERR blocking commands are not allowed in the public playground");
    }
    None
}

/// Reject a key or value that is larger than the session cap.
pub fn size_reason(parts: &[String], limits: &Limits) -> Option<String> {
    let name = parts.first()?.to_ascii_lowercase();
    if parts.len() >= 2 && command_has_key(&name) && parts[1].len() > limits.max_key {
        return Some(format!(
            "ERR key is too large (max {} bytes)",
            limits.max_key
        ));
    }
    let values = if command_has_key(&name) {
        &parts[2..]
    } else {
        &parts[1..]
    };
    if values.iter().any(|value| value.len() > limits.max_value) {
        return Some(format!(
            "ERR value is too large (max {} bytes)",
            limits.max_value
        ));
    }
    None
}

fn command_has_key(name: &str) -> bool {
    !matches!(
        name,
        "ping"
            | "echo"
            | "info"
            | "dbsize"
            | "quit"
            | "multi"
            | "exec"
            | "discard"
            | "unwatch"
            | "save"
            | "bgsave"
            | "flushall"
            | "flushdb"
            | "shutdown"
            | "monitor"
            | "sync"
            | "psync"
    )
}

/// Bytes a write would add, used as a ceiling before the command runs.
pub fn stored_bytes(parts: &[String]) -> usize {
    let Some(name) = parts.first() else {
        return 0;
    };
    match name.to_ascii_lowercase().as_str() {
        "set" | "rpush" | "lpush" | "hset" | "xadd" | "zadd" | "append" => {
            parts.iter().skip(1).map(String::len).sum()
        }
        _ => 0,
    }
}

pub fn run_command(
    server: &Server,
    rate: &mut RateWindow,
    limits: &Limits,
    parts: &[String],
    now: Instant,
) -> Result<Value, String> {
    if parts.is_empty() {
        return Ok(Value::SimpleString(String::new()));
    }
    if !rate.allow(now) {
        return Err("ERR rate limit exceeded".to_string());
    }
    if let Some(reason) = blocked_reason(parts) {
        return Err(reason.to_string());
    }
    if let Some(reason) = size_reason(parts, limits) {
        return Err(reason);
    }
    let extra = stored_bytes(parts);
    if extra > 0 && server.used_memory().saturating_add(extra) > limits.max_memory {
        return Err(format!(
            "ERR session memory limit exceeded (max {} bytes)",
            limits.max_memory
        ));
    }
    let value = Value::Array(
        parts
            .iter()
            .map(|part| Value::BulkString(part.clone()))
            .collect(),
    );
    Ok(flashdb::process_command(value, server))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn limits() -> Limits {
        Limits {
            max_sessions: 2,
            max_memory: 64,
            max_key: 4,
            max_value: 8,
            max_per_sec: 2,
            idle: Duration::from_secs(30),
        }
    }

    #[test]
    fn public_origin_rejects_a_missing_or_foreign_origin() {
        let allow = vec!["https://flashdb.rhythmarora.dev".to_string()];
        assert!(!origin_allowed(None, &allow));
        assert!(!origin_allowed(Some(""), &allow));
        assert!(!origin_allowed(Some("https://evil.example"), &allow));
        assert!(origin_allowed(
            Some("https://flashdb.rhythmarora.dev"),
            &allow
        ));
        assert!(origin_allowed(
            Some("https://flashdb.rhythmarora.dev/"),
            &allow
        ));
    }

    #[test]
    fn public_mode_blocks_admin_flush_and_blocking_commands() {
        for name in [
            "SAVE",
            "BGSAVE",
            "BGREWRITEAOF",
            "SHUTDOWN",
            "CONFIG",
            "DEBUG",
            "REPLICAOF",
            "SLAVEOF",
            "SYNC",
            "PSYNC",
            "SUBSCRIBE",
            "PSUBSCRIBE",
            "MONITOR",
            "FLUSHALL",
            "BLPOP",
        ] {
            assert!(
                blocked_reason(&parts(&[name])).is_some(),
                "{name} should be blocked"
            );
        }
        assert!(blocked_reason(&parts(&["XREAD", "BLOCK", "0", "STREAMS", "s", "$"])).is_some());
        assert!(blocked_reason(&parts(&["XREAD", "STREAMS", "s", "0"])).is_none());
        assert!(blocked_reason(&parts(&["PING"])).is_none());
    }

    #[test]
    fn sessions_do_not_share_keys_and_their_directory_is_removed() {
        let parent = std::env::temp_dir().join(format!("flashdb-pub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let dir_a = SessionDir::create(&parent, 1).unwrap();
        let dir_b = SessionDir::create(&parent, 2).unwrap();
        let server_a = Server::isolated(dir_a.path().join("dump.rdb"));
        let server_b = Server::isolated(dir_b.path().join("dump.rdb"));
        let limits = Limits {
            max_memory: 10_000,
            max_key: 100,
            max_value: 100,
            max_per_sec: 100,
            ..limits()
        };
        let now = Instant::now();
        let mut rate_a = RateWindow::new(100);
        let mut rate_b = RateWindow::new(100);
        run_command(
            &server_a,
            &mut rate_a,
            &limits,
            &parts(&["SET", "k", "from-a"]),
            now,
        )
        .unwrap();
        run_command(
            &server_b,
            &mut rate_b,
            &limits,
            &parts(&["SET", "k", "from-b"]),
            now,
        )
        .unwrap();
        let got_a =
            run_command(&server_a, &mut rate_a, &limits, &parts(&["GET", "k"]), now).unwrap();
        let got_b =
            run_command(&server_b, &mut rate_b, &limits, &parts(&["GET", "k"]), now).unwrap();
        assert_eq!(got_a, Value::BulkString("from-a".to_string()));
        assert_eq!(got_b, Value::BulkString("from-b".to_string()));
        let path_a = dir_a.path().to_path_buf();
        drop(dir_a);
        assert!(!path_a.exists(), "disconnect removes the session directory");
        drop(dir_b);
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn rate_and_size_caps_return_errors() {
        let server = Server::isolated(PathBuf::from("unused.rdb"));
        let limits = limits();
        let mut rate = RateWindow::new(limits.max_per_sec);
        let now = Instant::now();
        assert!(run_command(&server, &mut rate, &limits, &parts(&["PING"]), now).is_ok());
        assert!(run_command(&server, &mut rate, &limits, &parts(&["PING"]), now).is_ok());
        let limited = run_command(&server, &mut rate, &limits, &parts(&["PING"]), now).unwrap_err();
        assert!(limited.contains("rate limit"), "{limited}");
        assert!(rate.allow(now + Duration::from_secs(2)));

        let mut open = RateWindow::new(100);
        let key = size_reason(&parts(&["SET", "hello", "v"]), &limits).unwrap();
        assert!(key.contains("key is too large"), "{key}");
        let value = size_reason(&parts(&["SET", "k", "0123456789"]), &limits).unwrap();
        assert!(value.contains("value is too large"), "{value}");
        let tight = Limits {
            max_memory: 4,
            max_per_sec: 100,
            ..limits
        };
        let memory = run_command(
            &server,
            &mut open,
            &tight,
            &parts(&["SET", "k", "01234567"]),
            now,
        )
        .unwrap_err();
        assert!(memory.contains("memory limit"), "{memory}");
    }

    #[test]
    fn the_session_gate_stops_at_the_cap() {
        let gate = Gate::new(1);
        let first = gate.try_enter().expect("first session");
        assert!(gate.try_enter().is_none());
        drop(first);
        assert!(gate.try_enter().is_some());
    }
}
