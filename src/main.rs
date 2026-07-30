// main.rs — thin entry point. All server logic lives in the library (lib.rs)
// so it can be started and driven by the integration tests.
use anyhow::Result;
use flashdb::Config;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    // Parse the command line; a bad flag prints a message and exits non-zero
    // rather than panicking with a backtrace.
    let config = match Config::parse(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(msg) => {
            eprintln!("flashdb: {msg}");
            std::process::exit(1);
        }
    };

    let addr = config.addr();
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "FlashDB listening on {addr} (dir: {}, dbfilename: {})",
        config.dir, config.dbfilename
    );
    // Recover any keys a previous run persisted to <dir>/<dbfilename> before we
    // start taking client traffic.
    flashdb::run_with_config(listener, &config).await
}
