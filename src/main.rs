use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use priorart::auth::{Grant, IssuedCredential};
use priorart::config::Settings;
use priorart::service::{Service, DATABASE_FILE};
use priorart::store::{Store, LOCAL_PRINCIPAL_ID};
use priorart::{api, mcp, VERSION};

#[derive(Parser)]
#[command(name = "priorart", version, about = "priorart text store")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Local credential administration; requires direct access to PRIORART_DATA_DIR
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
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

#[derive(Subcommand)]
enum AdminCommand {
    /// Create a principal that can own collections, author records, and hold credentials
    CreatePrincipal,
    /// Provision a collection through trusted local administration.
    CreateCollection {
        #[arg(long, default_value = LOCAL_PRINCIPAL_ID)]
        owner: String,
        #[arg(long, default_value = "restricted")]
        visibility: priorart::store::Visibility,
    },
    /// Issue a credential for an existing principal. Prints its secret once.
    Issue {
        #[arg(long, default_value = LOCAL_PRINCIPAL_ID)]
        principal: String,
        /// Explicit COLLECTION:OPERATION; repeat for each permission
        #[arg(long = "grant", required = true)]
        grants: Vec<Grant>,
        /// Exclusive expiry as a UTC Unix timestamp in seconds; omitted means no expiry
        #[arg(long)]
        expires_at: Option<i64>,
    },
    /// List credential metadata, never secrets or verifiers
    List {
        #[arg(long, default_value = LOCAL_PRINCIPAL_ID)]
        principal: String,
    },
    /// Replace a credential and revoke the old one; prints the new secret once
    Rotate {
        #[arg(long)]
        credential_id: String,
    },
    /// Permanently revoke a credential
    Revoke {
        #[arg(long)]
        credential_id: String,
    },
    /// Replace explicit grants; no --grant removes all permissions
    SetGrants {
        #[arg(long)]
        credential_id: String,
        #[arg(long = "grant")]
        grants: Vec<Grant>,
    },
}

fn administer(command: AdminCommand) -> Result<(), AnyError> {
    let settings = Settings::from_env()?;
    // No HTTP listener or remote provisioning is involved.
    let store = Store::open(settings.data_dir.join(DATABASE_FILE))?;
    match command {
        AdminCommand::CreatePrincipal => println!("{}", store.create_principal()?),
        AdminCommand::CreateCollection { owner, visibility } => {
            println!("{}", store.create_collection(&owner, visibility)?);
        }
        AdminCommand::Issue {
            principal,
            grants,
            expires_at,
        } => {
            print_issued(store.issue_credential(&principal, &grants, expires_at)?)?;
        }
        AdminCommand::List { principal } => {
            println!(
                "{}",
                serde_json::to_string(&store.credentials(&principal)?)?
            );
        }
        AdminCommand::Rotate { credential_id } => {
            print_issued(store.rotate_credential(&credential_id)?)?;
        }
        AdminCommand::Revoke { credential_id } => store.revoke_credential(&credential_id)?,
        AdminCommand::SetGrants {
            credential_id,
            grants,
        } => {
            store.replace_credential_grants(&credential_id, &grants)?;
        }
    }
    Ok(())
}

fn print_issued(issued: IssuedCredential) -> Result<(), AnyError> {
    // The only intentional secret output. IssuedCredential itself is not serializable.
    let info = issued.info.clone();
    let output = serde_json::json!({"credential": info, "secret": issued.into_secret()});
    use std::io::Write;
    writeln!(std::io::stdout().lock(), "{output}")?;
    Ok(())
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Serve { host, port } => serve(host, port),
        Command::Admin { command } => administer(command),
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
    let service = Arc::new(Service::open(settings.clone())?);
    eprintln!(
        "priorart {VERSION}: data at {}",
        settings.data_dir.display()
    );
    runtime()?.block_on(async move {
        let address: SocketAddr = tokio::net::lookup_host((settings.host.as_str(), settings.port))
            .await?
            .next()
            .ok_or("the host resolved to no address")?;
        if !address.ip().is_loopback() && !settings.accepts_remote_peers() {
            return Err("the host resolved to a non-loopback address".into());
        }
        let listener = tokio::net::TcpListener::bind(address).await?;
        eprintln!("priorart: listening on http://{address}");
        axum::serve(
            listener,
            api::router(service).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await?;
        Ok(())
    })
}

/// Ctrl-C, or SIGTERM from `docker stop` and service managers.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
