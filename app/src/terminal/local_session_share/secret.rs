use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;

/// Number of random bytes used to derive a share secret. 32 bytes (256 bits)
/// of entropy, encoded as ~43 URL-safe base64 characters, is impractical to
/// guess (PRODUCT.md P26).
const SECRET_BYTES: usize = 32;

/// A high-entropy, URL-safe secret gating access to a local session share.
/// Knowing this secret is the only access check for guests in v1
/// (PRODUCT.md P16, P26).
#[derive(Clone, PartialEq, Eq)]
pub struct ShareSecret(String);

impl ShareSecret {
    /// Generates a new random secret.
    pub fn generate() -> Self {
        let mut bytes = [0u8; SECRET_BYTES];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Constant-time equality check against a candidate secret supplied by a
    /// guest, to avoid leaking timing information about the real secret.
    pub fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = candidate.as_bytes();
        if expected.len() != actual.len() {
            return false;
        }
        expected
            .iter()
            .zip(actual)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl std::fmt::Debug for ShareSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secrets must never be shown in full in logs or telemetry (PRODUCT.md P26).
        f.write_str("ShareSecret(<redacted>)")
    }
}

impl std::fmt::Display for ShareSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_secret_is_non_empty_and_high_entropy() {
        let secret = ShareSecret::generate();
        // 32 bytes base64url-no-pad encodes to 43 characters.
        assert_eq!(secret.as_str().len(), 43);
        assert!(secret
            .as_str()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn generated_secrets_are_unique() {
        let a = ShareSecret::generate();
        let b = ShareSecret::generate();
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn matches_is_exact() {
        let secret = ShareSecret::generate();
        assert!(secret.matches(secret.as_str()));
        assert!(!secret.matches("wrong-secret"));
        assert!(!secret.matches(""));
    }
}
