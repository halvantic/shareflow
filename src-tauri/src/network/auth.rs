use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute HMAC-SHA256(key=pairing_code, message=nonce).
/// Used by the client when responding to an AuthChallenge.
pub fn compute_auth_hmac(pairing_code: &str, nonce: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(pairing_code.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(nonce);
    mac.finalize().into_bytes().to_vec()
}

/// Verify an AuthResponse hash against the expected HMAC in constant time.
/// Returns true if the hash is correct.
pub fn verify_auth_hmac(pairing_code: &str, nonce: &[u8], hash: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(pairing_code.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(nonce);
    mac.verify_slice(hash).is_ok()
}
