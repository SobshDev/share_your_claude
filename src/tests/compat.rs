//! Fixtures that pin on-disk compatibility across dependency upgrades. The values were
//! produced by chacha20poly1305 0.10 and argon2 0.5 and must stay readable after upgrades.
use crate::{auth, oauth};
use base64::{Engine, engine::general_purpose::STANDARD};

/// Test-only `ENCRYPTION_KEY`.
const KEY: [u8; 32] = [7; 32];

/// `oauth::encrypt(&KEY, ..)` output for the tokens below: a 24-byte nonce, then the
/// `XChaCha20Poly1305` ciphertext and tag.
const STORED_TOKENS: &str = "WZApIg3n3A+FaXUpovEDoO+fRRa1IST2WPNWNLbzVQQGZ1N7elD5eD2JdD9bbOe9rGS6DKRr0cfio34ckIsP7spbDhCMIAe3DmJ585mkAQzDBfFe8zoPtDKV6+iQjOqGqyUdmm5llUICwR9GJhnmvM5Czgcq6xQ=";

/// Argon2id PHC hash of `PASSWORD` with the default parameters.
const STORED_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$Zml4dHVyZS1zYWx0LTE2Yg$AcCMe/zXCfJTkXdvsZjk/0kAlkm3LvcE2xdxzsLJYB0";
const PASSWORD: &[u8] = b"correct horse battery staple";

#[test]
fn stored_credential_still_decrypts() {
    let stored = STANDARD.decode(STORED_TOKENS).unwrap();
    let tokens = oauth::decrypt(&KEY, &stored).unwrap();
    assert_eq!(tokens.access_token, "fixture-access-token");
    assert_eq!(tokens.refresh_token, "fixture-refresh-token");

    assert!(oauth::decrypt(&[8; 32], &stored).is_err());
    let mut tampered = stored.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(oauth::decrypt(&KEY, &tampered).is_err());
}

#[test]
fn encrypt_round_trips() {
    let tokens = oauth::Tokens {
        access_token: "a".into(),
        refresh_token: "r".into(),
    };
    let first = oauth::encrypt(&KEY, &tokens).unwrap();
    let second = oauth::encrypt(&KEY, &tokens).unwrap();
    assert_ne!(
        first[..24],
        second[..24],
        "every encryption draws a fresh nonce"
    );
    assert_eq!(oauth::decrypt(&KEY, &first).unwrap().refresh_token, "r");
}

#[test]
fn stored_password_hash_still_verifies() {
    assert!(auth::verify_password(STORED_HASH, PASSWORD));
    assert!(!auth::verify_password(STORED_HASH, b"wrong password"));
}
