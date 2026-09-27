use anyhow::Context;
use argon2::{Argon2, PasswordHasher};
use base64::{Engine, engine::general_purpose::STANDARD};
use shared_router::{
    AppState, Phase,
    config::{self, Config},
    db,
};
use std::{
    future::IntoFuture,
    io::{self, IsTerminal, Read},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

/// How long open requests may keep running after a shutdown signal. This stays below the
/// 30-second `stop_grace_period` in compose.yaml, leaving time to record interrupted requests
/// and close the database before the container is killed.
const DRAIN_PERIOD: Duration = Duration::from_secs(20);
/// How long interrupted streams get to record their outcome and end after the drain period.
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);
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
    getrandom::fill(key.as_mut()).expect("operating system random number generator failed");
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
        .hash_password(password.as_bytes())
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
    let client = config::client_builder()?
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
    let mut phase = state.subscribe_phase();
    // Connect info lets the login limiter tell direct clients apart by peer address.
    let app = shared_router::app(state.clone())
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let signalled = state.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown().await;
        // `/readyz` now reports 503.
        signalled.set_phase(Phase::Draining);
        tracing::info!("shutdown requested; draining open requests");
    });
    let mut server = std::pin::pin!(server.into_future());
    let deadline = async {
        let _ = phase.wait_for(|phase| *phase != Phase::Serving).await;
        tokio::time::sleep(DRAIN_PERIOD).await;
    };
    let served = tokio::select! {
        result = &mut server => result.map_err(anyhow::Error::from),
        () = deadline => {
            tracing::warn!("drain period elapsed; interrupting open requests");
            // Open streams checkpoint their usage, record themselves as interrupted, and end.
            state.set_phase(Phase::Stopping);
            match tokio::time::timeout(INTERRUPT_GRACE, &mut server).await {
                Ok(result) => result.map_err(anyhow::Error::from),
                Err(_) => Ok(()),
            }
        }
    };
    // Requests still open, such as non-streaming calls, are recorded now with the shutdown
    // time instead of by startup recovery on the next start.
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
    tokio::select! { () = ctrl_c => (), () = terminate => () }
}
