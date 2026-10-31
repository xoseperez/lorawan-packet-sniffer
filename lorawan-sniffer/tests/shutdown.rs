//! Stopping the sniffer the way service managers do.
//!
//! `systemctl stop` and `docker stop` both send SIGTERM.  The sniffer must
//! treat it like Ctrl-C: stop the proxy, save what is queued and exit with
//! status 0, quickly.  Without a handler the process would die on the signal
//! (or, as PID 1 in a container, ignore it until Docker kills it).

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Generous upper bound for start-up and for the stop, so a slow CI machine
/// does not fail the test; a healthy run takes a fraction of a second.
const TIMEOUT: Duration = Duration::from_secs(15);

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("lorawan-sniffer-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Start the sniffer in UDP listen mode on free loopback ports, with stdout
/// and stderr going to `log`.
fn start(dir: &Scratch, log: &PathBuf) -> Child {
    let config = dir.0.join("sniffer.toml");
    fs::write(
        &config,
        format!(
            r#"
[sniffer]
  log_level = "INFO"
  [sniffer.api]
    bind = "127.0.0.1:0"
  [sniffer.database]
    path = "{}"

[gateway]
  protocol = "udp"
  [gateway.udp]
    mode = "listen"
    [gateway.udp.downstream]
      bind = "127.0.0.1:0"
"#,
            dir.0.join("sniffer.db").display()
        ),
    )
    .unwrap();

    let out = fs::File::create(log).unwrap();
    let err = out.try_clone().unwrap();
    Command::new(env!("CARGO_BIN_EXE_lorawan-sniffer"))
        .arg("-c")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .unwrap()
}

fn wait_for_line(log: &PathBuf, needle: &str, child: &mut Child) {
    let start = Instant::now();
    loop {
        let text = fs::read_to_string(log).unwrap_or_default();
        if text.contains(needle) {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("sniffer exited early ({status}) before logging {needle:?}:\n{text}");
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("sniffer did not log {needle:?} in time:\n{text}");
        }
        sleep(Duration::from_millis(20));
    }
}

fn send(signal: &str, child: &Child) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success(), "kill -{signal} failed");
}

fn wait_exit(child: &mut Child) -> (std::process::ExitStatus, Duration) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status, start.elapsed());
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("sniffer did not exit within {TIMEOUT:?} of the signal");
        }
        sleep(Duration::from_millis(20));
    }
}

fn stops_cleanly_on(signal: &str) {
    let dir = Scratch::new(&format!("stop-{}", signal.to_lowercase()));
    let log = dir.0.join("sniffer.log");
    let mut child = start(&dir, &log);

    // Logged once the proxy holds its socket, i.e. fully started.
    wait_for_line(&log, "UDP downstream bound to", &mut child);
    send(signal, &child);
    let (status, took) = wait_exit(&mut child);

    let text = fs::read_to_string(&log).unwrap();
    assert!(
        status.success(),
        "exit status {status} after {signal}:\n{text}"
    );
    assert!(
        text.contains(&format!("Received SIG{signal}, shutting down")),
        "no shutdown line for {signal}:\n{text}"
    );
    assert!(text.contains("Sniffer stopped"), "no final line:\n{text}");
    // Nothing is queued in this run, so stopping must not sit out the
    // writer's drain timeout.
    assert!(took < Duration::from_secs(3), "stop took {took:?}");
}

#[test]
fn sigterm_stops_cleanly() {
    stops_cleanly_on("TERM");
}

#[test]
fn sigint_stops_cleanly() {
    stops_cleanly_on("INT");
}
