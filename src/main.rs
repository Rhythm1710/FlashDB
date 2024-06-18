use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    println!("Logs from your program will appear here!");
    let listener = TcpListener::bind("127.0.0.1:6379").await?;

    loop {
        let (stream, _) = listener.accept().await?;
        println!("Accepted new connection");
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream).await {
                println!("Error: {}", e);
            }
        });
    }
}

async fn handle_connection(mut stream: TcpStream) -> Result<()> {
    loop {
        let mut buffer = [0; 1024];
        let bytes_read = stream.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        if let Err(e) = handle_request(&mut stream).await {
            println!("Error: {}", e);
        }
    }
    Ok(())
}

async fn handle_request(stream: &mut TcpStream) -> Result<()> {
    let response = b"+PONG\r\n";
    stream.write_all(response).await?;
    stream.flush().await?;
    Ok(())
}
//Testing
