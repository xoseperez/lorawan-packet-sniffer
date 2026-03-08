use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::metadata::{Direction, MType};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Top-level configuration
// ---------------------------------------------------------------------------

#[derive(Default, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Configuration {
    pub sniffer: Sniffer,
    pub gateway: Gateway,
    pub filters: Vec<FilterConfig>,
}

// ---------------------------------------------------------------------------
// [sniffer]
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Sniffer {
    pub log_level: String,
    pub log_to_syslog: bool,
    pub api: Api,
    pub database: Database,
}

impl Default for Sniffer {
    fn default() -> Self {
        Sniffer {
            log_level: "INFO".to_string(),
            log_to_syslog: false,
            api: Api::default(),
            database: Database::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Api {
    pub bind: String,
    pub cors_origins: Vec<String>,
}

impl Default for Api {
    fn default() -> Self {
        Api {
            bind: "0.0.0.0:8888".to_string(),
            cors_origins: vec![],
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Database {
    pub path: String,
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
}

impl Default for Database {
    fn default() -> Self {
        Database {
            path: "/var/lib/lorawan-sniffer/sniffer.db".to_string(),
            retention: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

// ---------------------------------------------------------------------------
// [gateway]
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Udp,
    Concentratord,
}

impl Default for Protocol {
    fn default() -> Self {
        Protocol::Udp
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Gateway {
    pub protocol: Protocol,
    pub udp: UdpConfig,
    pub concentratord: ConcentratordConfig,
}

// --- UDP ---

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct UdpConfig {
    /// Gateway-facing side: the sniffer binds here and the gateway connects.
    pub downstream: UdpDownstream,
    /// LNS-facing side: the sniffer connects to the real network server here.
    pub upstream: UdpUpstream,
}

/// Gateway-facing UDP configuration (sniffer listens, gateway sends here).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct UdpDownstream {
    pub bind: String,
}

impl Default for UdpDownstream {
    fn default() -> Self {
        UdpDownstream {
            bind: "0.0.0.0:1700".to_string(),
        }
    }
}

/// LNS-facing UDP configuration (sniffer forwards packets to this server).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct UdpUpstream {
    pub server: String,
    pub port: u16,
}

impl Default for UdpUpstream {
    fn default() -> Self {
        UdpUpstream {
            server: "localhost".to_string(),
            port: 1700,
        }
    }
}

// --- Concentratord ---

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct ConcentratordConfig {
    /// Gateway-facing side: the sniffer connects to the concentratord sockets.
    pub downstream: ConcentratordDownstream,
    /// LNS-facing side: the sniffer binds sockets that the gateway bridge connects to.
    pub upstream: ConcentratordUpstream,
}

/// Gateway-facing Concentratord configuration (connects to concentratord).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ConcentratordDownstream {
    pub event_url: String,
    pub command_url: String,
}

impl Default for ConcentratordDownstream {
    fn default() -> Self {
        ConcentratordDownstream {
            event_url: "ipc:///tmp/concentratord_event".to_string(),
            command_url: "ipc:///tmp/concentratord_command".to_string(),
        }
    }
}

/// LNS-facing Concentratord configuration (exposes sockets to the gateway bridge).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct ConcentratordUpstream {
    pub event_bind: String,
    pub command_bind: String,
}

impl Default for ConcentratordUpstream {
    fn default() -> Self {
        ConcentratordUpstream {
            event_bind: "ipc:///tmp/sniffer_event".to_string(),
            command_bind: "ipc:///tmp/sniffer_command".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// [[filters]]
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct FilterConfig {
    pub name: String,
    pub enabled: bool,
    /// `None` matches both directions.
    pub direction: Option<Direction>,
    pub suppress_ack: bool,
    /// Empty list matches any MType.
    pub mtype: Vec<MType>,
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load and merge one or more TOML configuration files.
///
/// Environment variables are expanded in the form `$VAR_NAME` before
/// TOML parsing (same pattern as chirpstack-concentratord).
pub fn get(filenames: Vec<String>) -> Result<Configuration> {
    let mut content = String::new();

    for path in &filenames {
        content.push_str(
            &fs::read_to_string(path)
                .with_context(|| format!("failed to read config file '{}'", path))?,
        );
    }

    // Expand $ENV_VAR references.
    for (k, v) in env::vars() {
        content = content.replace(&format!("${}", k), &v);
    }

    if content.is_empty() {
        return Ok(Configuration::default());
    }

    toml::from_str(&content).context("failed to parse configuration file")
}
