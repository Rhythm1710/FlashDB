// main.rs
use anyhow::Result;
use resp::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
mod resp;

type Store = Arc<Mutex<HashMap<String, String>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:6379").await?;
    let storage: Store = Arc::new(Mutex::new(HashMap::new()));

    loop {
        let (stream, _) = listener.accept().await?;
        println!("accepted new connection");
        let storage = storage.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, storage).await {
                eprintln!("Connection error: {:?}", e);
            }
        });
    }
}

async fn handle_conn(stream: TcpStream, storage: Store) -> Result<()> {
    let mut handler = resp::RespHandler::new(stream);
    loop {
        let value = match handler.read_value().await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => return Err(e),
        };

        // A malformed request produces an error reply, not a dropped
        // connection, so one bad client can't take down its own session.
        let response = process_command(value, &storage);
        handler.write_value(response).await?;
    }
    Ok(())
}

fn process_command(value: Value, storage: &Store) -> Value {
    let (command, args) = match extract_command(value) {
        Ok(parts) => parts,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };

    match command.to_lowercase().as_str() {
        "ping" => match args.first() {
            Some(v) => v.clone(),
            None => Value::SimpleString("PONG".to_string()),
        },
        "echo" => match args.first() {
            Some(v) => v.clone(),
            None => wrong_args("echo"),
        },
        "set" => set(&args, storage),
        "get" => get(&args, storage),
        "del" => del(&args, storage),
        c => Value::Error(format!("ERR unknown command '{}'", c)),
    }
}

fn set(args: &[Value], storage: &Store) -> Value {
    if args.len() < 2 {
        return wrong_args("set");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    let value = match unpack_bulk_str(&args[1]) {
        Ok(v) => v,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    storage.lock().unwrap().insert(key, value);
    Value::SimpleString("OK".to_string())
}

fn get(args: &[Value], storage: &Store) -> Value {
    if args.len() != 1 {
        return wrong_args("get");
    }
    let key = match unpack_bulk_str(&args[0]) {
        Ok(k) => k,
        Err(e) => return Value::Error(format!("ERR {}", e)),
    };
    match storage.lock().unwrap().get(&key).cloned() {
        Some(v) => Value::BulkString(v),
        None => Value::Null,
    }
}

fn del(args: &[Value], storage: &Store) -> Value {
    if args.is_empty() {
        return wrong_args("del");
    }
    let mut removed = 0i64;
    let mut store = storage.lock().unwrap();
    for arg in args {
        let key = match unpack_bulk_str(arg) {
            Ok(k) => k,
            Err(e) => return Value::Error(format!("ERR {}", e)),
        };
        if store.remove(&key).is_some() {
            removed += 1;
        }
    }
    Value::Integer(removed)
}

fn wrong_args(cmd: &str) -> Value {
    Value::Error(format!(
        "ERR wrong number of arguments for '{}' command",
        cmd
    ))
}

fn extract_command(value: Value) -> Result<(String, Vec<Value>)> {
    match value {
        Value::Array(a) => {
            let mut it = a.into_iter();
            let cmd = it.next().ok_or_else(|| anyhow::anyhow!("empty command"))?;
            Ok((unpack_bulk_str(&cmd)?, it.collect()))
        }
        _ => Err(anyhow::anyhow!("expected an array of bulk strings")),
    }
}

fn unpack_bulk_str(value: &Value) -> Result<String> {
    match value {
        Value::BulkString(s) => Ok(s.clone()),
        _ => Err(anyhow::anyhow!("expected a bulk string argument")),
    }
}
