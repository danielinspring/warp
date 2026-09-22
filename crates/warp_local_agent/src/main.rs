use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use warp_local_agent::{OllamaProviderFactory, ServerState};

/// Local agent service for Warp's Ollama integration.
#[derive(Parser, Debug)]
#[command(name = "warp-local-agent", version, about)]
struct Args {
    /// Address to listen on. Warp's own local HTTP server owns 9277-9282, hence the default.
    #[arg(
        long,
        env = "WARP_LOCAL_AGENT_LISTEN",
        default_value = "127.0.0.1:9377"
    )]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warp_local_agent=info,local_agent_runtime=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let listener = TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "warp-local-agent listening");
    warp_local_agent::serve(listener, ServerState::new(Arc::new(OllamaProviderFactory))).await?;
    Ok(())
}
