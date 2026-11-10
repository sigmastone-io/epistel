//! The Epistel mail server.

mod backends;
mod config;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::load(&cli.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("epistel: {error}");
            return ExitCode::from(2);
        }
    };
    match cli.command {
        Command::Check => {
            let limits = Backends::build(&config).outbound.limits();
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
            ExitCode::SUCCESS
        }
    }
}
