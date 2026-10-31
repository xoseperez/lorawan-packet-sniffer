use handlebars::Handlebars;

use crate::config;

/// The annotated configuration template; `{{ .. }}` marks where a setting of
/// the loaded configuration is filled in.
///
/// The MQTT password has no placeholder on purpose: this output gets saved,
/// copied around and pasted into bug reports, so a secret that came from a
/// configuration file or the environment must never be written into it.
const TEMPLATE: &str = r##"
# LoRaWAN Packet Sniffer configuration.

[sniffer]
  # Log level. Valid options: TRACE, DEBUG, INFO, WARN, ERROR, OFF
  log_level = "{{ sniffer.log_level }}"

  # Log to syslog instead of stdout.
  log_to_syslog = {{ sniffer.log_to_syslog }}

  [sniffer.api]
    # HTTP API / WebSocket bind address.
    bind = "{{ sniffer.api.bind }}"

    # Allowed CORS origins (optional).
    # cors_origins = ["https://example.com"]

    # Host names the WebUI and API answer to. IP addresses and "localhost"
    # always work and need no entry; a request for any other name is refused
    # with 403. This protects against DNS rebinding, where a web page points
    # its own domain name at this machine to read the captured traffic from
    # the visitor's browser. To open the WebUI by name, list the name without
    # a port, for example ["sniffer.example.com"]. Behind a reverse proxy
    # that passes the Host header on, list the public name. ["*"] turns the
    # check off and is unsafe. In Docker, set SNIFFER_API_ALLOWED_HOSTS to a
    # comma-separated list.
    allowed_hosts = [{{#each sniffer.api.allowed_hosts}}"{{this}}"{{#unless @last}}, {{/unless}}{{/each}}]

  [sniffer.database]
    # Path to the SQLite database file.
    path = "{{ sniffer.database.path }}"

    # Message retention period.
    retention = "{{ sniffer.database.retention }}"

  # MQTT publishing (optional): every captured message is also published to
  # the broker as JSON. Best effort: messages that cannot be sent are dropped
  # and counted, never queued without limit.
  [sniffer.mqtt]
    # Publish messages to MQTT.
    enabled = {{ sniffer.mqtt.enabled }}

    # Broker host name or IP address, and port (1883 plain, usually 8883 TLS).
    server = "{{ sniffer.mqtt.server }}"
    port   = {{ sniffer.mqtt.port }}

    # Connect using TLS. The broker certificate is always verified.
    tls = {{ sniffer.mqtt.tls }}

    # TLS only: PEM file with the CA certificate(s) that verify the broker.
    # Empty uses the bundled public CA roots.
    ca_file = "{{ sniffer.mqtt.ca_file }}"

    # TLS only: PEM client certificate and key for brokers requiring mutual
    # TLS. Set both or none.
    client_cert_file = "{{ sniffer.mqtt.client_cert_file }}"
    client_key_file  = "{{ sniffer.mqtt.client_key_file }}"

    # MQTT client identifier.
    client_id = "{{ sniffer.mqtt.client_id }}"

    # Optional credentials. Leave empty for anonymous access.
    username = "{{ sniffer.mqtt.username }}"

    # The password is never written by this command. Set it here, or use an
    # environment variable such as "$MQTT_PASSWORD" as its value.
    # password = ""

    # Topic template. Placeholders: {gateway_eui}, {direction}, {mtype},
    # {status}, {dev_addr}, {dev_eui}.
    topic = "{{ sniffer.mqtt.topic }}"

    # Quality of service: 0 (at most once), 1 or 2.
    qos = {{ sniffer.mqtt.qos }}


[gateway]
  # Gateway protocol: "udp" or "concentratord".
  protocol = "{{ gateway.protocol }}"

  [gateway.udp]
    # Operating mode: "proxy" relays between the gateways and the LNS,
    # "listen" only records what the gateways send (no LNS involved).
    mode = "{{ gateway.udp.mode }}"

    # Listen mode only: answer PUSH_DATA and PULL_DATA with the ACKs the
    # gateways expect, so they consider their server alive.
    listen_acks = {{ gateway.udp.listen_acks }}

    # Release a gateway's session after this much silence.
    session_timeout = "{{ gateway.udp.session_timeout }}"

    # Maximum number of gateways handled at once (at least 1).
    max_gateways = {{ gateway.udp.max_gateways }}

    [gateway.udp.downstream]
      bind = "{{ gateway.udp.downstream.bind }}"

    # Proxy mode only: the LoRaWAN Network Server to forward to.
    [gateway.udp.upstream]
      server = "{{ gateway.udp.upstream.server }}"
      port   = {{ gateway.udp.upstream.port }}

  [gateway.concentratord]
    [gateway.concentratord.downstream]
      event_url   = "{{ gateway.concentratord.downstream.event_url }}"
      command_url = "{{ gateway.concentratord.downstream.command_url }}"

    [gateway.concentratord.upstream]
      event_bind   = "{{ gateway.concentratord.upstream.event_bind }}"
      command_bind = "{{ gateway.concentratord.upstream.command_bind }}"


# Packet filter rules (repeatable). They have no effect in UDP listen mode.
#
# [[filters]]
#   name        = "Block confirmed uplinks"
#   enabled     = true
#   direction   = "up"
#   suppress_ack = false
#   mtype       = ["ConfirmedDataUp"]
#   # Optional: only apply to these gateways (EUIs, with or without ':' or
#   # '-'). Omit to apply to every gateway.
#   gateways    = ["AA:BB:CC:DD:00:11:22:33"]
"##;

/// Render the template for `config`.
fn render(config: &config::Configuration) -> String {
    let mut reg = Handlebars::new();
    // Every substituted value sits inside a TOML string or is a number or a
    // boolean, so escape for TOML rather than for HTML: the default would turn
    // `&` or `'` into entities and leave a `"` or `\` to break the string.
    reg.register_escape_fn(toml_escape);
    // A mistyped setting name should fail loudly, not render as nothing.
    reg.set_strict_mode(true);
    reg.render_template(TEMPLATE, config)
        .expect("render configfile error")
}

fn toml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    config::push_toml_escaped(&mut out, value);
    out
}

pub fn run(config: &config::Configuration) {
    println!("{}", render(config));
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::config::{Configuration, Protocol, UdpMode};

    /// Not every setting is written out by the template, and `Configuration`
    /// has no `PartialEq`, so compare the settings as JSON with these dropped.
    const NOT_RENDERED: [&str; 3] = [
        "filters",
        "sniffer.api.cors_origins",
        "sniffer.mqtt.password",
    ];

    fn parse(rendered: &str) -> Configuration {
        toml::from_str(rendered)
            .unwrap_or_else(|e| panic!("the output is not valid configuration: {e}\n{rendered}"))
    }

    /// Every setting of `config` by dotted path, minus the ones the template
    /// does not write out.
    fn rendered_settings(config: &Configuration) -> BTreeMap<String, Value> {
        fn walk(path: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
            match value {
                Value::Object(map) => {
                    for (key, value) in map {
                        let path = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        walk(&path, value, out);
                    }
                }
                leaf => {
                    out.insert(path.to_string(), leaf.clone());
                }
            }
        }

        let mut all = BTreeMap::new();
        walk("", &serde_json::to_value(config).unwrap(), &mut all);
        all.retain(|path, _| !NOT_RENDERED.iter().any(|skip| path.starts_with(skip)));
        all
    }

    /// A configuration in which every rendered setting differs from its
    /// default, with awkward characters wherever a string allows them.
    fn customised() -> Configuration {
        let mut config = Configuration::default();

        config.sniffer.log_level = "DEBUG".to_string();
        config.sniffer.log_to_syslog = true;
        config.sniffer.api.bind = "127.0.0.1:9000".to_string();
        config.sniffer.api.cors_origins = vec!["https://example.com".to_string()];
        config.sniffer.api.allowed_hosts =
            vec!["sniffer.example.com".to_string(), "lab-pc".to_string()];
        config.sniffer.database.path = r#"C:\data\"quoted" & <odd>=it's.db"#.to_string();
        config.sniffer.database.retention = Duration::from_secs(7 * 24 * 3600);

        let mqtt = &mut config.sniffer.mqtt;
        mqtt.enabled = true;
        mqtt.server = "broker.example.com".to_string();
        mqtt.port = 8883;
        mqtt.tls = true;
        mqtt.ca_file = "/etc/ssl/my ca.pem".to_string();
        mqtt.client_cert_file = "/etc/ssl/client.pem".to_string();
        mqtt.client_key_file = "/etc/ssl/client.key".to_string();
        mqtt.client_id = r#"sniffer "one""#.to_string();
        mqtt.username = r"al\ice".to_string();
        mqtt.password = "canary-7f3e".to_string();
        mqtt.topic = "custom/{gateway_eui}/{status}".to_string();
        mqtt.qos = 2;

        config.gateway.protocol = Protocol::Concentratord;
        let udp = &mut config.gateway.udp;
        udp.mode = UdpMode::Listen;
        udp.listen_acks = false;
        udp.session_timeout = Duration::from_secs(90);
        udp.max_gateways = 7;
        udp.downstream.bind = "127.0.0.1:1701".to_string();
        udp.upstream.server = "lns.example.com".to_string();
        udp.upstream.port = 1702;
        let concentratord = &mut config.gateway.concentratord;
        concentratord.downstream.event_url = "ipc:///run/event".to_string();
        concentratord.downstream.command_url = "ipc:///run/command".to_string();
        concentratord.upstream.event_bind = "ipc:///run/sniffer_event".to_string();
        concentratord.upstream.command_bind = "ipc:///run/sniffer_command".to_string();

        config
    }

    #[test]
    fn default_configuration_round_trips() {
        let defaults = Configuration::default();
        let reloaded = parse(&render(&defaults));
        assert_eq!(rendered_settings(&reloaded), rendered_settings(&defaults));
    }

    #[test]
    fn customised_configuration_round_trips() {
        // The comparison below proves nothing for a setting whose custom
        // value equals the default, because a key missing from the template
        // would then still come back right.
        let defaults = rendered_settings(&Configuration::default());
        let custom = rendered_settings(&customised());
        assert_eq!(defaults.len(), custom.len());
        for (path, value) in &custom {
            assert_ne!(value, &defaults[path], "{path} is still at its default");
        }

        let rendered = render(&customised());
        assert_eq!(rendered_settings(&parse(&rendered)), custom);

        // Spot-check the two kinds of value that need converting.
        assert!(
            rendered.contains(r#"session_timeout = "1m 30s""#),
            "{rendered}"
        );
        assert!(rendered.contains(r#"mode = "listen""#), "{rendered}");
    }

    #[test]
    fn allowed_hosts_are_written_as_an_array() {
        let render_hosts = |hosts: &[&str]| {
            let mut config = Configuration::default();
            config.sniffer.api.allowed_hosts = hosts.iter().map(|h| h.to_string()).collect();
            render(&config)
        };

        let rendered = render_hosts(&[]);
        assert!(
            rendered.contains("\n    allowed_hosts = []\n"),
            "{rendered}"
        );
        assert!(parse(&rendered).sniffer.api.allowed_hosts.is_empty());

        let rendered = render_hosts(&["sniffer.example.com"]);
        assert!(
            rendered.contains("\n    allowed_hosts = [\"sniffer.example.com\"]\n"),
            "{rendered}"
        );

        let rendered = render_hosts(&["a.example", "b.example", "c.example"]);
        assert!(
            rendered
                .contains("\n    allowed_hosts = [\"a.example\", \"b.example\", \"c.example\"]\n"),
            "{rendered}"
        );
        let reloaded = parse(&rendered);
        assert_eq!(
            reloaded.sniffer.api.allowed_hosts,
            ["a.example", "b.example", "c.example"]
        );
        config::validate(&reloaded).unwrap();

        // The unsafe opt-out survives a round trip as it is written.
        let reloaded = parse(&render_hosts(&["*"]));
        assert_eq!(reloaded.sniffer.api.allowed_hosts, ["*"]);
        config::validate(&reloaded).unwrap();
    }

    #[test]
    fn allowed_hosts_comment_explains_the_setting() {
        let rendered = render(&Configuration::default());
        let comment: String = rendered
            .lines()
            .take_while(|line| !line.trim_start().starts_with("allowed_hosts"))
            .skip_while(|line| !line.contains("Host names the WebUI"))
            .collect::<Vec<_>>()
            .join("\n");

        for phrase in [
            "IP addresses",
            "localhost",
            "DNS rebinding",
            "reverse proxy",
            "Host header",
            "unsafe",
            "SNIFFER_API_ALLOWED_HOSTS",
        ] {
            assert!(comment.contains(phrase), "no '{phrase}' in:\n{comment}");
        }
    }

    #[test]
    fn awkward_allowed_hosts_stay_inside_their_strings() {
        // Validation rejects such entries, but the template must not let one
        // break out of its string if one is ever rendered.
        let mut config = Configuration::default();
        config.sniffer.api.allowed_hosts = vec![r#"a"b\c"#.to_string(), "d&e'f".to_string()];

        let reloaded = parse(&render(&config));

        assert_eq!(
            reloaded.sniffer.api.allowed_hosts,
            config.sniffer.api.allowed_hosts
        );
    }

    #[test]
    fn password_is_never_rendered() {
        for password in ["canary-7f3e", r#"p@ss"w0rd\$HOME"#, ""] {
            let mut config = customised();
            config.sniffer.mqtt.password = password.to_string();
            let rendered = render(&config);

            if !password.is_empty() {
                assert!(!rendered.contains(password), "password in the output");
                let escaped = password.replace('\\', r"\\").replace('"', r#"\""#);
                assert!(
                    !rendered.contains(&escaped),
                    "escaped password in the output"
                );
            }
            // Only a commented placeholder, so the loaded file has no password
            // unless the user adds one.
            assert!(rendered.contains("\n    # password = \"\"\n"), "{rendered}");
            assert_eq!(parse(&rendered).sniffer.mqtt.password, "");
            assert!(
                !rendered
                    .lines()
                    .any(|l| l.trim_start().starts_with("password")),
                "an active password line"
            );
        }
    }

    #[test]
    fn filter_example_is_valid_once_uncommented() {
        let rendered = render(&Configuration::default());
        let example: Vec<&str> = rendered
            .lines()
            .skip_while(|line| *line != "# [[filters]]")
            .map(|line| line.strip_prefix('#').unwrap_or(line))
            .collect();
        assert!(!example.is_empty(), "no filter example in the output");

        let config = parse(&example.join("\n"));
        assert_eq!(config.filters.len(), 1);
        let rule = &config.filters[0];
        assert!(rule.enabled);
        assert_eq!(rule.gateways, ["AA:BB:CC:DD:00:11:22:33"]);
        assert_eq!(rule.mtype.len(), 1);
    }
}
