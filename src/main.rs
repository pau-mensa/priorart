use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use priorart::config::Settings;
use priorart::service::Service;
use priorart::{api, mcp, VERSION};

#[derive(Parser)]
#[command(name = "priorart", version, about = "priorart text store")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server (settings from PRIORART_*)
    Serve {
        /// Override PRIORART_HOST
        #[arg(long)]
        host: Option<String>,
        /// Override PRIORART_PORT
        #[arg(long)]
        port: Option<u16>,
    },
    /// Run the stdio MCP server against a running priorart (PRIORART_URL)
    Mcp {
        /// Override PRIORART_URL (default http://127.0.0.1:8000)
        #[arg(long)]
        url: Option<String>,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Serve { host, port } => serve(host, port),
        Command::Mcp { url } => runtime().and_then(|runtime| runtime.block_on(mcp::run(url))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

type AnyError = Box<dyn std::error::Error + Send + Sync>;

fn runtime() -> Result<tokio::runtime::Runtime, AnyError> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn serve(host: Option<String>, port: Option<u16>) -> Result<(), AnyError> {
    let mut settings = Settings::from_env()?;
    settings.host = host.unwrap_or(settings.host);
    settings.port = port.unwrap_or(settings.port);
    settings.validate()?;
    // Opened before the runtime starts: loading the encoder may download it
    // with a blocking client that owns its own runtime.
    let service = Arc::new(Service::open(settings.clone())?);
    eprintln!(
        "priorart {VERSION}: data at {}, encoder {}, {} documents",
        settings.data_dir.display(),
        settings.encoder,
        service.health().document_count
    );
    runtime()?.block_on(async move {
        let address: SocketAddr = tokio::net::lookup_host((settings.host.as_str(), settings.port))
            .await?
            .next()
            .ok_or("the host resolved to no address")?;
        let listener = tokio::net::TcpListener::bind(address).await?;
        eprintln!("priorart: listening on http://{address}");
        axum::serve(listener, api::router(service))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}
