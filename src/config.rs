use base64::{Engine, engine::general_purpose::STANDARD};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
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
        let public_origin = std::env::var("PUBLIC_ORIGIN")?;
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
        let key_path = PathBuf::from(std::env::var("ENCRYPTION_KEY_FILE")?);
        let encoded = Zeroizing::new(std::fs::read_to_string(key_path)?);
        let decoded = Zeroizing::new(STANDARD.decode(encoded.trim())?);
        let mut encryption_key = Zeroizing::new([0u8; 32]);
        anyhow::ensure!(
            decoded.len() == 32,
            "encryption key must contain 32 bytes encoded as base64"
        );
        encryption_key.copy_from_slice(&decoded);
        let password_hash = std::fs::read_to_string(std::env::var("ADMIN_PASSWORD_HASH_FILE")?)?
            .trim()
            .to_owned();
        let parsed = argon2::PasswordHash::new(&password_hash)
            .map_err(|_| anyhow::anyhow!("invalid admin password hash"))?;
        anyhow::ensure!(
            parsed.algorithm.as_str() == "argon2id",
            "admin password must use Argon2id"
        );
        Ok(Self {
            bind: std::env::var("BIND_ADDRESS")
                .unwrap_or_else(|_| "127.0.0.1:8080".into())
                .parse()?,
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://data/router.sqlite".into()),
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
