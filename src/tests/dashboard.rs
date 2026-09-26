//! Smoke tests for the server-rendered admin dashboard pages, their static assets, and the
//! markup rules the Content-Security-Policy depends on.
use super::*;

const TEMPLATES: [(&str, &str); 2] = [
    (
        "dashboard.html",
        include_str!("../../templates/dashboard.html"),
    ),
    ("login.html", include_str!("../../templates/login.html")),
];
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

fn header_value<'a>(response: &'a Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

/// Checks the headers every dashboard response relies on.
fn assert_page_headers(response: &Response, content_type: &str) {
    assert_eq!(header_value(response, "content-type"), content_type);
    assert_eq!(header_value(response, "x-content-type-options"), "nosniff");
    let csp = header_value(response, "content-security-policy");
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
    ] {
        assert!(csp.contains(directive), "{csp}");
    }
}

#[tokio::test]
async fn pages_redirect_or_render_with_security_headers() {
    let h = Harness::new().await;
    let root = h.get("/", &[]).await;
    assert!(root.status().is_redirection());
    assert_eq!(header_value(&root, "location"), "/admin");
    let anonymous = h.get("/admin", &[]).await;
    assert!(anonymous.status().is_redirection());
    assert_eq!(header_value(&anonymous, "location"), "/admin/login");
    let forged = h
        .get("/admin", &[("cookie", "router_session=forged")])
        .await;
    assert_eq!(header_value(&forged, "location"), "/admin/login");
    let login = h.get("/admin/login", &[]).await;
    assert_eq!(login.status(), 200);
    assert_page_headers(&login, "text/html; charset=utf-8");
    assert!(text_body(login).await.contains("id=\"login-form\""));
    let cookie = h.session().await.cookie.clone();
    let dashboard = h.get("/admin", &[("cookie", cookie.as_str())]).await;
    assert_eq!(dashboard.status(), 200);
    assert_page_headers(&dashboard, "text/html; charset=utf-8");
    assert!(text_body(dashboard).await.contains("/assets/app.js"));
}

#[tokio::test]
async fn assets_are_served_with_their_content_types() {
    let h = Harness::new().await;
    for (path, content_type, body) in [
        ("/assets/app.js", JS, include_str!("../../static/app.js")),
        (
            "/assets/analytics.js",
            JS,
            include_str!("../../static/analytics.js"),
        ),
        ("/assets/app.css", CSS, include_str!("../../static/app.css")),
    ] {
        // Assets load on the login page too, so they must not require a session.
        let response = h.get(path, &[]).await;
        assert_eq!(response.status(), 200, "{path}");
        assert_page_headers(&response, content_type);
        assert_eq!(text_body(response).await, body, "{path}");
    }
    // Every asset a template references is served with the type its extension implies.
    for (name, template) in TEMPLATES {
        for path in referenced_assets(template) {
            let response = h.get(&path, &[]).await;
            assert_eq!(response.status(), 200, "{name} references {path}");
            let expected = if path.ends_with(".js") { JS } else { CSS };
            assert_page_headers(&response, expected);
        }
    }
}

/// Every `/assets/...` URL in a `src` or `href` attribute.
fn referenced_assets(template: &str) -> Vec<String> {
    let mut assets = Vec::new();
    for attribute in ["src=\"", "href=\""] {
        for (index, _) in template.match_indices(attribute) {
            let value = &template[index + attribute.len()..];
            let value = &value[..value.find('"').unwrap()];
            if value.starts_with("/assets/") {
                assets.push(value.to_owned());
            }
        }
    }
    assert!(!assets.is_empty());
    assets
}

/// Markup that `script-src 'self'; style-src 'self'` would block at runtime: inline
/// scripts, `<style>` blocks, `style=` attributes, `on*=` handlers, and `javascript:` URLs.
fn csp_violations(template: &str) -> Vec<String> {
    let lower = template.to_ascii_lowercase();
    let mut violations = Vec::new();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find('<') {
        let end = rest[start..]
            .find('>')
            .map_or(rest.len(), |e| start + e + 1);
        let tag = &rest[start..end];
        rest = &rest[end..];
        if tag.starts_with("<style") {
            violations.push(tag.to_owned());
        }
        if tag.starts_with("<script") && !tag.contains(" src=") {
            violations.push(tag.to_owned());
        }
        if tag.contains("javascript:") {
            violations.push(tag.to_owned());
        }
        // Attribute names follow whitespace inside the tag.
        for (index, _) in tag.match_indices(char::is_whitespace) {
            let name: String = tag[index + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            let is_handler = name.len() > 2 && name.starts_with("on");
            let assigned = tag[index + 1 + name.len()..].trim_start().starts_with('=');
            if assigned && (name == "style" || is_handler) {
                violations.push(tag.to_owned());
            }
        }
    }
    violations
}

#[test]
fn templates_contain_nothing_the_csp_blocks() {
    for (name, template) in TEMPLATES {
        assert_eq!(csp_violations(template), Vec::<String>::new(), "{name}");
    }
    // The scanner itself catches each blocked construct.
    for markup in [
        "<script>alert(1)</script>",
        "<style>p{}</style>",
        "<p style=\"color:red\">",
        "<button\n  onclick=\"go()\">",
        "<img src=\"/a.png\" ONERROR = \"x()\">",
        "<a href=\"javascript:go()\">",
    ] {
        assert!(!csp_violations(markup).is_empty(), "{markup}");
    }
    for markup in [
        "<script src=\"/assets/app.js\" defer></script>",
        "<p data-style=\"x\" class=\"one\">on=off</p>",
        "<input id=\"password\" name=\"on\" />",
    ] {
        assert!(csp_violations(markup).is_empty(), "{markup}");
    }
}
