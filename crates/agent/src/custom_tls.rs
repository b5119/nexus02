// Ready to wire into tonic once tonic::transport::Error
// becomes pub. See issue #41 and #15 (tonic upgrade).

use std::sync::Arc;

use rustls::client::danger::HandshakeSignatureValid;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::{
    DistinguishedName, Error, RootCertStore, ServerConfig, SignatureScheme,
};
use rustls_pki_types::{CertificateDer, UnixTime};
use x509_cert::der::Decode;

/// A `ClientCertVerifier` that accepts any client certificate at the TLS layer.
///
/// Verification is deferred to the application-layer interceptor which performs
/// exact DER matching against the stored peer cert. TLS signature verification
/// (proving the client owns the private key) is still performed by delegating to
/// the standard `WebPkiClientVerifier` implementation.
struct AcceptAllClientCertVerifier {
    inner: Arc<dyn ClientCertVerifier>,
}

impl std::fmt::Debug for AcceptAllClientCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcceptAllClientCertVerifier").finish()
    }
}

impl ClientCertVerifier for AcceptAllClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        // Validate the certificate's validity period (notBefore/notAfter).
        let x509 = x509_cert::Certificate::from_der(end_entity.as_ref())
            .map_err(|_| Error::General("invalid client certificate".to_string()))?;
        let validity = &x509.tbs_certificate.validity;
        let not_before = validity.not_before.to_unix_duration().as_secs();
        let not_after = validity.not_after.to_unix_duration().as_secs();
        let now_secs = now.as_secs();
        if now_secs < not_before {
            return Err(Error::General("client certificate not yet valid".to_string()));
        }
        if now_secs > not_after {
            return Err(Error::General("client certificate has expired".to_string()));
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Build a `rustls::ServerConfig` that accepts all client certs at the TLS
/// layer and defers verification to the application-layer interceptor.
pub fn build_server_config(
    cert_pem: &str,
    key_pem: &str,
) -> Result<ServerConfig, anyhow::Error> {
    let provider = rustls::crypto::ring::default_provider();

    // Build an inner WebPkiClientVerifier for TLS signature verification.
    // The root store is empty — chain verification is handled at the app layer.
    let roots = Arc::new(RootCertStore::empty());
    let inner_verifier = WebPkiClientVerifier::builder_with_provider(roots, Arc::new(provider))
        .allow_unauthenticated()
        .build()?;

    let verifier = Arc::new(AcceptAllClientCertVerifier {
        inner: inner_verifier,
    });

    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            rustls_pemfile::certs(&mut cert_pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()?,
            rustls_pemfile::private_key(&mut key_pem.as_bytes())
                .map_err(|e| anyhow::anyhow!("invalid private key: {e}"))?
                .ok_or_else(|| anyhow::anyhow!("no private key found in PEM"))?,
        )?;

    config.alpn_protocols.push(b"h2".to_vec());

    Ok(config)
}
