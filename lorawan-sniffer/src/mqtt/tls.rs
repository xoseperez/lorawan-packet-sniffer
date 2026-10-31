//! TLS client configuration for the MQTT connection.
//!
//! The broker's certificate is verified against the bundled public CA roots,
//! or, when `ca_file` is set, against the certificates in that file only (a
//! private CA is not trusted on top of the public ones, so a broker that is
//! meant to use the private CA cannot be impersonated by anything a public CA
//! would sign).  A client certificate and key switch on mutual TLS.
//!
//! The crypto provider is passed in explicitly.  rustls' default-provider
//! builder would pick whichever provider the dependency graph happens to
//! enable, and `aws-lc-rs` needs CMake and NASM to cross-compile to musl.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::pem::{self, PemObject};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::MqttConfig;

/// Build the rustls configuration described by `cfg`.
///
/// Fails when a configured file cannot be read or holds no usable
/// certificate or key, so a wrong path or a mangled PEM stops the sniffer at
/// startup instead of surfacing as an endless reconnect loop.
pub fn client_config(cfg: &MqttConfig) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("the TLS library supports no usable protocol version")?
        .with_root_certificates(root_store(cfg)?);

    match (
        cfg.client_cert_file.is_empty(),
        cfg.client_key_file.is_empty(),
    ) {
        (true, true) => Ok(builder.with_no_client_auth()),
        (false, false) => {
            let certs = load_certificates(&cfg.client_cert_file)
                .with_context(|| format!("client_cert_file '{}'", cfg.client_cert_file))?;
            let key = load_private_key(&cfg.client_key_file)?;
            builder
                .with_client_auth_cert(certs, key)
                .context("the client certificate and key were rejected (do they belong together?)")
        }
        _ => bail!("client_cert_file and client_key_file must be set together"),
    }
}

/// The CAs the broker's certificate must chain to.
fn root_store(cfg: &MqttConfig) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();

    if cfg.ca_file.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        return Ok(roots);
    }

    let certs =
        load_certificates(&cfg.ca_file).with_context(|| format!("ca_file '{}'", cfg.ca_file))?;
    // A bundle such as the system's can hold the odd certificate rustls
    // refuses; those are skipped, but a file where none is usable is an error.
    let (added, ignored) = roots.add_parsable_certificates(certs);
    if added == 0 {
        bail!(
            "ca_file '{}' has no usable CA certificate ({} could not be parsed)",
            cfg.ca_file,
            ignored
        );
    }
    if ignored > 0 {
        warn!(
            "ca_file '{}': ignored {} certificate(s) that could not be parsed",
            cfg.ca_file, ignored
        );
    }
    Ok(roots)
}

/// All certificates in a PEM file; an error when there are none.
fn load_certificates(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(Path::new(path))
        .map_err(|e| anyhow!("cannot read the file: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("not a valid PEM certificate file: {}", e))?;
    if certs.is_empty() {
        bail!("the file contains no certificates");
    }
    Ok(certs)
}

/// The first private key in a PEM file (PKCS#8, PKCS#1 or SEC1).
///
/// The error text is built from a fixed vocabulary on purpose: the PEM
/// parser's own messages can quote parts of the data it choked on, and for a
/// private key that must never reach a log.
fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(Path::new(path)).map_err(|e| {
        let reason = match e {
            pem::Error::Io(_) => "cannot read the file",
            pem::Error::NoItemsFound => "the file contains no private key",
            _ => "the file is not a valid PEM private key",
        };
        anyhow!("client_key_file '{}': {}", path, reason)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::PathBuf;

    /// A file in the temp directory that is removed when dropped.
    struct TempFile(PathBuf);

    impl TempFile {
        fn with_contents(name: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "lorawan-sniffer-tls-test-{}-{}",
                std::process::id(),
                name
            ));
            fs::write(&path, contents).unwrap();
            TempFile(path)
        }

        fn path(&self) -> String {
            self.0.to_str().unwrap().to_string()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    /// The error `client_config` fails with, flattened to one string.
    fn error_for(cfg: &MqttConfig) -> String {
        match client_config(cfg) {
            Ok(_) => panic!("expected the configuration to be rejected"),
            Err(e) => format!("{e:#}"),
        }
    }

    const BAD_BASE64_CERT: &str =
        "-----BEGIN CERTIFICATE-----\n!!!this is not base64!!!\n-----END CERTIFICATE-----\n";

    /// Well-formed PEM whose payload ("hello world") is not a DER certificate:
    /// it loads as a file but is no use as a certificate.
    const NOT_DER_CERT: &str =
        "-----BEGIN CERTIFICATE-----\naGVsbG8gd29ybGQ=\n-----END CERTIFICATE-----\n";

    #[test]
    fn bundled_roots_build_a_config_without_any_file() {
        assert!(client_config(&MqttConfig::default()).is_ok());
    }

    #[test]
    fn missing_ca_file_is_an_error() {
        let cfg = MqttConfig {
            ca_file: "/nonexistent/lorawan-sniffer/ca.pem".to_string(),
            ..MqttConfig::default()
        };

        let err = error_for(&cfg);

        assert!(err.contains("ca_file"), "{err}");
        assert!(err.contains("/nonexistent/lorawan-sniffer/ca.pem"), "{err}");
    }

    #[test]
    fn ca_file_without_certificates_is_an_error() {
        let file = TempFile::with_contents("ca-empty", "this file has no PEM sections at all\n");
        let cfg = MqttConfig {
            ca_file: file.path(),
            ..MqttConfig::default()
        };

        let err = error_for(&cfg);

        assert!(err.contains("no certificates"), "{err}");
    }

    #[test]
    fn empty_ca_file_is_an_error() {
        let file = TempFile::with_contents("ca-zero-bytes", "");
        let cfg = MqttConfig {
            ca_file: file.path(),
            ..MqttConfig::default()
        };

        assert!(error_for(&cfg).contains("no certificates"));
    }

    #[test]
    fn garbage_pem_in_ca_file_is_an_error() {
        let file = TempFile::with_contents("ca-garbage", BAD_BASE64_CERT);
        let cfg = MqttConfig {
            ca_file: file.path(),
            ..MqttConfig::default()
        };

        let err = error_for(&cfg);

        assert!(err.contains("not a valid PEM"), "{err}");
    }

    #[test]
    fn ca_file_whose_certificate_is_not_a_certificate_is_an_error() {
        // Valid PEM and valid base64, but the bytes are not DER: nothing in the
        // file can be used as a trust anchor.
        let file = TempFile::with_contents("ca-not-der", NOT_DER_CERT);
        let cfg = MqttConfig {
            ca_file: file.path(),
            ..MqttConfig::default()
        };

        let err = error_for(&cfg);

        assert!(err.contains("no usable CA certificate"), "{err}");
    }

    #[test]
    fn missing_client_certificate_file_is_an_error() {
        let key = TempFile::with_contents("key-present", "irrelevant");
        let cfg = MqttConfig {
            client_cert_file: "/nonexistent/lorawan-sniffer/client.pem".to_string(),
            client_key_file: key.path(),
            ..MqttConfig::default()
        };

        let err = error_for(&cfg);

        assert!(err.contains("client_cert_file"), "{err}");
        assert!(
            err.contains("/nonexistent/lorawan-sniffer/client.pem"),
            "{err}"
        );
    }

    #[test]
    fn garbage_client_certificate_is_an_error() {
        let cert = TempFile::with_contents("cert-garbage", BAD_BASE64_CERT);
        let key = TempFile::with_contents("key-present-2", "irrelevant");
        let cfg = MqttConfig {
            client_cert_file: cert.path(),
            client_key_file: key.path(),
            ..MqttConfig::default()
        };

        assert!(error_for(&cfg).contains("not a valid PEM"));
    }

    #[test]
    fn client_certificate_without_key_is_an_error() {
        let cert = TempFile::with_contents("cert-alone", BAD_BASE64_CERT);
        let cfg = MqttConfig {
            client_cert_file: cert.path(),
            ..MqttConfig::default()
        };

        assert!(error_for(&cfg).contains("must be set together"));

        let cfg = MqttConfig {
            client_key_file: "/some/key.pem".to_string(),
            ..MqttConfig::default()
        };
        assert!(error_for(&cfg).contains("must be set together"));
    }

    #[test]
    fn key_file_errors_never_quote_the_file_contents() {
        // Not a real certificate and not a real key.  The marker stands in for
        // key material: it must not come back in any error message.
        const MARKER: &str = "SECRETKEYMATERIAL";

        for (name, contents) in [
            (
                "key-bad-base64",
                format!("-----BEGIN PRIVATE KEY-----\n{MARKER}!!!\n-----END PRIVATE KEY-----\n"),
            ),
            ("key-no-section", format!("{MARKER}\n")),
            (
                "key-unterminated",
                format!("-----BEGIN PRIVATE KEY-----\n{MARKER}\n"),
            ),
        ] {
            let key = TempFile::with_contents(name, &contents);
            let cert = TempFile::with_contents(&format!("{name}-cert"), NOT_DER_CERT);
            let cfg = MqttConfig {
                client_cert_file: cert.path(),
                client_key_file: key.path(),
                ..MqttConfig::default()
            };

            let err = error_for(&cfg);

            assert!(err.contains("client_key_file"), "{name}: {err}");
            assert!(!err.contains(MARKER), "{name}: {err}");
        }
    }

    #[test]
    fn missing_client_key_file_is_an_error() {
        let cert = TempFile::with_contents("cert-for-missing-key", NOT_DER_CERT);
        let cfg = MqttConfig {
            client_cert_file: cert.path(),
            client_key_file: "/nonexistent/lorawan-sniffer/client.key".to_string(),
            ..MqttConfig::default()
        };

        let err = error_for(&cfg);

        assert!(err.contains("client_key_file"), "{err}");
        assert!(
            err.contains("/nonexistent/lorawan-sniffer/client.key"),
            "{err}"
        );
    }
}
