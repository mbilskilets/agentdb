//! Runs the agentdb HTTP server until it receives SIGTERM or SIGINT.
//!
//! Settings come from the environment: `AGENTDB_SECRET`, `AGENTDB_MASTER_KEY`,
//! `AGENTDB_DATA_DIR` (default `./data`), `AGENTDB_HOST` (default
//! `127.0.0.1`), `AGENTDB_PORT` (default `4000`),
//! `AGENTDB_MAX_OPEN_TENANTS` (default 100) and, for English queries,
//! `TYPESAFE_API_KEY`.

use std::error::Error;

use agentdb::server::{Config, serve};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    let host = std::env::var("AGENTDB_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let port: u16 = match std::env::var("AGENTDB_PORT") {
        Ok(port) => port.parse()?,
        Err(_) => 4000,
    };
    let english = if config.jev.is_some() {
        "on"
    } else {
        "off (no TYPESAFE_API_KEY)"
    };
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    println!(
        "agentdb listening on http://{host}:{port}, data in {}, English queries {english}",
        config.data_dir.display()
    );
    let told_to_stop = async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    };
    serve(listener, config, told_to_stop).await?;
    println!("agentdb stopped");
    Ok(())
}
