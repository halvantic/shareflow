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

    // Restrict private key file permissions to owner-only on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to set key file permissions: {}", e))?;
    }

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

/// Compute SHA-256 fingerprint of a DER-encoded certificate.
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    use sha2::{Sha256, Digest};
    let hash = Sha256::digest(cert_der);
    hash.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(":")
}

/// Create a rustls ClientConfig that verifies the server certificate
/// against trusted peer fingerprints (if any are configured).
pub fn make_client_config(trusted_fingerprints: Vec<String>) -> Result<Arc<rustls::ClientConfig>, String> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinningCertVerifier { trusted_fingerprints }))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Certificate verifier that checks the peer's certificate fingerprint
/// against a list of trusted fingerprints. If no fingerprints are configured
/// (first connection), it accepts the cert and logs the fingerprint for pinning.
#[derive(Debug)]
struct PinningCertVerifier {
    trusted_fingerprints: Vec<String>,
}

impl rustls::client::danger::ServerCertVerifier for PinningCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = cert_fingerprint(end_entity.as_ref());

        if self.trusted_fingerprints.is_empty() {
            // No trusted peers configured yet — accept (TOFU: trust on first use)
            log::warn!("No trusted peer fingerprints configured. Accepting cert with fingerprint: {}", fp);
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }

        if self.trusted_fingerprints.iter().any(|t| t == &fp) {
            log::info!("Peer certificate fingerprint verified: {}", fp);
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            log::error!("Peer certificate fingerprint MISMATCH: {}. Connection rejected.", fp);
            Err(rustls::Error::General(format!(
                "Certificate fingerprint {} not in trusted peers list",
                fp
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
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
