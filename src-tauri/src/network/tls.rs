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

    // Restrict private key file permissions to owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to set key file permissions: {}", e))?;
    }
    // On Windows, remove inherited ACEs and grant full control only to the
    // current user so other local accounts cannot read the private key.
    #[cfg(target_os = "windows")]
    {
        let path_str = key_path.to_string_lossy();
        let status = std::process::Command::new("icacls")
            .args([
                path_str.as_ref(),
                "/inheritance:r",                   // remove inherited ACEs
                "/grant:r",
                &format!("{}:F", whoami_windows()),  // grant current user full control
            ])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => log::warn!("icacls on key file exited with {}", s),
            Err(e) => log::warn!("Failed to run icacls on key file: {}", e),
        }
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

/// Create a rustls ClientConfig that accepts any server certificate (TOFU model)
/// and captures the presented certificate fingerprint for post-handshake pinning.
///
/// The returned `Arc<std::sync::Mutex<Option<String>>>` will contain the live
/// fingerprint after the TLS handshake completes. The caller is responsible for
/// validating it against stored trusted-peer fingerprints and pinning new peers.
pub fn make_client_config() -> Result<(Arc<rustls::ClientConfig>, Arc<std::sync::Mutex<Option<String>>>), String> {
    let capture: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let verifier = PinningCertVerifier { fingerprint_capture: Arc::clone(&capture) };
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok((Arc::new(config), capture))
}

/// Certificate verifier that captures the presented certificate fingerprint into
/// a shared slot for post-handshake validation by the caller. Always accepts the
/// TLS cert — actual trust decisions are made after the peer ID is known.
struct PinningCertVerifier {
    fingerprint_capture: Arc<std::sync::Mutex<Option<String>>>,
}

impl std::fmt::Debug for PinningCertVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinningCertVerifier").finish()
    }
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
        // Store fingerprint so the caller can validate/pin it after the handshake.
        if let Ok(mut guard) = self.fingerprint_capture.lock() {
            *guard = Some(fp.clone());
        }
        log::debug!("Peer TLS cert fingerprint: {}", fp);
        Ok(rustls::client::danger::ServerCertVerified::assertion())
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
        // Follow XDG spec: use $XDG_DATA_HOME or $HOME/.local/share
        let base = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".local").join("share")
            });
        base.join("shareflow")
    }
}

/// Returns the current Windows username in DOMAIN\user format for icacls.
#[cfg(target_os = "windows")]
fn whoami_windows() -> String {
    // USERDOMAIN\USERNAME is the format icacls expects.
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "UNKNOWN".into());
    if domain.is_empty() { user } else { format!("{}\\{}", domain, user) }
}
