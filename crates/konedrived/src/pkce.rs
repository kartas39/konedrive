//! PKCE (RFC 7636, S256) and random `state` values.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// 32 random bytes, base64url without padding: 43 characters, all RFC 3986 unreserved.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("the OS random number generator failed");
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::from_verifier(random_token())
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc7636_appendix_b() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(pkce.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn random_tokens_are_43_unreserved_chars_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 43);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
    }

    #[test]
    fn new_pkce_is_self_consistent() {
        let pkce = Pkce::new();
        assert_eq!(Pkce::from_verifier(pkce.verifier.clone()).challenge, pkce.challenge);
    }
}
