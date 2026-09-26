use anyhow::Context;
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::aead::{OsRng, rand_core::RngCore};
use shared_router::{
    AppState,
    config::{self, Config},
    db,
};
use std::{
    future::IntoFuture,
    io::{self, IsTerminal, Read},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
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
/// Upper bound for the `healthcheck` command's readiness request.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "\
Usage: shared-router [COMMAND]

Commands:
  serve          Run the router (the default when no command is given)
  generate-key   Print a new random ENCRYPTION_KEY
  hash-password  Read a password from standard input and print its ADMIN_PASSWORD_HASH
  backup PATH    Write a consistent copy of the database to a new file at PATH
  healthcheck    Exit 0 if the local router reports ready, 1 otherwise
  help           Print this help (also -h, --help)
";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "shared_router=info".into()),
        )
        .init();
    let args: Option<Vec<String>> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string().ok())
        .collect();
    let args = args.unwrap_or_default();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => serve().await,
        ["generate-key"] => {
            generate_key();
            Ok(())
        }
        ["hash-password"] => hash_password(),
        ["backup", destination] if !destination.starts_with('-') => backup(destination).await,
        ["healthcheck"] => healthcheck().await,
        ["help" | "-h" | "--help"] => {
            print!("{USAGE}");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn generate_key() {
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(key.as_mut());
    println!("{}", STANDARD.encode(*key));
}

fn hash_password() -> anyhow::Result<()> {
    // Reading from a terminal would echo the password, so only piped input is accepted.
    anyhow::ensure!(
        !io::stdin().is_terminal(),
        "hash-password reads the password from standard input and does not accept typed input, \
         which would be echoed. Pipe it in instead, for example: \
         printf '%s' \"$router_password\" | shared-router hash-password"
    );
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
    Ok(())
}

async fn backup(destination: &str) -> anyhow::Result<()> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| config::DEFAULT_DATABASE_URL.into());
    db::backup(&url, std::path::Path::new(destination)).await?;
    println!("Database backup created");
    Ok(())
}

/// Container health probe: requests `/readyz` from the router on its configured port.
async fn healthcheck() -> anyhow::Result<()> {
    let mut address = config::bind_address(std::env::var("BIND_ADDRESS").ok())?;
    if address.ip().is_unspecified() {
        let loopback: IpAddr = if address.is_ipv4() {
            Ipv4Addr::LOCALHOST.into()
        } else {
            Ipv6Addr::LOCALHOST.into()
        };
        address.set_ip(loopback);
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HEALTHCHECK_TIMEOUT)
        .build()?;
    let status = client
        .get(format!("http://{address}/readyz"))
        .send()
        .await
        .with_context(|| format!("Could not reach the router at {address}"))?
        .status();
    anyhow::ensure!(status.is_success(), "The router is not ready ({status})");
    Ok(())
}

async fn serve() -> anyhow::Result<()> {
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
