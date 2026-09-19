use anyhow::{bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::verify_server_cert_signed_by_trust_anchor;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::sync::Arc;
use tokio_postgres_rustls::MakeRustlsConnect;

/// libpq-compatible sslmode semantics:
///   disable          — no TLS
///   allow/prefer     — TLS if the server offers it, no verification
///   require          — TLS mandatory, no verification (libpq behaviour)
///   verify-ca        — verify the chain, not the hostname
///   verify-full      — verify chain + hostname
/// A provided sslrootcert without an explicit mode implies verify-full.
pub fn build_connector(mode: &str, rootcert: Option<&str>) -> Result<MakeRustlsConnect> {
    let extra_roots = rootcert.map(read_root_certs).transpose()?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .context("building TLS connector")?;
    let config = match mode {
        "disable" | "allow" | "prefer" | "require" => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(HostnameBlindVerifier {
                provider,
                required_chain: None,
            })),
        "verify-ca" => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(HostnameBlindVerifier {
                provider,
                required_chain: Some(trust_roots(extra_roots)?),
            })),
        "verify-full" => builder.with_root_certificates(trust_roots(extra_roots)?),
        other => bail!("unsupported sslmode `{other}`"),
    };
    Ok(MakeRustlsConnect::new(config.with_no_client_auth()))
}

fn read_root_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("reading sslrootcert {path}"))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing sslrootcert {path}"))?;
    if certs.is_empty() {
        bail!("parsing sslrootcert {path}: no certificates found");
    }
    Ok(certs)
}

fn trust_roots(extra_roots: Option<Vec<CertificateDer<'static>>>) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let os_store = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(os_store.certs);
    for cert in extra_roots.into_iter().flatten() {
        roots.add(cert).context("parsing sslrootcert")?;
    }
    if roots.is_empty() {
        bail!("no trusted root certificates: the OS trust store is empty and no sslrootcert was given");
    }
    Ok(roots)
}

#[derive(Debug)]
struct HostnameBlindVerifier {
    provider: Arc<CryptoProvider>,
    required_chain: Option<RootCertStore>,
}

impl ServerCertVerifier for HostnameBlindVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(roots) = &self.required_chain {
            verify_server_cert_signed_by_trust_anchor(
                &ParsedCertificate::try_from(end_entity)?,
                roots,
                intermediates,
                now,
                self.provider.signature_verification_algorithms.all,
            )?;
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
