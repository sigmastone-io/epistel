//! The Epistel mail server.

mod admin;
mod backends;
mod config;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::admin::{AccountCommand, AddressCommand, AdminCommand, AdminError, PasswordCommand};
use crate::backends::Backends;
use crate::config::Config;

#[derive(Parser)]
#[command(name = "epistel", version, about)]
struct Cli {
    /// The configuration file.
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/epistel/epistel.toml",
        env = "EPISTEL_CONFIG"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    for (what, path) in [
        ("certificate", &config.tls.certificate),
        ("private key", &config.tls.private_key),
    ] {
        let state = if path.is_file() { "found" } else { "MISSING" };
        println!("  {what:<11} {} ({state})", path.display());
    }
    println!("  metadata    {:?}", config.metadata);
    println!("  blobs       {:?}", config.blobs);
    println!("  inbound     {:?}", config.inbound);
    println!(
        "  outbound    {:?}, up to {} bytes per message",
        config.outbound, limits.max_message_size
    );
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
