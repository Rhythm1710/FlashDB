// Uncomment this block to pass the first stage
use std::{
    io::{Read,Write},
    net::TcpListener,
};

use bytes::buf;

fn main() {
    // You can use print statements as follows for debugging, they'll be visible when running tests.
    println!("Logs from your program will appear here!");
    let listener = TcpListener::bind("127.0.0.1:6379").unwrap();
    
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                println!("accepted new connection");
                let mut buf = [0; 512];
                stream.read(&mut buf).unwrap();

                stream.write(b"PONG\r\n").unwrap();
            }
            Err(e) => {
                println!("error: {}", e);
            }
        }
    }
}


// use std::io::{Read, Write};
// use std::net::{TcpListener, TcpStream};

// fn main() {
//     // Creates a TCP server listening on localhost:8080
//     let listener = TcpListener::bind("localhost:8080").expect("Could not bind");

//     for stream in listener.incoming() {
//         match stream {
//             Ok(stream) => {
//                 handle_client(stream);
//             }
//             Err(e) => {
//                 eprintln!("Failed: {}", e);
//             }
//         }
//     }
// }

// fn handle_client(mut stream: TcpStream) {
//     let mut buf = [0; 512];
//     loop {
//         let bytes_read = stream.read(&mut buf).expect("Failed to read from client");

//         if bytes_read == 0 {
//             return;
//         }

//         stream.write_all(&buf[0..bytes_read]).expect("Failed to write to client");
//     }
// }
