use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use shared_router::{AppState, config::Config, db};
use std::{
    future::IntoFuture,
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// How long open requests may keep running after a shutdown signal. This stays below the
/// 30-second `stop_grace_period` in compose.yaml, leaving time to record interrupted requests
/// and close the database before the container is killed.
const DRAIN_PERIOD: Duration = Duration::from_secs(20);
/// Upper bound for closing the database pool once serving has stopped.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

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
            let url = std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| shared_router::config::DEFAULT_DATABASE_URL.into());
            db::backup(&url, std::path::Path::new(&destination)).await?;
            println!("Database backup created");
            return Ok(());
        }
        None | Some("serve") => (),
        _ => anyhow::bail!(
            "usage: shared-router [serve | generate-key | hash-password | backup PATH]"
        ),
    }
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    let address = config.bind;
    let state = AppState::new(config, pool.clone())?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address,"router listening");
    let draining = Arc::new(AtomicBool::new(false));
    let app = shared_router::app(state)
        .layer(middleware::from_fn_with_state(draining.clone(), readiness));
    let (stopping, mut stop_requested) = tokio::sync::watch::channel(false);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown().await;
        draining.store(true, Ordering::SeqCst);
        tracing::info!("shutdown requested; draining open requests");
        let _ = stopping.send(true);
    });
    let deadline = async {
        let _ = stop_requested.wait_for(|stopping| *stopping).await;
        tokio::time::sleep(DRAIN_PERIOD).await;
    };
    let served = tokio::select! {
        result = server.into_future() => result.map_err(anyhow::Error::from),
        () = deadline => {
            tracing::warn!("drain period elapsed; interrupting open requests");
            Ok(())
        }
    };
    // Requests still open are recorded now, with the shutdown time, instead of by startup
    // recovery on the next start.
    match db::interrupt_in_flight(&pool).await {
        Ok(0) => (),
        Ok(count) => tracing::warn!(count, "recorded open requests as interrupted"),
        Err(_) => tracing::error!(
            "failed to record open requests as interrupted; startup recovery will repair them"
        ),
    }
    if tokio::time::timeout(CLOSE_TIMEOUT, pool.close())
        .await
        .is_err()
    {
        tracing::warn!("timed out closing the database");
    }
    served
}

/// Reports the server as not ready once shutdown has begun, so load balancers stop routing to it.
async fn readiness(
    State(draining): State<Arc<AtomicBool>>,
    request: Request,
    next: Next,
) -> Response {
    let probe = request.uri().path() == "/readyz";
    let mut response = next.run(request).await;
    if probe && draining.load(Ordering::SeqCst) {
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        response.headers_mut().remove(header::CONTENT_LENGTH);
        *response.body_mut() = Body::from("shutting down");
    }
    response
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
