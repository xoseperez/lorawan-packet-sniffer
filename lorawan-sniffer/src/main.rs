#[macro_use]
extern crate log;
extern crate anyhow;
extern crate simple_logger;
extern crate syslog;

use std::process;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use simple_logger::SimpleLogger;
use syslog::{BasicLogger, Facility, Formatter3164};

mod api;
mod cmd;
mod config;
mod core;
mod mqtt;
mod proxy;

#[derive(Parser)]
#[command(
    name = "lorawan-sniffer",
    author,
    version,
    about = "LoRaWAN Packet Sniffer v2 – transparent MITM proxy for LoRaWAN gateways"
)]
struct Cli {
    /// Path to configuration file (can be repeated to merge multiple files).
    #[arg(short, long, value_name = "FILE")]
    config: Vec<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Print an annotated configuration template and exit.
    Configfile {},
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config = config::get(cli.config.clone()).unwrap_or_else(|e| {
        eprintln!("Configuration error: {:#}", e);
        process::exit(1);
    });

    if let Some(Commands::Configfile {}) = &cli.command {
        cmd::configfile::run(&config);
        process::exit(0);
    }

    if let Err(e) = config::validate(&config) {
        eprintln!("Configuration error: {:#}", e);
        process::exit(1);
    }

    // -----------------------------------------------------------------------
    // Logging setup
    // -----------------------------------------------------------------------
    if config.sniffer.log_to_syslog {
        let formatter = Formatter3164 {
            facility: Facility::LOG_USER,
            hostname: None,
            process: "lorawan-sniffer".into(),
            pid: process::id(),
        };
        let logger = syslog::unix(formatter).expect("could not connect to syslog");
        log::set_boxed_logger(Box::new(BasicLogger::new(logger)))
            .map(|()| {
                log::set_max_level(
                    log::Level::from_str(&config.sniffer.log_level)
                        .unwrap()
                        .to_level_filter(),
                )
            })
            .unwrap();
    } else {
        SimpleLogger::new()
            .with_utc_timestamps()
            .with_level(
                log::Level::from_str(&config.sniffer.log_level)
                    .unwrap()
                    .to_level_filter(),
            )
            .init()
            .unwrap();
    }

    // -----------------------------------------------------------------------
    // Run
    // -----------------------------------------------------------------------
    if let Err(e) = cmd::root::run(&config).await {
        error!("{:#}", e);
        process::exit(1);
    }
}
