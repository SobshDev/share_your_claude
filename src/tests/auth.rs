//! Client key authentication, admin/client isolation, key revocation, admin sign-in and
//! throttling, admin sessions, and the CSRF and origin checks.
use super::*;
use crate::auth;

#[tokio::test]
async fn conflicting_key_headers_are_rejected() {
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
    assert_error(r, 401, "authentication_error", AUTH_REQUIRED).await;
}

#[tokio::test]
async fn client_keys_cannot_use_the_admin_api() {
    let h = Harness::new().await;
    let r = h
        .get("/admin/api/keys", &[("x-api-key", h.key.as_str())])
        .await;
    assert_error(r, 401, "authentication_error", AUTH_REQUIRED).await;
}

#[tokio::test]
async fn removed_grants_and_revoked_keys_are_denied_before_upstream() {
    let h = Harness::new().await;
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
    assert_error(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await,
        403,
        "permission_error",
        NO_ACCESS,
    )
    .await;
    assert_eq!(
        h.admin(&format!("/admin/api/keys/{}", h.key_id), "DELETE", None)
            .await
            .status(),
        204
    );
    assert_error(
        h.request("/v1/messages", &h.key, message("hello", false))
            .await,
        401,
        "authentication_error",
        AUTH_REQUIRED,
    )
    .await;
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

/// Checks a rejected password.
async fn assert_wrong_password(response: Response) {
    assert_error(response, 401, "authentication_error", AUTH_REQUIRED).await;
}

/// Checks a sign-in refused by the login throttle.
async fn assert_throttled(response: Response) {
    assert_error(
        response,
        429,
        "rate_limit_error",
        "Too many sign-in attempts",
    )
    .await;
}

#[tokio::test]
async fn login_throttles_failures_per_client() {
    const A: &str = "198.51.100.1";
    const B: &str = "198.51.100.2";
    let h = Harness::new().await;
    let router = behind_proxy(&h);
    for _ in 0..auth::LoginLimiter::CLIENT_FAILURES {
        assert_wrong_password(login_from(&router, A, "wrong").await).await;
    }
    assert_throttled(login_from(&router, A, ADMIN_PASSWORD).await).await;
    // Only the last hop is trusted, so a client cannot escape by prepending addresses.
    let spoofed = format!("203.0.113.7, {A}");
    assert_throttled(login_from(&router, &spoofed, "wrong").await).await;
    // Another client is unaffected.
    assert_wrong_password(login_from(&router, B, "wrong").await).await;
    let response = login_from(&router, B, ADMIN_PASSWORD).await;
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
}

#[tokio::test]
async fn successful_login_clears_the_clients_failures() {
    const A: &str = "2001:db8::1";
    // Another address in the same IPv6 /64 is the same client.
    const SAME: &str = "2001:db8::2";
    let h = Harness::new().await;
    let router = behind_proxy(&h);
    for _ in 1..auth::LoginLimiter::CLIENT_FAILURES {
        assert_wrong_password(login_from(&router, A, "wrong").await).await;
    }
    assert_eq!(login_from(&router, A, ADMIN_PASSWORD).await.status(), 200);
    for _ in 0..auth::LoginLimiter::CLIENT_FAILURES {
        assert_wrong_password(login_from(&router, SAME, "wrong").await).await;
    }
    assert_throttled(login_from(&router, A, ADMIN_PASSWORD).await).await;
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
    // A client's window resets after WINDOW_SECS with a full new allowance.
    let reset = 1000 + LoginLimiter::WINDOW_SECS;
    for _ in 0..LoginLimiter::CLIENT_FAILURES {
        assert!(limiter.begin(a, reset));
    }
    assert!(!limiter.begin(a, reset));

    // The global ceiling stops distributed guessing, and a success releases its reservation.
    let mut limiter = LoginLimiter::new(0);
    let client = |i: u32| {
        let [_, _, high, low] = i.to_be_bytes();
        Some(std::net::IpAddr::from([10, 0, high, low]))
    };
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
    for i in 0..u32::try_from(LoginLimiter::MAX_CLIENTS).unwrap() + 100 {
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

/// Creates a person as the harness session, sending only the given origin and CSRF token.
async fn create_person(h: &Harness, origin: Option<&str>, csrf: Option<&str>) -> Response {
    let session = h.session().await;
    let mut headers = vec![
        ("content-type", "application/json"),
        ("cookie", session.cookie.as_str()),
    ];
    headers.extend(origin.map(|o| ("origin", o)));
    headers.extend(csrf.map(|t| ("x-csrf-token", t)));
    h.send(
        "POST",
        "/admin/api/people",
        &headers,
        json!({"name":"Sam"}).to_string(),
    )
    .await
}

#[tokio::test]
async fn admin_writes_need_both_origin_and_csrf_token() {
    let h = Harness::new().await;
    let session = h.session().await;
    let csrf = session.csrf.as_str();
    for (origin, token, message) in [
        (Some(ORIGIN), None, WRONG_CSRF),
        (Some(ORIGIN), Some("wrong-token"), WRONG_CSRF),
        (Some("https://evil.example"), Some(csrf), WRONG_ORIGIN),
        (None, Some(csrf), WRONG_ORIGIN),
        (None, None, WRONG_ORIGIN),
    ] {
        assert_error(
            create_person(&h, origin, token).await,
            403,
            "permission_error",
            message,
        )
        .await;
    }
    let people: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM person")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(people, 1);
    assert_eq!(
        create_person(&h, Some(ORIGIN), Some(csrf)).await.status(),
        201
    );
    // Reads need neither.
    assert_eq!(
        h.get("/admin/api/me", &[("cookie", session.cookie.as_str())])
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn login_requires_the_dashboard_origin() {
    let h = Harness::new().await;
    for origin in [Some("https://evil.example"), None] {
        let mut headers = vec![("content-type", "application/json")];
        headers.extend(origin.map(|o| ("origin", o)));
        let response = h
            .send(
                "POST",
                "/admin/api/login",
                &headers,
                json!({"password":ADMIN_PASSWORD}).to_string(),
            )
            .await;
        assert!(response.headers().get("set-cookie").is_none());
        assert_error(response, 403, "permission_error", WRONG_ORIGIN).await;
    }
}

#[tokio::test]
async fn sessions_expire_log_out_and_use_the_configured_cookie() {
    let h = Harness::new().await;
    for secure in [false, true] {
        let (_, router) = h.variant(|state| state.config.secure_cookie = secure);
        let (name, other) = if secure {
            ("__Host-router_session", "router_session")
        } else {
            ("router_session", "__Host-router_session")
        };
        let response = send_to(
            &router,
            "POST",
            "/admin/api/login",
            &[("origin", ORIGIN), ("content-type", "application/json")],
            json!({"password":ADMIN_PASSWORD}).to_string(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let set_cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(set_cookie.starts_with(&format!("{name}=")), "{set_cookie}");
        assert!(set_cookie.contains("; Path=/;"));
        assert_eq!(set_cookie.ends_with("; Secure"), secure, "{set_cookie}");
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        let token = cookie.split_once('=').unwrap().1.to_owned();
        let me = |cookie: String| {
            let router = router.clone();
            async move {
                send_to(
                    &router,
                    "GET",
                    "/admin/api/me",
                    &[("cookie", &cookie)],
                    Body::empty(),
                )
                .await
            }
        };
        let signed_out =
            |response| assert_error(response, 401, "authentication_error", AUTH_REQUIRED);
        assert_eq!(me(cookie.clone()).await.status(), 200);
        signed_out(me(format!("{other}={token}")).await).await;
        signed_out(me(format!("{cookie}; {cookie}")).await).await;
        let two_headers = send_to(
            &router,
            "GET",
            "/admin/api/me",
            &[("cookie", &cookie), ("cookie", &cookie)],
            Body::empty(),
        )
        .await;
        signed_out(two_headers).await;

        // Logout deletes the session and clears the cookie.
        let session = sign_in(&router).await;
        let response = admin_as(&router, &session, "POST", "/admin/api/logout", None).await;
        assert_eq!(response.status(), 204);
        let cleared = response.headers()["set-cookie"].to_str().unwrap();
        assert!(cleared.starts_with(&format!("{name}=;")));
        assert!(cleared.contains("Max-Age=0"));
        signed_out(me(session.cookie.clone()).await).await;

        // An expired session is rejected, and the dashboard sends the owner to sign in.
        sqlx::query("UPDATE admin_session SET expires_at=? WHERE token_hash=?")
            .bind(db::epoch() - 1)
            .bind(auth::hash(&token))
            .execute(&h.state.db)
            .await
            .unwrap();
        signed_out(me(cookie.clone()).await).await;
        let page = send_to(
            &router,
            "GET",
            "/admin",
            &[("cookie", &cookie)],
            Body::empty(),
        )
        .await;
        assert!(page.status().is_redirection());
        assert_eq!(page.headers()["location"], "/admin/login");
    }
}

/// Twelve hours, the lifetime of an admin session.
const SESSION_SECS: i64 = 12 * 3600;

#[tokio::test]
async fn sessions_last_twelve_hours_and_end_when_the_clock_reaches_expiry() {
    let h = Harness::new().await;
    // The login reads the clock between these two readings, so its expiry lies between
    // them plus the lifetime. Both readings are whole seconds and nearly always equal.
    let before = db::epoch();
    let response = h.login(ADMIN_PASSWORD).await;
    let after = db::epoch();
    assert_eq!(response.status(), 200);
    let set_cookie = response.headers()["set-cookie"].to_str().unwrap();
    let max_age = format!("Max-Age={SESSION_SECS}");
    assert!(
        set_cookie.split("; ").any(|attribute| attribute == max_age),
        "{set_cookie}"
    );
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    let token = cookie.split_once('=').unwrap().1;
    let expires_at = |value: i64| {
        sqlx::query("UPDATE admin_session SET expires_at=? WHERE token_hash=?")
            .bind(value)
            .bind(auth::hash(token))
            .execute(&h.state.db)
    };
    let stored: i64 = sqlx::query_scalar("SELECT expires_at FROM admin_session WHERE token_hash=?")
        .bind(auth::hash(token))
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(
        (before + SESSION_SECS..=after + SESSION_SECS).contains(&stored),
        "{before} {stored} {after}"
    );
    let headers = [("cookie", cookie.as_str())];
    let me = || h.get("/admin/api/me", &headers);
    // Move the expiry instead of waiting: a session is valid until the clock reaches it.
    expires_at(db::epoch() + 60).await.unwrap();
    assert_eq!(me().await.status(), 200);
    expires_at(db::epoch()).await.unwrap();
    assert_error(me().await, 401, "authentication_error", AUTH_REQUIRED).await;
}

#[tokio::test]
async fn changing_the_owner_password_revokes_sessions() {
    use argon2::PasswordHasher;
    let h = Harness::new().await;
    let session = sign_in(&h.router).await;
    let rotated = argon2::Argon2::default()
        .hash_password_with_salt(b"a-different-test-password", b"other-salt-16-by")
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
    assert_error(me, 401, "authentication_error", AUTH_REQUIRED).await;
    let write = admin_as(
        &router,
        &session,
        "POST",
        "/admin/api/people",
        Some(json!({"name":"Sam"})),
    )
    .await;
    assert_error(write, 401, "authentication_error", AUTH_REQUIRED).await;
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
    async fn models(h: &Harness) -> StatusCode {
        h.get("/v1/models", &[("x-api-key", h.key.as_str())])
            .await
            .status()
    }
    let h = Harness::new().await;
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
