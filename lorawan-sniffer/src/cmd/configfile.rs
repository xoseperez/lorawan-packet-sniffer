use handlebars::Handlebars;
use serde_json::Value;

use crate::config;
use crate::core::random::{self, RandomError, RandomSource};

/// The annotated configuration template; `{{ .. }}` marks where a setting of
/// the loaded configuration is filled in.
///
/// Neither the MQTT password nor the loaded login token has a placeholder, on
/// purpose: this output gets saved, copied around and pasted into bug reports,
/// so a secret that came from a configuration file or the environment must
/// never be written into it.  The `token` line holds `generated_token`
/// instead, a fresh value made for this output alone.
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

    # Ask for the token below before the WebUI and the API answer anything
    # (the WebUI shows a login page; scripts send "Authorization: Bearer
    # <token>"). When false, the token is ignored.
    login = {{ sniffer.api.login }}

    # The shared login token: a random token of at least 32 hexadecimal
    # characters. The sniffer checks only its format, so always generate it,
    # for example with `openssl rand -hex 16`; this one was generated for
    # this output. Changing it logs every browser out at the next restart.
    # In Docker, set SNIFFER_API_LOGIN and SNIFFER_API_TOKEN instead.
    token = "{{ generated_token }}"

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
#   # Message types to match; omit or leave empty to match every type. A rule
#   # with a list never matches a malformed frame (CRC failed, bad header or
#   # wrong length): its type is only its first byte read as a header, which
#   # means nothing for noise. A rule without a list still matches them.
#   mtype       = ["ConfirmedDataUp"]
#   # Optional: only apply to these gateways (EUIs, with or without ':' or
#   # '-'). Omit to apply to every gateway.
#   gateways    = ["AA:BB:CC:DD:00:11:22:33"]
"##;

/// The values the template is rendered from: the configuration as JSON, with
/// `generated_token` added at the top level.
///
/// The loaded `sniffer.api.token` is taken out. The template has no placeholder
/// for it, but with it gone a placeholder added by mistake fails the strict
/// rendering below instead of printing a secret.
fn template_values(config: &config::Configuration, generated_token: &str) -> Value {
    let mut values = serde_json::to_value(config).expect("configuration is valid JSON");
    if let Some(api) = values
        .pointer_mut("/sniffer/api")
        .and_then(Value::as_object_mut)
    {
        api.remove("token");
    }
    values["generated_token"] = Value::String(generated_token.to_string());
    values
}

/// Render the template for `config`, with `generated_token` as the value of
/// the `token` setting.
fn render(config: &config::Configuration, generated_token: &str) -> String {
    let mut reg = Handlebars::new();
    // Every substituted value sits inside a TOML string or is a number or a
    // boolean, so escape for TOML rather than for HTML: the default would turn
    // `&` or `'` into entities and leave a `"` or `\` to break the string.
    reg.register_escape_fn(toml_escape);
    // A mistyped setting name should fail loudly, not render as nothing.
    reg.set_strict_mode(true);
    reg.render_template(TEMPLATE, &template_values(config, generated_token))
        .expect("render configfile error")
}

fn toml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    config::push_toml_escaped(&mut out, value);
    out
}

/// The whole output: a login token read from `random`, then the template.
///
/// The token comes first so that a failed read leaves nothing to print.
/// A template without a token, or with a short one, would look finished and
/// be refused later (or worse, be edited by hand into something guessable).
fn output(
    config: &config::Configuration,
    random: &dyn RandomSource,
) -> Result<String, RandomError> {
    let token = random::generate_token(random)?;
    Ok(render(config, &token))
}

/// Print the annotated template for `config`, with a freshly generated login
/// token.  Prints nothing and returns the error when no token can be made.
pub fn run(config: &config::Configuration, random: &dyn RandomSource) -> Result<(), RandomError> {
    println!("{}", output(config, random)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::config::{token_shape_ok, Configuration, Protocol, UdpMode};
    use crate::core::random::testing::FailingRandom;
    use crate::core::random::{generate_token, SystemRandomSource};

    /// Not every setting is written out by the template, and `Configuration`
    /// has no `PartialEq`, so compare the settings as JSON with these dropped.
    /// The loaded token is one of them on purpose: the output carries a new
    /// one, never the loaded one.
    const NOT_RENDERED: [&str; 4] = [
        "filters",
        "sniffer.api.cors_origins",
        "sniffer.api.token",
        "sniffer.mqtt.password",
    ];

    /// The token the tests render with, where its value does not matter.
    const TOKEN: &str = "5f0c9a3e7b1d48c6a2e9d4b7f3a18c05";

    fn new_token() -> String {
        generate_token(&SystemRandomSource::new()).unwrap()
    }

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
        config.sniffer.api.login = true;
        config.sniffer.api.token = "canary-token-2d8f".to_string();
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
        let reloaded = parse(&render(&defaults, TOKEN));
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

        let rendered = render(&customised(), TOKEN);
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
            render(&config, TOKEN)
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
        let rendered = render(&Configuration::default(), TOKEN);
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

        let reloaded = parse(&render(&config, TOKEN));

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
            let rendered = render(&config, TOKEN);

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
    fn login_and_token_are_written_after_allowed_hosts() {
        let rendered = render(&Configuration::default(), TOKEN);

        let hosts = rendered.find("\n    allowed_hosts = ").unwrap();
        let login = rendered.find("\n    login = false\n").unwrap();
        let token = rendered
            .find(&format!("\n    token = \"{TOKEN}\"\n"))
            .unwrap();
        let database = rendered.find("[sniffer.database]").unwrap();
        assert!(
            hosts < login && login < token && token < database,
            "{rendered}"
        );

        let reloaded = parse(&rendered);
        assert!(!reloaded.sniffer.api.login);
        assert_eq!(reloaded.sniffer.api.token, TOKEN);

        let mut config = Configuration::default();
        config.sniffer.api.login = true;
        let rendered = render(&config, TOKEN);
        assert!(rendered.contains("\n    login = true\n"), "{rendered}");
        assert!(parse(&rendered).sniffer.api.login);
    }

    #[test]
    fn login_comments_explain_the_settings() {
        let rendered = render(&Configuration::default(), TOKEN);
        let comment: String = rendered
            .lines()
            .skip_while(|line| !line.contains("Ask for the token below"))
            .take_while(|line| !line.starts_with("  [sniffer.database]"))
            .collect::<Vec<_>>()
            .join("\n");

        for phrase in [
            "Authorization: Bearer",
            "When false, the token is ignored",
            "at least 32 hexadecimal",
            "checks only its format",
            "openssl rand -hex 16",
            "generated for\n    # this output",
            "logs every browser out",
            "SNIFFER_API_LOGIN",
            "SNIFFER_API_TOKEN",
        ] {
            assert!(comment.contains(phrase), "no '{phrase}' in:\n{comment}");
        }
    }

    #[test]
    fn output_is_valid_once_login_is_turned_on() {
        let out = output(&Configuration::default(), &SystemRandomSource::new()).unwrap();
        let mut config = parse(&out);
        assert!(!config.sniffer.api.login);
        assert!(token_shape_ok(&config.sniffer.api.token));

        config.sniffer.api.login = true;
        config::validate(&config).unwrap();

        // Written with login on, it is valid as it stands.
        let mut loaded = Configuration::default();
        loaded.sniffer.api.login = true;
        let config = parse(&output(&loaded, &SystemRandomSource::new()).unwrap());
        assert!(config.sniffer.api.login);
        config::validate(&config).unwrap();
    }

    #[test]
    fn every_output_has_its_own_token() {
        let source = SystemRandomSource::new();
        let first = parse(&output(&Configuration::default(), &source).unwrap());
        let second = parse(&output(&Configuration::default(), &source).unwrap());

        let (first, second) = (first.sniffer.api.token, second.sniffer.api.token);
        assert!(token_shape_ok(&first) && token_shape_ok(&second));
        assert_eq!(first.len(), 32);
        assert!(first
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(first, second);
    }

    #[test]
    fn a_loaded_token_is_never_rendered() {
        let hex_looking = new_token();
        for canary in [
            "canary-token-2d8f",
            r#"ca"na\ry&<'>"#,
            hex_looking.as_str(),
            "$SNIFFER_API_TOKEN",
        ] {
            for login in [false, true] {
                let mut config = customised();
                config.sniffer.api.login = login;
                config.sniffer.api.token = canary.to_string();
                let out = output(&config, &SystemRandomSource::new()).unwrap();

                assert!(!out.contains(canary), "the token is in the output:\n{out}");
                let mut escaped = String::new();
                config::push_toml_escaped(&mut escaped, canary);
                assert!(
                    !out.contains(&escaped),
                    "the escaped token is in the output:\n{out}"
                );
                assert_ne!(parse(&out).sniffer.api.token, canary);
            }
        }
    }

    #[test]
    fn the_loaded_token_is_not_among_the_template_values() {
        let mut config = Configuration::default();
        config.sniffer.api.token = "canary-token-2d8f".to_string();

        let values = template_values(&config, TOKEN);

        assert!(!values.to_string().contains("canary-token-2d8f"));
        assert_eq!(values["generated_token"], TOKEN);
        assert_eq!(values["sniffer"]["api"]["login"], false);
    }

    #[test]
    fn a_failing_random_source_gives_no_output() {
        let mut config = Configuration::default();
        config.sniffer.api.login = true;

        // No text is produced at all, so there is nothing a caller could
        // print: not even a template with an empty or short token.
        assert_eq!(output(&config, &FailingRandom), Err(RandomError));
        assert_eq!(run(&config, &FailingRandom), Err(RandomError));
    }

    #[test]
    fn filter_example_is_valid_once_uncommented() {
        let rendered = render(&Configuration::default(), TOKEN);
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
