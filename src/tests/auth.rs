//! Client key authentication, admin/client isolation, key revocation, admin sign-in and
//! throttling, admin sessions, and the CSRF and origin checks.
use super::*;
use crate::auth;

#[tokio::test]
async fn auth_conflicts_admin_isolation_and_revocation() {
    let h = Harness::new().await;
    let r = h
        .get(
            "/v1/models",
            &[
                ("x-api-key", h.key.as_str()),
                ("authorization", "Bearer different"),
            ],
        )
        .await;
    assert_eq!(r.status(), 401);
    let r = h
        .get("/admin/api/keys", &[("x-api-key", h.key.as_str())])
        .await;
    assert_eq!(r.status(), 401);
    assert_eq!(
        h.admin(
            &format!("/admin/api/keys/{}/models", h.key_id),
            "PUT",
            Some(json!({"models":[]}))
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        403
    );
    assert_eq!(
        h.admin(&format!("/admin/api/keys/{}", h.key_id), "DELETE", None)
            .await
            .status(),
        204
    );
    assert_eq!(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await
            .status(),
        401
    );
    assert_eq!(h.mock.requests.load(Ordering::SeqCst), 0);
}

/// One sign-in attempt from `client`, as forwarded by one trusted proxy hop.
async fn login_from(router: &Router, forwarded_for: &str, password: &str) -> Response {
    send_to(
        router,
        "POST",
        "/admin/api/login",
        &[
            ("origin", ORIGIN),
            ("content-type", "application/json"),
            ("x-forwarded-for", forwarded_for),
        ],
        json!({"password":password}).to_string(),
    )
    .await
}

/// A router that trusts one proxy hop, so `X-Forwarded-For` identifies the client.
fn behind_proxy(h: &Harness) -> Router {
    h.variant(|state| state.oauth.get_mut().login.trusted_proxy_hops = 1)
        .1
}

#[tokio::test]
async fn login_throttles_failures_per_client() {
    let h = Harness::new().await;
    let router = behind_proxy(&h);
    const A: &str = "198.51.100.1";
    for _ in 0..auth::LoginLimiter::CLIENT_FAILURES {
        assert_eq!(login_from(&router, A, "wrong").await.status(), 401);
    }
    let throttled = login_from(&router, A, ADMIN_PASSWORD).await;
    assert_eq!(throttled.status(), 429);
    assert_eq!(
        json_body(throttled).await["error"]["type"],
        "rate_limit_error"
    );
    // Only the last hop is trusted, so a client cannot escape by prepending addresses.
    let spoofed = format!("203.0.113.7, {A}");
    assert_eq!(login_from(&router, &spoofed, "wrong").await.status(), 429);
    // Another client is unaffected.
    const B: &str = "198.51.100.2";
    assert_eq!(login_from(&router, B, "wrong").await.status(), 401);
    let response = login_from(&router, B, ADMIN_PASSWORD).await;
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
}

#[tokio::test]
async fn successful_login_clears_the_clients_failures() {
    let h = Harness::new().await;
    let router = behind_proxy(&h);
    const A: &str = "2001:db8::1";
    for _ in 1..auth::LoginLimiter::CLIENT_FAILURES {
        assert_eq!(login_from(&router, A, "wrong").await.status(), 401);
    }
    assert_eq!(login_from(&router, A, ADMIN_PASSWORD).await.status(), 200);
    // Another address in the same IPv6 /64 is the same client.
    const SAME: &str = "2001:db8::2";
    for _ in 0..auth::LoginLimiter::CLIENT_FAILURES {
        assert_eq!(login_from(&router, SAME, "wrong").await.status(), 401);
    }
    assert_eq!(login_from(&router, A, ADMIN_PASSWORD).await.status(), 429);
}

#[test]
fn login_limiter_windows_global_ceiling_and_size_bound() {
    use auth::LoginLimiter;
    let a = Some("198.51.100.1".parse().unwrap());
    let b = Some("198.51.100.2".parse().unwrap());
    let mut limiter = LoginLimiter::new(0);
    for _ in 0..LoginLimiter::CLIENT_FAILURES {
        assert!(limiter.begin(a, 1000));
    }
    assert!(!limiter.begin(a, 1059));
    assert!(limiter.begin(b, 1059));
    // A client's window resets after WINDOW_SECS.
    assert!(limiter.begin(a, 1000 + LoginLimiter::WINDOW_SECS));

    // The global ceiling stops distributed guessing, and a success releases its reservation.
    let mut limiter = LoginLimiter::new(0);
    let client = |i: u32| Some(std::net::IpAddr::from([10, 0, (i >> 8) as u8, i as u8]));
    for i in 0..LoginLimiter::GLOBAL_FAILURES {
        assert!(limiter.begin(client(i), 5000));
    }
    assert!(!limiter.begin(client(1000), 5000));
    limiter.succeeded(client(0), 5000);
    assert!(limiter.begin(client(1000), 5000));
    assert!(!limiter.begin(client(1001), 5000));
    assert!(limiter.begin(client(1001), 5000 + LoginLimiter::WINDOW_SECS));

    // The map never tracks more than MAX_CLIENTS addresses.
    let mut limiter = LoginLimiter::new(0);
    for i in 0..LoginLimiter::MAX_CLIENTS as u32 + 100 {
        assert!(limiter.begin(client(i), i64::from(i) * LoginLimiter::WINDOW_SECS));
        assert!(limiter.tracked_clients() <= LoginLimiter::MAX_CLIENTS);
    }
}

#[test]
fn login_limiter_identifies_clients() {
    use auth::LoginLimiter;
    let peer: std::net::SocketAddr = "192.0.2.10:4000".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.append(
        "x-forwarded-for",
        "203.0.113.1, 198.51.100.1".parse().unwrap(),
    );
    headers.append("x-forwarded-for", "198.51.100.2".parse().unwrap());
    let ip = |s: &str| Some(s.parse::<std::net::IpAddr>().unwrap());
    assert_eq!(
        LoginLimiter::new(0).client(&headers, Some(peer)),
        ip("192.0.2.10")
    );
    assert_eq!(
        LoginLimiter::new(1).client(&headers, Some(peer)),
        ip("198.51.100.2")
    );
    assert_eq!(
        LoginLimiter::new(2).client(&headers, Some(peer)),
        ip("198.51.100.1")
    );
    assert_eq!(
        LoginLimiter::new(4).client(&headers, Some(peer)),
        ip("192.0.2.10")
    );
    assert_eq!(LoginLimiter::new(0).client(&headers, None), None);
    let mut v6 = HeaderMap::new();
    v6.insert("x-forwarded-for", "2001:db8:1:2:3:4:5:6".parse().unwrap());
    assert_eq!(LoginLimiter::new(1).client(&v6, None), ip("2001:db8:1:2::"));
    v6.insert("x-forwarded-for", "::ffff:198.51.100.9".parse().unwrap());
    assert_eq!(LoginLimiter::new(1).client(&v6, None), ip("198.51.100.9"));
}

#[tokio::test]
async fn changing_the_owner_password_revokes_sessions() {
    use argon2::PasswordHasher;
    let h = Harness::new().await;
    let session = sign_in(&h.router).await;
    let rotated = argon2::Argon2::default()
        .hash_password(
            b"a-different-test-password",
            &argon2::password_hash::SaltString::encode_b64(b"other-salt-16-by").unwrap(),
        )
        .unwrap()
        .to_string();
    let (_, router) = h.variant(|state| state.config.password_hash = rotated);
    let me = send_to(
        &router,
        "GET",
        "/admin/api/me",
        &[("cookie", session.cookie.as_str())],
        Body::empty(),
    )
    .await;
    assert_eq!(me.status(), 401);
    let write = admin_as(
        &router,
        &session,
        "POST",
        "/admin/api/people",
        Some(json!({"name":"Sam"})),
    )
    .await;
    assert_eq!(write.status(), 401);
    let response = send_to(
        &router,
        "POST",
        "/admin/api/login",
        &[("origin", ORIGIN), ("content-type", "application/json")],
        json!({"password":"a-different-test-password"}).to_string(),
    )
    .await;
    assert_eq!(response.status(), 200);
}

async fn last_used(h: &Harness) -> Option<String> {
    sqlx::query_scalar("SELECT last_used_at FROM api_key WHERE id=?")
        .bind(&h.key_id)
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

async fn set_last_used(h: &Harness, seconds_ago: i64) -> String {
    let value = (chrono::Utc::now() - chrono::Duration::seconds(seconds_ago))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query("UPDATE api_key SET last_used_at=? WHERE id=?")
        .bind(&value)
        .bind(&h.key_id)
        .execute(&h.state.db)
        .await
        .unwrap();
    value
}

#[tokio::test]
async fn last_used_is_written_at_most_once_a_minute() {
    let h = Harness::new().await;
    async fn models(h: &Harness) -> StatusCode {
        h.get("/v1/models", &[("x-api-key", h.key.as_str())])
            .await
            .status()
    }
    assert_eq!(last_used(&h).await, None);
    assert_eq!(models(&h).await, 200);
    let first = last_used(&h).await.expect("first use is recorded");
    assert_eq!(models(&h).await, 200);
    assert_eq!(last_used(&h).await.as_ref(), Some(&first));
    let recent = set_last_used(&h, 30).await;
    assert_eq!(models(&h).await, 200);
    assert_eq!(last_used(&h).await, Some(recent));
    let old = set_last_used(&h, 120).await;
    assert_eq!(models(&h).await, 200);
    assert!(last_used(&h).await.unwrap() > old);
}

#[tokio::test]
async fn key_authentication_survives_a_failed_last_used_write() {
    let h = Harness::new().await;
    sqlx::query(
        "CREATE TRIGGER block_last_used BEFORE UPDATE OF last_used_at ON api_key \
         BEGIN SELECT RAISE(ABORT,'blocked'); END",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    let response = h.get("/v1/models", &[("x-api-key", h.key.as_str())]).await;
    assert_eq!(response.status(), 200);
    assert_eq!(last_used(&h).await, None);
}
