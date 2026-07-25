// main.rs — thin entry point. All server logic lives in the library (lib.rs)
// so it can be started and driven by the integration tests.
use anyhow::Result;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let addr = "127.0.0.1:6379";
    let listener = TcpListener::bind(addr).await?;
    println!("FlashDB listening on {addr}");
    flashdb::run(listener).await
}
