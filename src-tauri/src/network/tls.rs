use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::path::PathBuf;
use std::sync::Arc;

/// Generate a self-signed TLS certificate for this peer.
/// Certificates are persisted so they remain stable across restarts
/// (enabling cert pinning for trusted peers).
pub fn get_or_create_identity() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let cert_path = data_dir().join("cert.pem");
    let key_path = data_dir().join("key.pem");

    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path).map_err(|e| e.to_string())?;
        let key_pem = std::fs::read_to_string(&key_path).map_err(|e| e.to_string())?;
        let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let key = rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_bytes())
            .next()
            .ok_or("No private key found")?
            .map_err(|e| e.to_string())?;
        return Ok((certs, PrivateKeyDer::Pkcs8(key)));
    }

    // Generate new self-signed cert
    let key_pair = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut params = CertificateParams::new(vec!["shareflow.local".to_string()])
        .map_err(|e| e.to_string())?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("ShareFlow Peer".into()),
    );
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| e.to_string())?;

    // Persist
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&cert_path, cert.pem()).map_err(|e| e.to_string())?;
    std::fs::write(&key_path, key_pair.serialize_pem()).map_err(|e| e.to_string())?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    Ok((vec![cert_der], key_der))
}

/// Create a rustls ServerConfig for accepting connections.
pub fn make_server_config() -> Result<Arc<rustls::ServerConfig>, String> {
    let (certs, key) = get_or_create_identity()?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(config))
}

/// Create a rustls ClientConfig that accepts any server cert
/// (we do our own cert pinning at the application level).
pub fn make_client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Certificate verifier that accepts any cert.
/// We handle trust via application-level cert pinning instead.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("shareflow")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join("Library/Application Support/shareflow")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        PathBuf::from(".").join("shareflow")
    }
}
