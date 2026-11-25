//! Limits for the public playground.
//!
//! Each browser WebSocket gets its own [`flashdb::Server`]. This module decides
//! which commands that session may run, and how big or how fast it may get.
//! Dropping a [`SessionDir`] deletes that session's directory.

use flashdb::resp::Value;
use flashdb::Server;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Caps applied to every public session. Flags and environment variables fill
/// this in; the numbers here are the defaults.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_sessions: usize,
    pub max_per_client: usize,
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
            max_per_client: 4,
            max_memory: 1024 * 1024,
            max_key: 1024,
            max_value: 8 * 1024,
            max_per_sec: 30,
            idle: Duration::from_secs(120),
        }
    }
}

/// Largest command text a public session will run. A key plus two values at
/// the size caps (`HSET key field value`) fits, with room for the command
/// word and spaces. A longer line is an error reply; the socket stays open.
pub fn max_message_bytes(limits: &Limits) -> usize {
    limits
        .max_key
        .saturating_add(limits.max_value.saturating_mul(2))
        .saturating_add(256)
}

pub fn command_line_too_long(line: &str, limits: &Limits) -> bool {
    line.len() > max_message_bytes(limits)
}

/// Address used for the per-client session cap.
///
/// The default is the TCP peer. `FLASHDB_TRUST_PROXY` opts in to `X-Real-IP`,
/// then the right-most `X-Forwarded-For` entry. A missing or unparseable
/// header keeps the peer address.
pub fn client_ip(
    peer: IpAddr,
    trust_proxy: bool,
    real_ip: Option<&str>,
    forwarded_for: Option<&str>,
) -> IpAddr {
    if !trust_proxy {
        return peer;
    }
    if let Some(ip) = real_ip.and_then(parse_header_ip) {
        return ip;
    }
    if let Some(header) = forwarded_for {
        if let Some(last) = header.rsplit(',').next() {
            if let Some(ip) = parse_header_ip(last) {
                return ip;
            }
        }
    }
    peer
}

fn parse_header_ip(raw: &str) -> Option<IpAddr> {
    let token = raw.trim().trim_matches('"');
    if token.is_empty() {
        return None;
    }
    token.parse().ok()
}

/// How many public sessions are open. `try_enter` fails when the cap is hit,
/// and dropping the permit frees the slot even if the task panics.
pub struct Gate {
    current: AtomicUsize,
    max: usize,
    max_per_client: usize,
    clients: Mutex<HashMap<IpAddr, usize>>,
}

pub struct Permit {
    gate: Arc<Gate>,
    client: IpAddr,
}

pub enum Admit {
    Admitted(Permit),
    TooManySessions,
    TooManyForClient,
}

impl Gate {
    pub fn new(max: usize, max_per_client: usize) -> Arc<Self> {
        Arc::new(Self {
            current: AtomicUsize::new(0),
            max: max.max(1),
            max_per_client: max_per_client.max(1),
            clients: Mutex::new(HashMap::new()),
        })
    }

    pub fn try_enter(self: &Arc<Self>, client: IpAddr) -> Admit {
        let mut clients = self.clients.lock().unwrap();
        let open = clients.get(&client).copied().unwrap_or(0);
        if open >= self.max_per_client {
            return Admit::TooManyForClient;
        }
        let mut current = self.current.load(Ordering::Relaxed);
        loop {
            if current >= self.max {
                return Admit::TooManySessions;
            }
            match self.current.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    *clients.entry(client).or_insert(0) += 1;
                    return Admit::Admitted(Permit {
                        gate: Arc::clone(self),
                        client,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.current.fetch_sub(1, Ordering::AcqRel);
        let mut clients = self.gate.clients.lock().unwrap();
        if let Some(count) = clients.get_mut(&self.client) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                clients.remove(&self.client);
            }
        }
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

fn changes_keyspace(name: &str) -> bool {
    flashdb::is_write_command(name) || name == "exec"
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
    let name = parts[0].to_ascii_lowercase();
    let snapshot = if changes_keyspace(&name) {
        Some(server.snapshot_entries())
    } else {
        None
    };
    let value = Value::Array(
        parts
            .iter()
            .map(|part| Value::BulkString(part.clone()))
            .collect(),
    );
    let reply = flashdb::process_command(value, server);
    if server.used_memory() > limits.max_memory {
        if let Some(entries) = snapshot {
            server.restore_entries(entries);
        }
        return Err(format!(
            "ERR session memory limit exceeded (max {} bytes)",
            limits.max_memory
        ));
    }
    Ok(reply)
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
            max_per_client: 1,
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
            max_per_client: 4,
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
        assert_eq!(server.used_memory(), 0, "the oversized SET is rolled back");
    }

    #[test]
    fn memory_cap_rolls_back_a_new_sorted_set_member() {
        let server = Server::isolated(PathBuf::from("unused.rdb"));
        let limits = Limits {
            max_memory: 12,
            max_key: 64,
            max_value: 64,
            max_per_sec: 20,
            max_sessions: 2,
            max_per_client: 2,
            idle: Duration::from_secs(30),
        };
        let mut rate = RateWindow::new(20);
        let now = Instant::now();
        run_command(&server, &mut rate, &limits, &parts(&["SET", "k", "v"]), now).unwrap();
        let before = server.used_memory();
        let err = run_command(
            &server,
            &mut rate,
            &limits,
            &parts(&["ZINCRBY", "z", "1", "abcdefgh"]),
            now,
        )
        .unwrap_err();
        assert!(err.contains("memory limit"), "{err}");
        assert_eq!(server.used_memory(), before);
        let score = run_command(
            &server,
            &mut rate,
            &limits,
            &parts(&["ZSCORE", "z", "abcdefgh"]),
            now,
        )
        .unwrap();
        assert_eq!(score, Value::Null);
    }

    #[test]
    fn the_session_gate_stops_at_the_cap() {
        let gate = Gate::new(1, 4);
        let client = IpAddr::from([127, 0, 0, 1]);
        let other = IpAddr::from([127, 0, 0, 2]);
        let Admit::Admitted(first) = gate.try_enter(client) else {
            panic!("first session");
        };
        assert!(matches!(gate.try_enter(other), Admit::TooManySessions));
        drop(first);
        assert!(matches!(gate.try_enter(other), Admit::Admitted(_)));
    }

    #[test]
    fn message_cap_fits_one_hset_of_two_max_sized_arguments() {
        let limits = Limits::default();
        let key = "k".repeat(limits.max_key);
        let field = "f".repeat(limits.max_value);
        let value = "v".repeat(limits.max_value);
        let line = format!("HSET {key} {field} {value}");
        assert!(
            !command_line_too_long(&line, &limits),
            "len {} cap {}",
            line.len(),
            max_message_bytes(&limits)
        );
        let extra = format!("{line} {}", "x".repeat(257));
        assert!(command_line_too_long(&extra, &limits));
    }

    #[test]
    fn client_ip_uses_the_peer_unless_the_proxy_is_trusted() {
        let peer = IpAddr::from([10, 0, 0, 1]);
        let real = "203.0.113.9";
        let forwarded = "198.51.100.4, 203.0.113.8";
        assert_eq!(
            client_ip(peer, false, Some(real), Some(forwarded)),
            peer,
            "headers are ignored until FLASHDB_TRUST_PROXY is set"
        );
        assert_eq!(
            client_ip(peer, true, Some(real), Some(forwarded)),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_ip(peer, true, None, Some(forwarded)),
            "203.0.113.8".parse::<IpAddr>().unwrap(),
            "the right-most X-Forwarded-For entry"
        );
        assert_eq!(
            client_ip(peer, true, Some("not-an-ip"), Some("also-bad, still-bad")),
            peer
        );
        assert_eq!(client_ip(peer, true, None, None), peer);
        assert_eq!(
            client_ip(peer, true, Some(" 2001:db8::8 "), None),
            "2001:db8::8".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn one_client_cannot_take_every_session_slot() {
        let gate = Gate::new(8, 1);
        let first = IpAddr::from([10, 0, 0, 1]);
        let second = IpAddr::from([10, 0, 0, 2]);
        let Admit::Admitted(_hold) = gate.try_enter(first) else {
            panic!("first client");
        };
        assert!(matches!(gate.try_enter(first), Admit::TooManyForClient));
        assert!(matches!(gate.try_enter(second), Admit::Admitted(_)));
    }
}
