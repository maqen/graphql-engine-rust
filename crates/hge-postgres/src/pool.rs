//! Connection pools, with TLS via rustls.
//!
//! `sslmode` follows libpq semantics: `prefer` / `require` encrypt without verifying the
//! server certificate, `verify-ca` / `verify-full` verify it against `sslrootcert` (if given)
//! or the Mozilla root store.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use hge_core::{Code, Error};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Pool settings (V2 `pool_settings` + env defaults).
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub max_connections: usize,
    /// Seconds a connection may sit idle (V2 `idle_timeout`); currently advisory.
    pub idle_timeout: Option<u64>,
    /// Seconds to wait for a free connection (V2 `pool_timeout`).
    pub pool_timeout: Option<u64>,
    /// Use cached prepared statements (V2 `use_prepared_statements`).
    pub use_prepared_statements: bool,
    /// Isolation level of mutation transactions (V2 `isolation_level`).
    pub isolation_level: IsolationLevel,
}

/// A source's transaction isolation level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IsolationLevel {
    #[default]
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl IsolationLevel {
    /// Parse V2's metadata value (`read-committed`, `repeatable-read`, `serializable`).
    pub fn from_metadata(s: &str) -> Option<Self> {
        Some(match s {
            "read-committed" => IsolationLevel::ReadCommitted,
            "repeatable-read" => IsolationLevel::RepeatableRead,
            "serializable" => IsolationLevel::Serializable,
            _ => return None,
        })
    }

    pub fn sql(self) -> &'static str {
        match self {
            IsolationLevel::ReadCommitted => "READ COMMITTED",
            IsolationLevel::RepeatableRead => "REPEATABLE READ",
            IsolationLevel::Serializable => "SERIALIZABLE",
        }
    }
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            max_connections: 50,
            idle_timeout: Some(180),
            pool_timeout: None,
            use_prepared_statements: true,
            isolation_level: IsolationLevel::ReadCommitted,
        }
    }
}

/// Create a pool. Connections are opened lazily.
/// Build a pool. Also returns the TLS connector, which query cancellation needs.
pub fn connect_pool(url: &str, config: &PoolConfig) -> Result<(Pool, MakeRustlsConnect), Error> {
    let (url, tls) = prepare_url(url)?;
    let pg_config: tokio_postgres::Config = url.parse().map_err(|e| {
        Error::new(
            Code::InvalidConfiguration,
            format!("invalid database url: {e}"),
        )
    })?;
    let connector = MakeRustlsConnect::new(tls_config(&tls)?);
    let manager = Manager::from_config(
        pg_config,
        connector.clone(),
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let mut builder = Pool::builder(manager)
        .max_size(config.max_connections.max(1))
        .runtime(Runtime::Tokio1);
    if let Some(t) = config.pool_timeout {
        builder = builder.wait_timeout(Some(Duration::from_secs(t)));
    }
    builder = builder.create_timeout(Some(Duration::from_secs(30)));
    let pool = builder
        .build()
        .map_err(|e| Error::new(Code::InvalidConfiguration, format!("pool error: {e}")))?;
    Ok((pool, connector))
}

#[derive(Debug, Default)]
struct TlsSettings {
    verify: bool,
    root_cert: Option<String>,
}

/// Map libpq-only `sslmode` values and options onto what tokio-postgres understands.
fn prepare_url(url: &str) -> Result<(String, TlsSettings), Error> {
    let mut tls = TlsSettings::default();
    let Some((base, query)) = url.split_once('?') else {
        return Ok((url.to_owned(), tls));
    };
    let mut kept = vec![];
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "sslmode" if v == "verify-ca" || v == "verify-full" => {
                tls.verify = true;
                kept.push("sslmode=require".to_owned());
            }
            "sslmode" if v == "allow" => kept.push("sslmode=prefer".to_owned()),
            "sslrootcert" => tls.root_cert = Some(percent_decode(v)),
            "sslcert" | "sslkey" | "sslpassword" | "sslcrl" => {}
            _ => kept.push(pair.to_owned()),
        }
    }
    let url = if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    Ok((url, tls))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn tls_config(tls: &TlsSettings) -> Result<ClientConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::unexpected(format!("tls: {e}")))?;
    if !tls.verify {
        return Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth());
    }
    let mut roots = RootCertStore::empty();
    match &tls.root_cert {
        Some(path) => {
            let certs = CertificateDer::pem_file_iter(path).map_err(|e| {
                Error::new(
                    Code::InvalidConfiguration,
                    format!("sslrootcert {path}: {e}"),
                )
            })?;
            for cert in certs {
                let cert = cert.map_err(|e| {
                    Error::new(
                        Code::InvalidConfiguration,
                        format!("sslrootcert {path}: {e}"),
                    )
                })?;
                roots.add(cert).map_err(|e| {
                    Error::new(
                        Code::InvalidConfiguration,
                        format!("sslrootcert {path}: {e}"),
                    )
                })?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(builder.with_root_certificates(roots).with_no_client_auth())
}

/// libpq `sslmode=require` semantics: encrypt, but do not verify the certificate.
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_libpq_ssl_modes() {
        let (url, tls) = prepare_url(
            "postgres://u@h/db?sslmode=verify-full&sslrootcert=%2Fca.pem&application_name=x",
        )
        .unwrap();
        assert_eq!(url, "postgres://u@h/db?sslmode=require&application_name=x");
        assert!(tls.verify);
        assert_eq!(tls.root_cert.as_deref(), Some("/ca.pem"));
        let (url, tls) = prepare_url("postgres://u@h/db").unwrap();
        assert_eq!(url, "postgres://u@h/db");
        assert!(!tls.verify);
    }
}
