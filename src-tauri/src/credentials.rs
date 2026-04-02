use aes_gcm::{Aes256Gcm, Key, Nonce};
use aes_gcm::aead::{Aead, KeyInit};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

const ENC_PREFIX: &str = "enc:";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Encrypt a credential string. Returns an `enc:<base64>` string.
/// If encryption fails, returns the plaintext unchanged (with a log warning).
/// Calling this on an already-encrypted value is a no-op.
pub fn encrypt_credential(plaintext: &str) -> String {
    if plaintext.starts_with(ENC_PREFIX) {
        return plaintext.to_string();
    }
    let key_bytes = match load_or_create_key() {
        Ok(k) => k,
        Err(e) => {
            log::error!("Cannot load encryption key — credential stored in plaintext: {}", e);
            return plaintext.to_string();
        }
    };
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce_bytes: [u8; NONCE_LEN] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);
    match cipher.encrypt(nonce, plaintext.as_bytes()) {
        Ok(ciphertext) => {
            let mut buf = Vec::with_capacity(NONCE_LEN + ciphertext.len());
            buf.extend_from_slice(&nonce_bytes);
            buf.extend_from_slice(&ciphertext);
            format!("{}{}", ENC_PREFIX, BASE64.encode(&buf))
        }
        Err(e) => {
            log::error!("Credential encryption error: {}", e);
            plaintext.to_string()
        }
    }
}

/// Decrypt a credential string produced by `encrypt_credential`.
/// Values without the `enc:` prefix are returned unchanged (migration path
/// for existing plaintext credentials).
pub fn decrypt_credential(value: &str) -> String {
    if !value.starts_with(ENC_PREFIX) {
        return value.to_string();
    }
    let encoded = &value[ENC_PREFIX.len()..];
    let data = match BASE64.decode(encoded) {
        Ok(d) => d,
        Err(_) => {
            log::warn!("Credential has enc: prefix but invalid base64 — returning raw");
            return value.to_string();
        }
    };
    if data.len() <= NONCE_LEN {
        log::warn!("Credential ciphertext too short — returning raw");
        return value.to_string();
    }
    let key_bytes = match load_or_create_key() {
        Ok(k) => k,
        Err(e) => {
            log::error!("Cannot load encryption key — returning raw credential: {}", e);
            return value.to_string();
        }
    };
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&data[..NONCE_LEN]);
    match cipher.decrypt(nonce, &data[NONCE_LEN..]) {
        Ok(plaintext) => String::from_utf8(plaintext).unwrap_or_else(|_| value.to_string()),
        Err(_) => {
            log::warn!("Credential decryption failed (wrong key?) — returning raw");
            value.to_string()
        }
    }
}

/// Load the 32-byte AES key from disk, creating and persisting a new random
/// key if the file does not exist or is corrupt.
fn load_or_create_key() -> Result<Vec<u8>, String> {
    let path = key_path();
    if path.exists() {
        let data = std::fs::read(&path)
            .map_err(|e| format!("Cannot read key file: {}", e))?;
        if data.len() == KEY_LEN {
            return Ok(data);
        }
        log::warn!("Key file has unexpected length {} — regenerating", data.len());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create key directory: {}", e))?;
    }
    let key: Vec<u8> = (0..KEY_LEN).map(|_| rand::random::<u8>()).collect();
    std::fs::write(&path, &key)
        .map_err(|e| format!("Cannot write key file: {}", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("Generated new credential encryption key at {:?}", path);
    Ok(key)
}

/// Path to the machine-local encryption key file.
fn key_path() -> std::path::PathBuf {
    let mut path = crate::core::config::dirs_config_path();
    path.push("shareflow");
    path.push(".shareflow.key");
    path
}
