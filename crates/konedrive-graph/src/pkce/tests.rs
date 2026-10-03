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
