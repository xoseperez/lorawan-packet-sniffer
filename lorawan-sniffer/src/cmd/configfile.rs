use handlebars::Handlebars;

use crate::config;

pub fn run(config: &config::Configuration) {
    let template = r#"
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

  [sniffer.database]
    # Path to the SQLite database file.
    path = "{{ sniffer.database.path }}"

    # Message retention period.
    retention = "{{ sniffer.database.retention }}"


[gateway]
  # Gateway protocol: "udp" or "concentratord".
  protocol = "{{ gateway.protocol }}"

  [gateway.udp]
    [gateway.udp.downstream]
      bind = "{{ gateway.udp.downstream.bind }}"

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


# Packet filter rules (repeatable).
#
# [[filters]]
#   name        = "Block confirmed uplinks"
#   enabled     = true
#   direction   = "up"
#   suppress_ack = false
#   mtype       = ["ConfirmedDataUp"]
"#;

    let reg = Handlebars::new();
    println!(
        "{}",
        reg.render_template(template, config)
            .expect("render configfile error")
    );
}
