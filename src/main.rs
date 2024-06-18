use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
};

use anyhow::Result;

fn main() {
    println!("Logs from your program will appear here!");
    let listener = TcpListener::bind("127.0.0.1:6379").unwrap();
    
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                println!("accepted new connection");
                if let Err(e) = handle_connection(&mut stream) {
                    println!("error: {}", e);
                }
            }
            Err(e) => {
                println!("error: {}", e);
            }
        }
    }
}

fn handle_connection(stream: &mut TcpStream) -> Result<()> {
    loop {
        let mut buffer = [0; 1024];
        match stream.read(&mut buffer) {
            Ok(bytes_read) => {
                if bytes_read == 0 {
                    continue;
                }
            }
            Err(e) => {
                println!("err: {}", e);
                continue;
            }
        }
        if let Err(e) = handle_request(stream) {
            println!("err: {}", e);
            continue;
        }
    }
}

fn handle_request(stream: &mut TcpStream) -> std::io::Result<()> {
    let response = b"+PONG\r\n";
    stream.write_all(response)?;
    stream.flush()?;
    Ok(())
}
