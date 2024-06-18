// main.rs
use resp::Value;
use tokio::net::{TcpListener, TcpStream};
use anyhow::Result;
mod resp;

#[tokio::main]
async fn main() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:6379").await?;
    
    loop {
        let stream = listener.accept().await;
        match stream {
            Ok((stream, _)) => {
                println!("accepted new connection");
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream).await {
                        eprintln!("Connection error: {:?}", e);
                    }
                });
            }
            Err(e) => {
                println!("error: {}", e);
            }
        }
    }
}

async fn handle_conn(stream: TcpStream) -> Result<()> {
    let mut handler = resp::RespHandler::new(stream);
    println!("Starting read loop");
    loop {
        let value = match handler.read_value().await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => return Err(e),
        };
        println!("Got value {:?}", value);
        
        let response = match process_command(value) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };
        
        println!("Sending value {:?}", response);
        handler.write_value(response).await?;
    }
    Ok(())
}

fn process_command(value: Value) -> Result<Value> {
    let (command, args) = extract_command(value)?;
    match command.to_lowercase().as_str() {
        "ping" => Ok(Value::SimpleString("PONG".to_string())),
        "echo" => Ok(args.first().unwrap().clone()),
        c => Err(anyhow::anyhow!("Cannot handle command {}", c)),
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
