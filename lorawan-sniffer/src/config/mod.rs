use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::metadata::{Direction, MType};
use crate::mqtt::topic::TopicTemplate;

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
    pub mqtt: MqttConfig,
}

impl Default for Sniffer {
    fn default() -> Self {
        Sniffer {
            log_level: "INFO".to_string(),
            log_to_syslog: false,
            api: Api::default(),
            database: Database::default(),
            mqtt: MqttConfig::default(),
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

// --- MQTT ---

/// Settings for publishing captured records to an MQTT broker.
///
/// Deliberately not `Debug`: it holds the broker password and the path of the
/// client key, and a stray `{:?}` must not be able to print them.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct MqttConfig {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
    /// Connect over TLS.
    pub tls: bool,
    /// PEM file with the CA certificate(s) that must have issued the broker's
    /// certificate.  Empty uses the bundled public CA roots.
    pub ca_file: String,
    /// PEM client certificate for mutual TLS; needs `client_key_file`.
    pub client_cert_file: String,
    /// PEM client private key for mutual TLS; needs `client_cert_file`.
    pub client_key_file: String,
    pub client_id: String,
    pub username: String,
    pub password: String,
    /// Topic template; see `mqtt::topic` for the placeholders.
    pub topic: String,
    /// Delivery guarantee requested from the broker: 0, 1 or 2.
    pub qos: u8,
}

impl Default for MqttConfig {
    fn default() -> Self {
        MqttConfig {
            enabled: false,
            server: "localhost".to_string(),
            port: 1883,
            tls: false,
            ca_file: String::new(),
            client_cert_file: String::new(),
            client_key_file: String::new(),
            client_id: "lorawan-sniffer".to_string(),
            username: String::new(),
            password: String::new(),
            topic: "lorawan-sniffer/{gateway_eui}/{direction}/{mtype}".to_string(),
            qos: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// [gateway]
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Udp,
    Concentratord,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Gateway {
    pub protocol: Protocol,
    pub udp: UdpConfig,
    pub concentratord: ConcentratordConfig,
}

// --- UDP ---

/// What the UDP side of the sniffer does with the traffic it receives.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UdpMode {
    /// Relay gateway traffic to the LNS and back, applying the filter rules.
    #[default]
    Proxy,
    /// Only record what gateways send; no LNS is involved and nothing is
    /// forwarded or blocked.
    Listen,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct UdpConfig {
    /// `"proxy"` relays to the LNS; `"listen"` only records.
    pub mode: UdpMode,
    /// Listen mode only: answer PUSH_DATA with PUSH_ACK and PULL_DATA with
    /// PULL_ACK, so gateways consider their server alive.
    pub listen_acks: bool,
    /// How long a gateway may stay silent before the sniffer forgets it (and,
    /// in proxy mode, closes its LNS socket).
    #[serde(with = "humantime_serde")]
    pub session_timeout: Duration,
    /// Most gateways served at once; packets from further gateways are
    /// ignored.  Must be at least 1.
    pub max_gateways: usize,
    /// Gateway-facing side: the sniffer binds here and the gateway connects.
    pub downstream: UdpDownstream,
    /// LNS-facing side: the sniffer connects to the real network server here.
    pub upstream: UdpUpstream,
}

impl Default for UdpConfig {
    fn default() -> Self {
        UdpConfig {
            mode: UdpMode::default(),
            listen_acks: true,
            session_timeout: Duration::from_secs(300),
            max_gateways: 100,
            downstream: UdpDownstream::default(),
            upstream: UdpUpstream::default(),
        }
    }
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
    /// Gateway EUIs the rule applies to, as 16 hex digits optionally
    /// separated by `:` or `-`.  Empty list applies to every gateway.
    pub gateways: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load and merge one or more TOML configuration files.
///
/// Environment variables are expanded in the form `$VAR_NAME` before
/// TOML parsing (same pattern as chirpstack-concentratord); see
/// [`expand_env`] for exactly what that does to the values.
pub fn get(filenames: Vec<String>) -> Result<Configuration> {
    let mut content = String::new();

    for path in &filenames {
        content.push_str(
            &fs::read_to_string(path)
                .with_context(|| format!("failed to read config file '{}'", path))?,
        );
    }

    let content = expand_env(&content, |name| env::var(name).ok());

    if content.is_empty() {
        return Ok(Configuration::default());
    }

    toml::from_str(&content).context("failed to parse configuration file")
}

/// Replace `$NAME` references in `content` with the values `lookup` returns.
///
/// One left-to-right pass: a reference is `$` followed by the longest run of
/// `[A-Za-z_][A-Za-z0-9_]*`, so `$X_CLIENT_ID` is looked up as one name and
/// never mistaken for `$X_CLIENT` followed by `_ID`.  A name `lookup` does not
/// know, and a `$` that starts no name, stay exactly as written.
///
/// Substituted values are escaped for a TOML basic string (backslash and
/// double quote, plus control characters, which a basic string may not hold
/// raw), because every reference in the shipped configurations sits inside
/// double quotes: without that, a password containing a quote would end the
/// string early and could even smuggle in further settings.  The inserted text
/// is never scanned again, so a value that itself contains `$OTHER` keeps it
/// literally instead of pulling in a second variable.
///
/// References that are not inside a basic string (numbers, booleans) work as
/// before, since their values contain nothing that needs escaping.  A
/// reference inside a single-quoted (literal) TOML string would show the
/// escapes as written; no shipped configuration does that.
fn expand_env(content: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];

        let name_len = env_name_len(after);
        let name = &after[..name_len];
        match (name_len, lookup(name)) {
            (0, _) | (_, None) => {
                out.push('$');
                out.push_str(name);
            }
            (_, Some(value)) => push_toml_escaped(&mut out, &value),
        }
        rest = &after[name_len..];
    }

    out.push_str(rest);
    out
}

/// Length in bytes of the environment variable name at the start of `s`
/// (zero when `s` does not start with one).  Names are ASCII, so bytes and
/// characters coincide.
fn env_name_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    match bytes.first() {
        Some(b) if b.is_ascii_alphabetic() || *b == b'_' => {}
        _ => return 0,
    }
    bytes
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
        .unwrap_or(bytes.len())
}

/// Append `value` to `out`, escaped so it reads back unchanged inside a TOML
/// basic string.
pub(crate) fn push_toml_escaped(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Check settings that are only meaningful together or that would otherwise
/// fail late and quietly.  Run once after loading, before anything starts.
///
/// Only the MQTT section is checked, and only when MQTT is enabled: a disabled
/// section is never used, so a leftover typo in it must not stop the sniffer.
pub fn validate(cfg: &Configuration) -> Result<()> {
    let mqtt = &cfg.sniffer.mqtt;
    if !mqtt.enabled {
        return Ok(());
    }

    if mqtt.qos > 2 {
        anyhow::bail!("[sniffer.mqtt] qos must be 0, 1 or 2, not {}", mqtt.qos);
    }

    mqtt.topic
        .parse::<TopicTemplate>()
        .with_context(|| format!("[sniffer.mqtt] invalid topic '{}'", mqtt.topic))?;

    // A certificate without its key (or the reverse) cannot authenticate, and
    // silently falling back to no client certificate would hide the mistake
    // until the broker refuses the connection.
    if mqtt.client_cert_file.is_empty() != mqtt.client_key_file.is_empty() {
        anyhow::bail!("[sniffer.mqtt] client_cert_file and client_key_file must be set together");
    }

    // Without TLS none of the files is used, so they are not required to exist.
    if mqtt.tls {
        for (key, path) in [
            ("ca_file", &mqtt.ca_file),
            ("client_cert_file", &mqtt.client_cert_file),
            ("client_key_file", &mqtt.client_key_file),
        ] {
            if !path.is_empty() {
                fs::File::open(path)
                    .with_context(|| format!("[sniffer.mqtt] cannot read {} '{}'", key, path))?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::PathBuf;

    fn parse(toml_text: &str) -> Configuration {
        toml::from_str(toml_text).unwrap()
    }

    fn lookup_in<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = vars.iter().copied().collect();
        move |name| map.get(name).map(|v| v.to_string())
    }

    /// A file in the temp directory that is removed when dropped.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "lorawan-sniffer-config-test-{}-{}",
                std::process::id(),
                name
            ));
            fs::write(&path, b"placeholder").unwrap();
            TempFile(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    // --- [sniffer.mqtt] ---

    #[test]
    fn mqtt_section_is_optional() {
        let cfg = parse(
            r#"
            [sniffer]
              log_level = "DEBUG"
            "#,
        );

        let mqtt = &cfg.sniffer.mqtt;
        assert!(!mqtt.enabled);
        assert_eq!(mqtt.server, "localhost");
        assert_eq!(mqtt.port, 1883);
        assert!(!mqtt.tls);
        assert_eq!(mqtt.ca_file, "");
        assert_eq!(mqtt.client_cert_file, "");
        assert_eq!(mqtt.client_key_file, "");
        assert_eq!(mqtt.client_id, "lorawan-sniffer");
        assert_eq!(mqtt.username, "");
        assert_eq!(mqtt.password, "");
        assert_eq!(
            mqtt.topic,
            "lorawan-sniffer/{gateway_eui}/{direction}/{mtype}"
        );
        assert_eq!(mqtt.qos, 0);

        // An empty file is the same configuration as a missing section.
        assert!(!parse("").sniffer.mqtt.enabled);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn mqtt_section_parses() {
        let cfg = parse(
            r#"
            [sniffer.mqtt]
              enabled = true
              server = "broker.example.com"
              port = 8883
              tls = true
              ca_file = "/etc/ca.pem"
              client_cert_file = "/etc/client.pem"
              client_key_file = "/etc/client.key"
              client_id = "sniffer-1"
              username = "alice"
              password = "s3cret"
              topic = "lora/{gateway_eui}/{dev_addr}"
              qos = 1
            "#,
        );

        let mqtt = &cfg.sniffer.mqtt;
        assert!(mqtt.enabled);
        assert_eq!(mqtt.server, "broker.example.com");
        assert_eq!(mqtt.port, 8883);
        assert!(mqtt.tls);
        assert_eq!(mqtt.ca_file, "/etc/ca.pem");
        assert_eq!(mqtt.client_cert_file, "/etc/client.pem");
        assert_eq!(mqtt.client_key_file, "/etc/client.key");
        assert_eq!(mqtt.client_id, "sniffer-1");
        assert_eq!(mqtt.username, "alice");
        assert_eq!(mqtt.password, "s3cret");
        assert_eq!(mqtt.topic, "lora/{gateway_eui}/{dev_addr}");
        assert_eq!(mqtt.qos, 1);
    }

    #[test]
    fn validate_rejects_bad_qos() {
        for qos in [0, 1, 2] {
            let cfg = parse(&format!("[sniffer.mqtt]\nenabled = true\nqos = {qos}\n"));
            assert!(validate(&cfg).is_ok(), "qos {qos}");
        }

        let cfg = parse("[sniffer.mqtt]\nenabled = true\nqos = 3\n");
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("qos"), "{err}");
    }

    #[test]
    fn validate_rejects_empty_topic() {
        let cfg = parse("[sniffer.mqtt]\nenabled = true\ntopic = \"\"\n");
        let err = format!("{:#}", validate(&cfg).unwrap_err());
        assert!(err.contains("topic"), "{err}");
    }

    #[test]
    fn validate_rejects_bad_topic_templates() {
        for topic in ["a/{nope}", "a/{mtype", "a/+/b", "a/#"] {
            let cfg = parse(&format!(
                "[sniffer.mqtt]\nenabled = true\ntopic = \"{topic}\"\n"
            ));
            assert!(validate(&cfg).is_err(), "{topic}");
        }
    }

    #[test]
    fn validate_rejects_client_cert_without_key() {
        let cfg = parse("[sniffer.mqtt]\nenabled = true\nclient_cert_file = \"/c.pem\"\n");
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("client_cert_file"), "{err}");

        let cfg = parse("[sniffer.mqtt]\nenabled = true\nclient_key_file = \"/c.key\"\n");
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_requires_tls_files_to_be_readable() {
        let file = TempFile::new("ca");
        let cfg = parse(&format!(
            "[sniffer.mqtt]\nenabled = true\ntls = true\nca_file = '{}'\n",
            file.path()
        ));
        assert!(validate(&cfg).is_ok());

        let cfg =
            parse("[sniffer.mqtt]\nenabled = true\ntls = true\nca_file = '/nonexistent/ca.pem'\n");
        let err = format!("{:#}", validate(&cfg).unwrap_err());
        assert!(
            err.contains("ca_file") && err.contains("/nonexistent/ca.pem"),
            "{err}"
        );

        // The same setting is ignored when TLS is off.
        let cfg =
            parse("[sniffer.mqtt]\nenabled = true\ntls = false\nca_file = '/nonexistent/ca.pem'\n");
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn validate_ignores_a_disabled_mqtt_section() {
        let cfg = parse("[sniffer.mqtt]\nenabled = false\nqos = 9\ntopic = \"\"\n");
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn validate_errors_do_not_contain_the_password() {
        let cfg = parse(
            "[sniffer.mqtt]\nenabled = true\nqos = 7\npassword = \"canary-7f3e\"\n\
             client_cert_file = \"/c.pem\"\n",
        );
        let err = format!("{:#}", validate(&cfg).unwrap_err());
        assert!(!err.contains("canary-7f3e"), "{err}");
    }

    // --- environment expansion ---

    #[test]
    fn env_values_with_quotes_and_backslashes_parse() {
        let tricky = r#"a"b\c$HOME"#;
        let vars = [("SNIFFER_MQTT_PASSWORD", tricky), ("HOME", "/root")];
        let expanded = expand_env(
            "[sniffer.mqtt]\npassword = \"$SNIFFER_MQTT_PASSWORD\"\n",
            lookup_in(&vars),
        );

        let cfg = parse(&expanded);

        // The value arrives exactly as set, `$HOME` included: it is data, not
        // another reference.
        assert_eq!(cfg.sniffer.mqtt.password, tricky);
    }

    #[test]
    fn env_values_with_control_characters_parse() {
        let value = "line1\nline2\ttab\r\u{7f}";
        let vars = [("V", value)];

        let cfg = parse(&expand_env(
            "[sniffer.mqtt]\npassword = \"$V\"\n",
            lookup_in(&vars),
        ));

        assert_eq!(cfg.sniffer.mqtt.password, value);
    }

    #[test]
    fn env_values_cannot_inject_settings() {
        // A value that closes the string and starts a new line must stay a
        // single string value.
        let evil = "x\"\nenabled = true\n[sniffer.api]\nbind = \"evil";
        let vars = [("P", evil)];

        let cfg = parse(&expand_env(
            "[sniffer.mqtt]\npassword = \"$P\"\n",
            lookup_in(&vars),
        ));

        assert_eq!(cfg.sniffer.mqtt.password, evil);
        assert!(!cfg.sniffer.mqtt.enabled);
        assert_eq!(cfg.sniffer.api.bind, "0.0.0.0:8888");
    }

    #[test]
    fn env_values_are_not_reexpanded() {
        let vars = [("A", "$B"), ("B", "second"), ("C", "$C")];

        assert_eq!(
            expand_env("x=$A y=$B z=$C", lookup_in(&vars)),
            "x=$B y=second z=$C"
        );
        // The order the references appear in must not matter either.
        assert_eq!(expand_env("$B $A", lookup_in(&vars)), "second $B");
    }

    #[test]
    fn prefix_names_do_not_collide() {
        let vars = [("X_CLIENT", "short"), ("X_CLIENT_ID", "long")];

        assert_eq!(
            expand_env("$X_CLIENT_ID|$X_CLIENT|$X_CLIENT_", lookup_in(&vars)),
            // `$X_CLIENT_` is a different (unknown) name, so it is left alone
            // rather than read as `$X_CLIENT` plus an underscore.
            "long|short|$X_CLIENT_"
        );
        assert_eq!(
            expand_env("$X_CLIENT_ID", lookup_in(&[("X_CLIENT", "short")])),
            "$X_CLIENT_ID"
        );
    }

    #[test]
    fn unknown_names_left_untouched() {
        let vars = [("KNOWN", "yes")];

        assert_eq!(
            expand_env(
                "$KNOWN $UNKNOWN ${KNOWN} $ $1 $$KNOWN cost: 5$",
                lookup_in(&vars)
            ),
            "yes $UNKNOWN ${KNOWN} $ $1 $yes cost: 5$"
        );
    }

    #[test]
    fn numeric_placeholders_still_work() {
        let vars = [("PORT", "8883"), ("TLS", "true"), ("QOS", "1")];
        let expanded = expand_env(
            "[sniffer.mqtt]\nport = $PORT\ntls = $TLS\nqos = $QOS\n",
            lookup_in(&vars),
        );

        let cfg = parse(&expanded);

        assert_eq!(cfg.sniffer.mqtt.port, 8883);
        assert!(cfg.sniffer.mqtt.tls);
        assert_eq!(cfg.sniffer.mqtt.qos, 1);
    }

    #[test]
    fn expansion_keeps_non_ascii_text() {
        let vars = [("V", "café")];

        assert_eq!(expand_env("é $V ü$V", lookup_in(&vars)), "é café ücafé");
    }

    #[test]
    fn expansion_of_nothing_is_nothing() {
        assert_eq!(expand_env("", |_| None), "");
        assert_eq!(expand_env("no references", |_| None), "no references");
    }
}
