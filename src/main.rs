use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::aead::{OsRng, rand_core::RngCore};
use shared_router::{AppState, config::Config, db};
use std::io::{self, Read};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "shared_router=info".into()),
        )
        .init();
    match std::env::args().nth(1).as_deref() {
        Some("generate-key") => {
            let mut key = zeroize::Zeroizing::new([0u8; 32]);
            OsRng.fill_bytes(key.as_mut());
            println!("{}", STANDARD.encode(*key));
            return Ok(());
        }
        Some("hash-password") => {
            let mut password = zeroize::Zeroizing::new(String::new());
            io::stdin().read_to_string(&mut password)?;
            let password = password.trim_end_matches(['\r', '\n']);
            anyhow::ensure!(
                password.len() >= 12 && password.len() <= 1024,
                "Use a password of 12–1024 bytes"
            );
            let hash = Argon2::default()
                .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
                .map_err(|_| anyhow::anyhow!("Could not hash password"))?
                .to_string();
            println!("{hash}");
            return Ok(());
        }
        Some("backup") => {
            let destination = std::env::args().nth(2).ok_or_else(|| {
                anyhow::anyhow!("usage: shared-router backup /path/to/new-backup.sqlite")
            })?;
            anyhow::ensure!(
                !std::path::Path::new(&destination).exists(),
                "Backup destination already exists"
            );
            let url = std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://data/router.sqlite".into());
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await?;
            sqlx::query("VACUUM INTO ?")
                .bind(&destination)
                .execute(&pool)
                .await?;
            pool.close().await;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))?;
            }
            println!("Database backup created");
            return Ok(());
        }
        None | Some("serve") => (),
        _ => anyhow::bail!(
            "usage: shared-router [serve | generate-key | hash-password | backup PATH]"
        ),
    }
    let config = Config::from_env()?;
    if config.database_url == "sqlite://data/router.sqlite" {
        std::fs::create_dir_all("data")?;
    }
    let pool = db::connect(&config.database_url).await?;
    let address = config.bind;
    let state = AppState::new(config, pool)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address,"router listening");
    axum::serve(listener, shared_router::app(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _=ctrl_c=>(),_=terminate=>() }
}
