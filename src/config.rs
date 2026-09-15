use base64::{Engine, engine::general_purpose::STANDARD};
use std::{net::SocketAddr, time::Duration};
use zeroize::Zeroizing;

pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub public_origin: String,
    pub password_hash: String,
    pub encryption_key: Zeroizing<[u8; 32]>,
    pub secure_cookie: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let required = |name: &str| {
            get(name)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("{name} must be set"))
        };
        let public_origin = required("PUBLIC_ORIGIN")?;
        let origin = url::Url::parse(&public_origin)?;
        let local = origin
            .host_str()
            .is_some_and(|v| matches!(v, "localhost" | "127.0.0.1" | "[::1]"));
        anyhow::ensure!(
            origin.scheme() == "https" || (local && origin.scheme() == "http"),
            "PUBLIC_ORIGIN must use HTTPS (HTTP is allowed only on loopback)"
        );
        anyhow::ensure!(
            origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none()
                && origin.username().is_empty()
                && origin.password().is_none(),
            "PUBLIC_ORIGIN must be an origin without path or credentials"
        );
        let encoded = Zeroizing::new(required("ENCRYPTION_KEY")?);
        let decoded = Zeroizing::new(
            STANDARD
                .decode(encoded.trim())
                .map_err(|_| anyhow::anyhow!("ENCRYPTION_KEY must be base64-encoded"))?,
        );
        let mut encryption_key = Zeroizing::new([0u8; 32]);
        anyhow::ensure!(
            decoded.len() == 32,
            "ENCRYPTION_KEY must contain 32 bytes encoded as base64"
        );
        encryption_key.copy_from_slice(&decoded);
        let password_hash = required("ADMIN_PASSWORD_HASH")?.trim().to_owned();
        let parsed = argon2::PasswordHash::new(&password_hash)
            .map_err(|_| anyhow::anyhow!("invalid admin password hash"))?;
        anyhow::ensure!(
            parsed.algorithm.as_str() == "argon2id",
            "admin password must use Argon2id"
        );
        Ok(Self {
            bind: get("BIND_ADDRESS")
                .unwrap_or_else(|| "127.0.0.1:8080".into())
                .parse()?,
            database_url: get("DATABASE_URL")
                .unwrap_or_else(|| "sqlite://data/router.sqlite".into()),
            public_origin: origin.origin().ascii_serialization(),
            password_hash,
            encryption_key,
            secure_cookie: origin.scheme() == "https",
        })
    }
}

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .timeout(Duration::from_secs(1800))
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    use std::collections::HashMap;

    fn values() -> HashMap<&'static str, String> {
        let hash = Argon2::default()
            .hash_password(
                b"test-owner-password",
                &SaltString::encode_b64(b"test-salt-16-byte").unwrap(),
            )
            .unwrap()
            .to_string();
        HashMap::from([
            ("PUBLIC_ORIGIN", "https://router.example.com".into()),
            ("ENCRYPTION_KEY", STANDARD.encode([7; 32])),
            ("ADMIN_PASSWORD_HASH", hash),
        ])
    }

    #[test]
    fn reads_direct_values_without_secret_files() {
        let values = values();
        let config = Config::from_lookup(|name| values.get(name).cloned()).unwrap();
        assert_eq!(*config.encryption_key, [7; 32]);
        assert_eq!(config.password_hash, values["ADMIN_PASSWORD_HASH"]);
        assert!(config.secure_cookie);
    }

    #[test]
    fn rejects_missing_empty_malformed_and_wrong_length_secrets() {
        let valid = values();
        for field in ["ENCRYPTION_KEY", "ADMIN_PASSWORD_HASH"] {
            for value in [None, Some(""), Some("not-a-secret-value")] {
                let mut values = valid.clone();
                match value {
                    Some(value) => {
                        values.insert(field, value.into());
                    }
                    None => {
                        values.remove(field);
                    }
                }
                assert!(Config::from_lookup(|name| values.get(name).cloned()).is_err());
            }
        }
        let mut values = valid;
        values.insert("ENCRYPTION_KEY", STANDARD.encode([7; 16]));
        assert!(Config::from_lookup(|name| values.get(name).cloned()).is_err());
    }
}
