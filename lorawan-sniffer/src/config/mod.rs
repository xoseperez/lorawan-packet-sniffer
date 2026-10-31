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
    /// Problems found while loading that do not stop the sniffer, to be
    /// logged once logging is set up.
    #[serde(skip)]
    pub load_warnings: Vec<String>,
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
    /// Host names the WebUI and API answer to, besides IP addresses and
    /// `localhost`, which always work.  Written as a TOML array or as one
    /// comma-separated string (see [`host_list`]); `["*"]` turns the check
    /// off.  Checked by [`validate`].
    #[serde(deserialize_with = "host_list")]
    pub allowed_hosts: Vec<String>,
}

impl Default for Api {
    fn default() -> Self {
        Api {
            bind: "0.0.0.0:8888".to_string(),
            cors_origins: vec![],
            allowed_hosts: vec![],
        }
    }
}

/// Deserialize a list of host names from a TOML array of strings or from one
/// string of names separated by commas.
///
/// The string form exists for Docker, where the setting is written as
/// `allowed_hosts = "$SNIFFER_API_ALLOWED_HOSTS"` and a `$VAR` can only expand
/// to text.  Both forms get the same treatment: every item is split on commas,
/// trimmed, and dropped when empty, so an unset variable (an empty string) and
/// a trailing comma both mean "nothing more to list" instead of becoming an
/// empty name.
fn host_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;

    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an array of host names or one string of comma-separated host names")
        }

        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
            Ok(split_hosts(text))
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut hosts = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                hosts.extend(split_hosts(&item));
            }
            Ok(hosts)
        }
    }

    deserializer.deserialize_any(Visitor)
}

fn split_hosts(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .collect()
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

    let mut config: Configuration =
        toml::from_str(&content).context("failed to parse configuration file")?;
    config.load_warnings = unexpanded_references(&content)
        .into_iter()
        .map(|key| {
            format!(
                "{key} refers to an environment variable that is not set, so its text \
                 is used as it is; define the variable (for the systemd service, in \
                 /etc/lorawan-sniffer/sniffer.env) or write the value itself"
            )
        })
        .collect();
    Ok(config)
}

/// Keys whose whole value still reads `$NAME` after expansion.
///
/// That is a reference to an environment variable that is not set, and the
/// sniffer would quietly use the text itself, for example sending
/// "$MQTT_PASSWORD" as the password.  Only the keys are returned, never the
/// values: a password can legitimately start with `$`, and it must not end up
/// in the logs.
fn unexpanded_references(content: &str) -> Vec<String> {
    fn walk(value: &toml::Value, path: &str, keys: &mut Vec<String>) {
        match value {
            toml::Value::String(text) => {
                let name = text.strip_prefix('$').unwrap_or_default();
                if !name.is_empty() && env_name_len(name) == name.len() {
                    keys.push(path.to_string());
                }
            }
            toml::Value::Table(table) => {
                for (key, value) in table {
                    let path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(value, &path, keys);
                }
            }
            toml::Value::Array(items) => {
                for (i, value) in items.iter().enumerate() {
                    walk(value, &format!("{path}[{i}]"), keys);
                }
            }
            _ => {}
        }
    }

    let mut keys = Vec::new();
    if let Ok(table) = content.parse::<toml::Table>() {
        walk(&toml::Value::Table(table), "", &mut keys);
    }
    keys
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
/// The API's `allowed_hosts` is always checked: it guards the WebUI whether or
/// not anything else is enabled.  The MQTT section is checked only when MQTT
/// is enabled: a disabled section is never used, so a leftover typo in it must
/// not stop the sniffer.
pub fn validate(cfg: &Configuration) -> Result<()> {
    // This has to stay ahead of the early return below, which skips every
    // check after it when MQTT is off.
    validate_allowed_hosts(&cfg.sniffer.api.allowed_hosts)?;

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

/// Whether `byte` may appear in a host name: ASCII letters and digits, `-`,
/// `_` and `.`.  The Host check uses the same rule, so that it only ever
/// suggests a name this validation accepts.
pub(crate) fn is_host_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// Each entry of `allowed_hosts` must be a bare host name, or the list must be
/// exactly `["*"]`.
///
/// A wrong entry would not fail anywhere else: a name that carries a port or
/// a scheme can never equal the host of a request, so the WebUI would simply
/// stay unreachable by that name, with only a 403 to show for it.  Failing at
/// startup, with the reason, is kinder than that.
fn validate_allowed_hosts(hosts: &[String]) -> Result<()> {
    const SETTING: &str = "[sniffer.api] allowed_hosts";

    if hosts.len() > 1 && hosts.iter().any(|host| host == "*") {
        anyhow::bail!(
            "{SETTING}: \"*\" turns the host check off, so it must be the only entry; \
             remove the other names or remove \"*\""
        );
    }

    for host in hosts {
        if host == "*" {
            continue;
        }
        if host.is_empty() {
            anyhow::bail!("{SETTING}: an entry is empty");
        }
        if host.contains('*') {
            anyhow::bail!(
                "{SETTING}: '{host}' uses a wildcard, which is not supported; list each \
                 host name, or use [\"*\"] alone to turn the host check off (unsafe)"
            );
        }
        if host.contains(char::is_whitespace) {
            anyhow::bail!("{SETTING}: '{host}' contains whitespace; list bare host names");
        }
        if host.contains("://") {
            anyhow::bail!(
                "{SETTING}: '{host}' contains a scheme; list the bare host name, without \
                 \"http://\" or \"https://\""
            );
        }
        if host.contains('/') {
            anyhow::bail!(
                "{SETTING}: '{host}' contains a '/'; list the bare host name, without a path"
            );
        }
        if host.contains('@') {
            anyhow::bail!(
                "{SETTING}: '{host}' contains an '@'; list the bare host name, without \
                 user information"
            );
        }
        if host.contains(':') {
            anyhow::bail!(
                "{SETTING}: '{host}' contains a ':'; list the bare host name, without a port. \
                 IP addresses (IPv4 and IPv6, with any port) are always allowed and never \
                 need an entry"
            );
        }
        // Browsers send an internationalised name in its ASCII (punycode)
        // form, so an entry written with other characters could never match.
        if host.starts_with('$') {
            // The environment expansion leaves a reference to an unset
            // variable as written, and startup stops here before the
            // warning about it can be logged.
            anyhow::bail!(
                "{SETTING}: '{host}' looks like a reference to an environment variable \
                 that is not set; define the variable or list the host names themselves"
            );
        }
        if !host.bytes().all(is_host_name_byte) {
            anyhow::bail!(
                "{SETTING}: '{host}' is not a valid host name; use letters, digits, '-', '_' \
                 and '.' only (for an internationalised name, its ASCII \"xn--\" form)"
            );
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

    // --- [sniffer.api] allowed_hosts ---

    fn allowed_hosts_of(toml_text: &str) -> Vec<String> {
        parse(toml_text).sniffer.api.allowed_hosts
    }

    /// The error `validate` gives for a configuration, which must come even
    /// though MQTT is off (the default).
    fn allowed_hosts_error(toml_text: &str) -> String {
        let cfg = parse(toml_text);
        assert!(!cfg.sniffer.mqtt.enabled);
        format!("{:#}", validate(&cfg).unwrap_err())
    }

    #[test]
    fn allowed_hosts_defaults_to_nothing() {
        assert!(Configuration::default()
            .sniffer
            .api
            .allowed_hosts
            .is_empty());
        assert!(allowed_hosts_of("").is_empty());
        assert!(allowed_hosts_of("[sniffer.api]\nbind = \"127.0.0.1:1\"\n").is_empty());
        assert!(validate(&Configuration::default()).is_ok());
    }

    #[test]
    fn allowed_hosts_accepts_an_array() {
        let cfg = parse(
            "[sniffer.api]\nallowed_hosts = [\"sniffer.example.com\", \"lab-pc\", \"Other.Example\"]\n",
        );

        assert_eq!(
            cfg.sniffer.api.allowed_hosts,
            ["sniffer.example.com", "lab-pc", "Other.Example"]
        );
        assert!(validate(&cfg).is_ok());
        assert!(allowed_hosts_of("[sniffer.api]\nallowed_hosts = []\n").is_empty());
    }

    #[test]
    fn allowed_hosts_accepts_a_comma_separated_string() {
        assert_eq!(
            allowed_hosts_of("[sniffer.api]\nallowed_hosts = \"sniffer.example.com\"\n"),
            ["sniffer.example.com"]
        );
        // Spaces around the commas, and empty pieces, are not part of any name.
        assert_eq!(
            allowed_hosts_of(
                "[sniffer.api]\nallowed_hosts = \"  a.example ,b.example,, c.example ,\"\n"
            ),
            ["a.example", "b.example", "c.example"]
        );
        let cfg = parse("[sniffer.api]\nallowed_hosts = \"a.example,b.example\"\n");
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn allowed_hosts_empty_string_means_none() {
        // What Docker produces when SNIFFER_API_ALLOWED_HOSTS is left empty.
        for value in ["\"\"", "\"  \"", "\",\"", "[\"\"]", "[]"] {
            let text = format!("[sniffer.api]\nallowed_hosts = {value}\n");
            assert!(allowed_hosts_of(&text).is_empty(), "{value}");
            assert!(validate(&parse(&text)).is_ok(), "{value}");
        }
    }

    #[test]
    fn allowed_hosts_splits_commas_inside_array_items_too() {
        // `["$VAR"]` with a comma-separated value behaves like the string form.
        assert_eq!(
            allowed_hosts_of(
                "[sniffer.api]\nallowed_hosts = [\"a.example, b.example\", \"c.example\"]\n"
            ),
            ["a.example", "b.example", "c.example"]
        );
    }

    #[test]
    fn allowed_hosts_rejects_other_value_types() {
        for value in ["5", "true", "[1, 2]", "{ a = 1 }"] {
            let text = format!("[sniffer.api]\nallowed_hosts = {value}\n");
            let err = toml::from_str::<Configuration>(&text).err().unwrap();
            assert!(err.to_string().contains("allowed_hosts"), "{value}: {err}");
        }
    }

    #[test]
    fn allowed_hosts_read_from_an_environment_variable() {
        let text = "[sniffer.api]\nallowed_hosts=\"$SNIFFER_API_ALLOWED_HOSTS\"\n";

        let list = [("SNIFFER_API_ALLOWED_HOSTS", "a.example, b.example")];
        assert_eq!(
            allowed_hosts_of(&expand_env(text, lookup_in(&list))),
            ["a.example", "b.example"]
        );

        // A variable the entrypoint left empty.
        let empty = [("SNIFFER_API_ALLOWED_HOSTS", "")];
        assert!(allowed_hosts_of(&expand_env(text, lookup_in(&empty))).is_empty());

        // The star arrives as a one-entry list.
        let star = [("SNIFFER_API_ALLOWED_HOSTS", "*")];
        assert_eq!(allowed_hosts_of(&expand_env(text, lookup_in(&star))), ["*"]);
    }

    #[test]
    fn validate_accepts_plain_host_names() {
        for host in [
            "sniffer.example.com",
            "Sniffer.Example.COM",
            "lab-pc",
            "lab_pc",
            "sniffer.local.",
            "xn--bcher-kva.example",
            "192.168.1.5",
        ] {
            let cfg = parse(&format!("[sniffer.api]\nallowed_hosts = [\"{host}\"]\n"));
            assert!(validate(&cfg).is_ok(), "{host}");
        }
    }

    #[test]
    fn validate_accepts_a_lone_star() {
        for text in [
            "[sniffer.api]\nallowed_hosts = [\"*\"]\n",
            "[sniffer.api]\nallowed_hosts = \"*\"\n",
            "[sniffer.api]\nallowed_hosts = \" * ,\"\n",
        ] {
            let cfg = parse(text);
            assert_eq!(cfg.sniffer.api.allowed_hosts, ["*"], "{text}");
            assert!(validate(&cfg).is_ok(), "{text}");
        }
    }

    #[test]
    fn validate_rejects_a_star_next_to_names() {
        for text in [
            "[sniffer.api]\nallowed_hosts = [\"*\", \"a.example\"]\n",
            "[sniffer.api]\nallowed_hosts = [\"a.example\", \"*\"]\n",
            "[sniffer.api]\nallowed_hosts = \"a.example,*\"\n",
            "[sniffer.api]\nallowed_hosts = [\"*\", \"*\"]\n",
        ] {
            let err = allowed_hosts_error(text);
            assert!(
                err.contains("allowed_hosts") && err.contains("only entry"),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_entries_that_are_not_bare_host_names() {
        let cases = [
            ("sniffer.example.com:8888", "port"),
            ("localhost:8888", "port"),
            ("::1", "IP addresses"),
            ("[::1]:8888", "IP addresses"),
            ("http://sniffer.example.com", "scheme"),
            ("https://sniffer.example.com:8443/", "scheme"),
            ("sniffer.example.com/ui", "'/'"),
            ("user@sniffer.example.com", "'@'"),
            ("two words", "whitespace"),
            ("*.example.com", "wildcard"),
            ("sniffer*", "wildcard"),
            ("b\u{fc}cher.example", "valid host name"),
            ("$SNIFFER_NEVER_SET", "environment variable"),
            ("a?b", "valid host name"),
            ("a#b", "valid host name"),
        ];

        for (entry, reason) in cases {
            let text = format!("[sniffer.api]\nallowed_hosts = [\"{entry}\"]\n");
            let err = allowed_hosts_error(&text);
            assert!(
                err.contains("[sniffer.api] allowed_hosts")
                    && err.contains(&format!("'{entry}'"))
                    && err.contains(reason),
                "{entry}: {err}"
            );

            // The same entry in the comma-separated form, next to a good one.
            let text = format!("[sniffer.api]\nallowed_hosts = \"ok.example,{entry}\"\n");
            assert!(validate(&parse(&text)).is_err(), "{entry}");
        }
    }

    #[test]
    fn validate_rejects_bad_hosts_with_or_without_mqtt() {
        for enabled in [false, true] {
            let cfg = parse(&format!(
                "[sniffer.mqtt]\nenabled = {enabled}\n[sniffer.api]\nallowed_hosts = \"host:80\"\n"
            ));
            let err = format!("{:#}", validate(&cfg).unwrap_err());
            assert!(err.contains("allowed_hosts"), "mqtt {enabled}: {err}");
        }
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

    /// The configuration shipped in the release archives must keep loading
    /// as the code changes, or a fresh install would fail to start.
    #[test]
    fn packaged_config_is_valid() {
        let text = include_str!("../../../packaging/sniffer.toml");
        let cfg = parse(&expand_env(text, |_| None));

        validate(&cfg).unwrap();
        assert!(cfg.gateway.protocol == Protocol::Udp);
        assert_eq!(cfg.gateway.udp.mode, UdpMode::Proxy);
        assert_eq!(
            cfg.sniffer.database.path,
            "/var/lib/lorawan-sniffer/sniffer.db"
        );
        assert!(!cfg.sniffer.mqtt.enabled);
        // Only IP addresses and localhost until the operator lists a name.
        assert!(cfg.sniffer.api.allowed_hosts.is_empty());

        // Next to an LNS on the same host: the sniffer must not take the
        // LNS's port, and the upstream must not be the sniffer itself.
        let bind: std::net::SocketAddr = cfg.gateway.udp.downstream.bind.parse().unwrap();
        assert_eq!(bind.port(), 1702);
        assert_eq!(cfg.gateway.udp.upstream.server, "127.0.0.1");
        assert_eq!(cfg.gateway.udp.upstream.port, 1700);
    }

    /// The commented `allowed_hosts` example in each sample file is what an
    /// operator will uncomment, so it has to be valid as written.
    #[test]
    fn sample_allowed_hosts_example_works_once_uncommented() {
        for (name, text) in [
            ("sniffer.toml", include_str!("../../../sniffer.toml")),
            (
                "packaging/sniffer.toml",
                include_str!("../../../packaging/sniffer.toml"),
            ),
        ] {
            let example = "# allowed_hosts = [\"sniffer.example.com\"]";
            assert!(text.contains(example), "{name} has no example");

            let cfg = parse(&text.replace(example, &example[2..]));

            assert_eq!(
                cfg.sniffer.api.allowed_hosts,
                ["sniffer.example.com"],
                "{name}"
            );
            validate(&cfg).unwrap();
        }
    }

    #[test]
    fn unset_reference_is_reported_by_key_only() {
        let text = r#"
[sniffer.mqtt]
  username = "sniffer"
  password = "$LORAWAN_SNIFFER_TEST_NEVER_SET"
[[filters]]
  name = "$ALSO_NEVER_SET_IN_TESTS"
"#;

        assert_eq!(
            unexpanded_references(text),
            vec!["filters[0].name", "sniffer.mqtt.password"]
        );
    }

    #[test]
    fn dollar_inside_a_value_is_not_a_reference() {
        let text = r#"
[sniffer.mqtt]
  password = "pa$word"
  username = "$"
  client_id = "$1abc"
"#;

        assert!(unexpanded_references(text).is_empty());
    }

    #[test]
    fn loading_records_the_warning_without_the_value() {
        let file = TempFile::new("unset-reference.toml");
        fs::write(
            &file.0,
            "[sniffer.mqtt]\npassword = \"$LORAWAN_SNIFFER_TEST_NEVER_SET\"\n",
        )
        .unwrap();

        let cfg = get(vec![file.path().to_string()]).unwrap();

        assert_eq!(cfg.load_warnings.len(), 1);
        assert!(cfg.load_warnings[0].starts_with("sniffer.mqtt.password "));
        assert!(!cfg.load_warnings[0].contains("LORAWAN_SNIFFER_TEST_NEVER_SET"));
    }

    #[test]
    fn packaged_config_loads_without_warnings() {
        let file = TempFile::new("packaged.toml");
        fs::write(&file.0, include_str!("../../../packaging/sniffer.toml")).unwrap();

        assert!(get(vec![file.path().to_string()])
            .unwrap()
            .load_warnings
            .is_empty());
    }

    /// The MQTT password in the packaged file is meant to come from the
    /// service's environment file; check that route works once uncommented.
    #[test]
    fn packaged_config_takes_password_from_environment() {
        let text = include_str!("../../../packaging/sniffer.toml").replace(
            "# password = \"$MQTT_PASSWORD\"",
            "password = \"$MQTT_PASSWORD\"",
        );
        let vars = [("MQTT_PASSWORD", r#"a"b\c$HOME"#)];

        let cfg = parse(&expand_env(&text, lookup_in(&vars)));

        assert_eq!(cfg.sniffer.mqtt.password, r#"a"b\c$HOME"#);
    }
}
