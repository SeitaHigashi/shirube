//! Shared construction of outbound HTTP clients.
//!
//! Every `reqwest` client in this binary is built through [`client_builder`]
//! so that they all trust the same set of TLS roots.
//!
//! # Why this exists
//!
//! `reqwest` is compiled with the `rustls-tls` feature, whose trust anchors
//! are the Mozilla root set bundled into the binary at compile time. That set
//! is fixed: unlike OpenSSL-backed clients, it does **not** consult
//! `SSL_CERT_FILE`, `SSL_CERT_DIR`, or the OS trust store. That is the right
//! default for a trading bot talking to bitFlyer over the public internet —
//! the trust anchors cannot be widened by whatever happens to be installed on
//! the host.
//!
//! It breaks, however, in any environment that terminates TLS on a local
//! egress proxy and re-signs it with a private CA, which is exactly how the
//! self-improvement loop's cloud routine runs (see
//! `docs/self-improvement-loop.md`). There every outbound call — the bitFlyer
//! execution backfill and the `backtest-data` release download alike — failed
//! with `invalid peer certificate: UnknownIssuer`, so the loop could not
//! bootstrap its backtest DB at all.
//!
//! The fix is deliberately narrow: keep the built-in roots as the baseline and
//! *additionally* trust the PEM bundle named by `SSL_CERT_FILE` (or
//! `REQUESTS_CA_BUNDLE`) when one is set and readable. Nothing changes on a
//! host that sets neither variable, which is the normal deployment: the client
//! keeps exactly the webpki roots it had before.
//!
//! NOTE: a missing, unreadable, or unparseable bundle is a warning rather than
//! an error. Failing client construction would take down a bot that was
//! previously working just because an unrelated env var pointed somewhere
//! stale; the subsequent TLS handshake is a better place to fail, since it
//! reports the host that actually could not be verified.

use std::time::Duration;

/// Environment variables consulted for an additional PEM trust bundle, in
/// order. These are the two conventional spellings; `SSL_CERT_FILE` is the
/// OpenSSL one and `REQUESTS_CA_BUNDLE` is honoured by most tooling that
/// wraps it.
const CA_BUNDLE_VARS: [&str; 2] = ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE"];

/// A `reqwest::ClientBuilder` carrying this binary's standard TLS trust
/// configuration: the compiled-in webpki roots, plus any certificates found
/// in the PEM bundle named by `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE`.
///
/// Callers add their own timeouts, default headers, etc. on top:
///
/// ```ignore
/// let http = crate::http::client_builder()
///     .timeout(Duration::from_secs(10))
///     .build()?;
/// ```
pub fn client_builder() -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder();
    for cert in extra_root_certificates() {
        builder = builder.add_root_certificate(cert);
    }
    builder
}

/// Like [`client_builder`], but also applies `timeout` and builds the client,
/// panicking on failure — the shape the existing call sites already used.
///
/// A failure here means the TLS backend itself could not be initialised, which
/// is not recoverable at runtime and happens identically on every call, so the
/// panic surfaces it at startup rather than on the first request.
pub fn client_with_timeout(timeout: Duration) -> reqwest::Client {
    client_builder()
        .timeout(timeout)
        .build()
        .expect("Failed to build HTTP client")
}

/// Load extra trust anchors from whichever of [`CA_BUNDLE_VARS`] is set first.
///
/// Returns an empty vector — the normal case on a developer machine or the
/// production host — when no variable is set, the path does not exist, or the
/// file holds no parseable certificate.
fn extra_root_certificates() -> Vec<reqwest::Certificate> {
    let Some((var, path)) = CA_BUNDLE_VARS
        .iter()
        .find_map(|var| std::env::var(var).ok().map(|path| (*var, path)))
        .filter(|(_, path)| !path.is_empty())
    else {
        return Vec::new();
    };

    let pem = match std::fs::read(&path) {
        Ok(pem) => pem,
        Err(e) => {
            tracing::warn!("{var}={path} could not be read ({e}); using built-in TLS roots only");
            return Vec::new();
        }
    };

    match reqwest::Certificate::from_pem_bundle(&pem) {
        Ok(certs) => {
            tracing::info!(
                "Trusting {} extra TLS root certificate(s) from {var}={path}",
                certs.len()
            );
            certs
        }
        Err(e) => {
            tracing::warn!("{var}={path} is not a usable PEM bundle ({e}); using built-in TLS roots only");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// All three scenarios live in one `#[test]` on purpose.
    ///
    /// NOTE: they assert on process-global environment variables, and the
    /// test harness runs `#[test]` functions on parallel threads — as
    /// separate tests they would race each other's `set_var`/`remove_var`
    /// and fail intermittently. Keeping them sequential in a single test is
    /// the fix that needs no extra dependency.
    #[test]
    fn env_bundle_is_read_without_disturbing_the_built_in_roots() {
        // 1. No bundle configured — the normal deployment. The client keeps
        //    exactly the compiled-in webpki roots and nothing is added.
        temp_env_clear(|| {
            assert!(extra_root_certificates().is_empty());
            assert!(client_builder().build().is_ok());
        });

        // 2. A bundle path that does not exist must degrade to a warning, not
        //    a failure: a stale env var must never take down a bot that was
        //    otherwise working.
        temp_env_ca_bundle("/nonexistent/ca-bundle.crt", || {
            assert!(extra_root_certificates().is_empty());
            assert!(client_builder().build().is_ok());
        });

        // 3. The regression this module exists for: a real PEM bundle named
        //    by SSL_CERT_FILE is parsed and its certificates are handed to
        //    the client, rather than being ignored as they were by the bare
        //    `reqwest::Client::builder()` the call sites used before.
        let dir = std::env::temp_dir().join(format!("shirube-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bundle.crt");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(TEST_PEM.as_bytes())
            .unwrap();

        temp_env_ca_bundle(path.to_str().unwrap(), || {
            assert_eq!(extra_root_certificates().len(), 1);
            assert!(client_builder().build().is_ok());
        });

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Set `SSL_CERT_FILE` (and clear `REQUESTS_CA_BUNDLE` so it cannot
    /// shadow the assertion) for the duration of `f`, then restore both.
    fn temp_env_ca_bundle(path: &str, f: impl FnOnce()) {
        let saved: Vec<_> = CA_BUNDLE_VARS
            .iter()
            .map(|v| (*v, std::env::var(v).ok()))
            .collect();
        std::env::set_var("SSL_CERT_FILE", path);
        std::env::remove_var("REQUESTS_CA_BUNDLE");
        f();
        for (var, value) in saved {
            match value {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }

    fn temp_env_clear(f: impl FnOnce()) {
        let saved: Vec<_> = CA_BUNDLE_VARS
            .iter()
            .map(|v| (*v, std::env::var(v).ok()))
            .collect();
        for var in CA_BUNDLE_VARS {
            std::env::remove_var(var);
        }
        f();
        for (var, value) in saved {
            match value {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }

    /// A throwaway self-signed certificate, used only to prove the PEM bundle
    /// is parsed. It is never presented to or trusted by any real peer.
    const TEST_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBiTCCAS+gAwIBAgIUJyNbHLxI2QKMHlZRZxLHTx1B5QkwCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPc2hpcnViZS10ZXN0LWNhMB4XDTI2MDkxMTE5MTM1MloXDTQ2
MDkwNjE5MTM1MlowGjEYMBYGA1UEAwwPc2hpcnViZS10ZXN0LWNhMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEywSs8Wn01UGy3Zbn4zul8KvMCPU4Gk5HPr6j25Bz
gACFWCmH2a0RzkBcQgCfGP/TSMojt7TXqu2ZkStvIOGm3qNTMFEwHQYDVR0OBBYE
FIowHuVXFoGawSVUXhl2eZVdwP7oMB8GA1UdIwQYMBaAFIowHuVXFoGawSVUXhl2
eZVdwP7oMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIgaPHvDjuQ
MbJepJf6c0xsS57IQHjtkJ7P7H7Ur7lfWNkCIQDAi19l7iLOn1wDjk6XDlv6/sz9
b00JDd+cd5tkSPqfGA==
-----END CERTIFICATE-----
";
}
