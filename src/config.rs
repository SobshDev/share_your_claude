use anyhow::Context;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{net::SocketAddr, str::FromStr, time::Duration};
use zeroize::Zeroizing;

/// Database used when `DATABASE_URL` is not set.
pub const DEFAULT_DATABASE_URL: &str = "sqlite://data/router.sqlite";
/// Listen address used when `BIND_ADDRESS` is not set.
pub const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1:8080";

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
        let origin = url::Url::parse(&public_origin)
            .context("PUBLIC_ORIGIN must be an absolute URL such as https://router.example.com")?;
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
            .map_err(|_| anyhow::anyhow!("ADMIN_PASSWORD_HASH must be a PHC-format hash"))?;
        anyhow::ensure!(
            parsed.algorithm.as_str() == "argon2id",
            "ADMIN_PASSWORD_HASH must be an Argon2id hash"
        );
        let database_url = get("DATABASE_URL").unwrap_or_else(|| DEFAULT_DATABASE_URL.into());
        let database_error =
            || format!("DATABASE_URL must be a SQLite URL such as {DEFAULT_DATABASE_URL}");
        anyhow::ensure!(database_url.starts_with("sqlite:"), database_error());
        sqlx::sqlite::SqliteConnectOptions::from_str(&database_url).with_context(database_error)?;
        Ok(Self {
            bind: bind_address(get("BIND_ADDRESS"))?,
            database_url,
            public_origin: origin.origin().ascii_serialization(),
            password_hash,
            encryption_key,
            secure_cookie: origin.scheme() == "https",
        })
    }
}

/// Parses `BIND_ADDRESS`, falling back to the default when it is unset.
pub fn bind_address(value: Option<String>) -> anyhow::Result<SocketAddr> {
    value
        .as_deref()
        .unwrap_or(DEFAULT_BIND_ADDRESS)
        .trim()
        .parse()
        .with_context(|| {
            format!("BIND_ADDRESS must be an IP address and port such as {DEFAULT_BIND_ADDRESS}")
        })
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
    use argon2::{Algorithm, Argon2, Params, PasswordHasher, Version, password_hash::SaltString};
    use std::collections::HashMap;

    fn hash(algorithm: Algorithm) -> String {
        Argon2::new(algorithm, Version::V0x13, Params::default())
            .hash_password(
                b"test-owner-password",
                &SaltString::encode_b64(b"test-salt-16-byte").unwrap(),
            )
            .unwrap()
            .to_string()
    }

    fn values() -> HashMap<&'static str, String> {
        HashMap::from([
            ("PUBLIC_ORIGIN", "https://router.example.com".into()),
            ("ENCRYPTION_KEY", STANDARD.encode([7; 32])),
            ("ADMIN_PASSWORD_HASH", hash(Algorithm::Argon2id)),
        ])
    }

    fn load(values: &HashMap<&'static str, String>) -> anyhow::Result<Config> {
        Config::from_lookup(|name| values.get(name).cloned())
    }

    fn with(field: &'static str, value: &str) -> HashMap<&'static str, String> {
        let mut values = values();
        values.insert(field, value.into());
        values
    }

    /// Loading fails, and the error names the variable that caused it.
    fn assert_rejected(field: &'static str, value: &str) {
        match load(&with(field, value)) {
            Ok(_) => panic!("{field}={value:?} was accepted"),
            Err(error) => assert!(
                format!("{error:#}").contains(field),
                "{field}={value:?} failed without naming it: {error:#}"
            ),
        }
    }

    #[test]
    fn reads_direct_values_without_secret_files() {
        let values = values();
        let config = load(&values).unwrap();
        assert_eq!(*config.encryption_key, [7; 32]);
        assert_eq!(config.password_hash, values["ADMIN_PASSWORD_HASH"]);
        assert!(config.secure_cookie);
        assert_eq!(config.bind, DEFAULT_BIND_ADDRESS.parse().unwrap());
        assert_eq!(config.database_url, DEFAULT_DATABASE_URL);
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
        assert_rejected("ENCRYPTION_KEY", &STANDARD.encode([7; 16]));
        assert_rejected("ENCRYPTION_KEY", "not base64!");
        assert_rejected("ADMIN_PASSWORD_HASH", "not-a-phc-hash");
    }

    #[test]
    fn requires_argon2id_for_the_admin_password() {
        assert_rejected("ADMIN_PASSWORD_HASH", &hash(Algorithm::Argon2i));
        assert_rejected("ADMIN_PASSWORD_HASH", &hash(Algorithm::Argon2d));
    }

    #[test]
    fn allows_plain_http_only_on_loopback() {
        for origin in [
            "http://localhost:8080",
            "http://127.0.0.1",
            "http://[::1]:8080",
        ] {
            let config = load(&with("PUBLIC_ORIGIN", origin)).unwrap();
            assert_eq!(config.public_origin, origin);
            assert!(!config.secure_cookie, "{origin} must not set Secure");
        }
        for origin in [
            "http://router.example.com",
            "http://127.0.0.2",
            "http://localhost.example.com",
            "ftp://localhost",
        ] {
            assert_rejected("PUBLIC_ORIGIN", origin);
        }
    }

    #[test]
    fn requires_a_bare_origin() {
        for origin in [
            "https://x/path",
            "https://x/?q",
            "https://x/#f",
            "https://u:p@x",
            "https://u@x",
            "router.example.com",
            "/relative",
        ] {
            assert_rejected("PUBLIC_ORIGIN", origin);
        }
    }

    #[test]
    fn normalizes_the_public_origin() {
        for (input, expected) in [
            ("https://Router.Example.com/", "https://router.example.com"),
            (
                "https://router.example.com:443",
                "https://router.example.com",
            ),
            (
                "https://router.example.com:8443/",
                "https://router.example.com:8443",
            ),
        ] {
            let config = load(&with("PUBLIC_ORIGIN", input)).unwrap();
            assert_eq!(config.public_origin, expected);
            assert!(config.secure_cookie);
        }
    }

    #[test]
    fn names_bad_bind_and_database_values() {
        assert_rejected("BIND_ADDRESS", "localhost:8080");
        assert_rejected("BIND_ADDRESS", "0.0.0.0");
        assert_rejected("DATABASE_URL", "postgres://db/router");
        assert_rejected("DATABASE_URL", "sqlite://data/router.sqlite?mode=bogus");
        let config = load(&with("BIND_ADDRESS", "0.0.0.0:9000")).unwrap();
        assert_eq!(config.bind, "0.0.0.0:9000".parse().unwrap());
    }
}
