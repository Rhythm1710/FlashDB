// main.rs
use resp::Value;
use tokio::net::{TcpListener, TcpStream};
use anyhow::Result;
use std::sync::{Arc, Mutex};
mod resp;

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:6379").await?;
    let storage = Arc::new(Mutex::new(std::collections::HashMap::new()));

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

async fn handle_conn(stream: TcpStream, storage: Arc<Mutex<std::collections::HashMap<String, String>>>) -> Result<()> {
    let mut handler = resp::RespHandler::new(stream);
    println!("Starting read loop");
    loop {
        let value = match handler.read_value().await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => return Err(e),
        };
        println!("Got value {:?}", value);
        
        let response = match process_command(value, &storage) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };
        
        println!("Sending value {:?}", response);
        handler.write_value(response).await?;
    }
    Ok(())
}

fn process_command(value: Value, storage: &Arc<Mutex<std::collections::HashMap<String, String>>>) -> Result<Value> {
    let (command, args) = extract_command(value)?;
    match command.to_lowercase().as_str() {
        "ping" => Ok(Value::SimpleString("PONG".to_string())),
        "echo" => Ok(args.first().unwrap().clone()),
        "set" => {
            let key = unpack_bulk_str(args[0].clone())?;
            let value = unpack_bulk_str(args[1].clone())?;
            let mut storage = storage.lock().unwrap();
            set(&mut storage, key, value)
        },
        "get" => {
            let key = unpack_bulk_str(args[0].clone())?;
            let storage = storage.lock().unwrap();
            Ok(get(&storage, key))
        },
        c => Err(anyhow::anyhow!("Cannot handle command {}", c)),
    }
}

fn set(storage: &mut std::collections::HashMap<String, String>, key: String, value: String) -> Result<Value> {
    storage.insert(key, value);
    Ok(Value::SimpleString("OK".to_string()))
}

fn get(storage: &std::collections::HashMap<String, String>, key: String) -> Value {
    match storage.get(&key) {
        Some(v) => Value::BulkString(v.to_string()),
        None => Value::Null,
    }
}

fn extract_command(value: Value) -> Result<(String, Vec<Value>)> {
    match value {
        Value::Array(a) => {
            Ok((
                unpack_bulk_str(a.first().unwrap().clone())?,
                a.into_iter().skip(1).collect(),
            ))
        },
        _ => Err(anyhow::anyhow!("Unexpected command format")),
    }
}

fn unpack_bulk_str(value: Value) -> Result<String> {
    match value {
        Value::BulkString(s) => Ok(s),
        _ => Err(anyhow::anyhow!("Expected command to be a bulk string")),
    }
}
