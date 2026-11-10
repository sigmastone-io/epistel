//! The Epistel mail server.

mod admin;
mod backends;
mod config;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use epistel_core::ingest::Ingest;

use crate::admin::{AccountCommand, AddressCommand, AdminCommand, AdminError, PasswordCommand};
use crate::backends::Backends;
use crate::config::Config;

#[derive(Parser)]
#[command(name = "epistel", version, about)]
struct Cli {
    /// The configuration file.
    #[arg(long, short, global = true, default_value = "/etc/epistel/epistel.toml", env = "EPISTEL_CONFIG")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the server: files incoming mail until told to stop.
    ///
    /// The IMAP and Submission listeners are not built yet; this runs ingest only.
    Serve,
    /// Reads the configuration, reports what it selects, and exits.
    Check,
    /// Manages accounts.
    #[command(subcommand)]
    Account(AccountCommand),
    /// Manages app passwords.
    #[command(subcommand)]
    Password(PasswordCommand),
    /// Manages the addresses accounts receive mail for.
    #[command(subcommand)]
    Address(AddressCommand),
}

fn check(cli: &Cli, config: &Config, backends: &Backends) {
    let limits = backends.outbound.limits();
    println!("configuration ok: {}", cli.config.display());
    println!("  host name   {}", config.server.hostname);
    println!("  imap        {} (implicit TLS)", config.imap.listen);
    println!("  submission  {} (implicit TLS)", config.submission.listen);
    for (what, path) in [("certificate", &config.tls.certificate), ("private key", &config.tls.private_key)] {
        let state = if path.is_file() { "found" } else { "MISSING" };
        println!("  {what:<11} {} ({state})", path.display());
    }
    println!("  metadata    {:?}", config.metadata);
    println!("  blobs       {:?}", config.blobs);
    println!("  inbound     {:?}", config.inbound);
    println!("  outbound    {:?}, up to {} bytes per message", config.outbound, limits.max_message_size);
}

/// Completes when the process is asked to stop: Ctrl-C anywhere, and the termination signal a
/// service manager sends on Unix.
async fn stop_requested() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Without a way to hear the signal, run until killed.
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                () = interrupt => {}
                _ = terminate.recv() => {}
            },
            Err(_) => interrupt.await,
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
}

async fn serve(config: &Config, backends: Backends) {
    // Lines for a person at a terminal, JSON for whatever collects the output of a service.
    let logs = tracing_subscriber::fmt().with_writer(std::io::stdout);
    if std::io::stdout().is_terminal() {
        logs.init()
    } else {
        logs.json().init()
    }

    tracing::info!(
        hostname = %config.server.hostname,
        metadata = ?config.metadata,
        blobs = ?config.blobs,
        inbound = ?config.inbound,
        "ingest started; the IMAP and Submission listeners are not built yet"
    );
    let ingest = Ingest::new(backends.metadata, backends.blobs, backends.inbound, backends.changes);
    ingest.run(stop_requested()).await;
    tracing::info!("stopped");
}

/// An error with everything that led to it, on one line.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Exit codes: 0 done, 1 the command was refused, 2 bad configuration, 3 a backend failed.
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::load(&cli.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("epistel: {}", chain(&error));
            return ExitCode::from(2);
        }
    };
    let backends = Backends::build(&config).await;
    let command = match cli.command {
        Command::Check => {
            check(&cli, &config, &backends);
            return ExitCode::SUCCESS;
        }
        Command::Serve => {
            serve(&config, backends).await;
            return ExitCode::SUCCESS;
        }
        Command::Account(command) => AdminCommand::Account(command),
        Command::Password(command) => AdminCommand::Password(command),
        Command::Address(command) => AdminCommand::Address(command),
    };
    match admin::run(&backends, command).await {
        Ok(lines) => {
            lines.iter().for_each(|line| println!("{line}"));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("epistel: {}", chain(&error));
            ExitCode::from(if matches!(error, AdminError::Backend(_)) { 3 } else { 1 })
        }
    }
}
