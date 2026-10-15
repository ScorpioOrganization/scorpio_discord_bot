use axum::http::{Method, StatusCode, header};

use crate::auth::ROOT_USERNAME;
use crate::test_util::{IP, ROOT_PW, TestApp, TestResponse};

const USER_PW: &str = "user-password-123";

#[tokio::test]
async fn healthz_and_security_headers() {
    let app = TestApp::new().await;
    let res = app.get("/healthz", None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body, "ok");
    assert!(
        res.header(header::CONTENT_SECURITY_POLICY.as_str())
            .contains("script-src 'self'")
    );
    assert_eq!(res.header(header::X_FRAME_OPTIONS.as_str()), "DENY");
    assert_eq!(
        res.header(header::X_CONTENT_TYPE_OPTIONS.as_str()),
        "nosniff"
    );
    assert_eq!(res.header(header::CACHE_CONTROL.as_str()), "no-store");
}

#[tokio::test]
async fn pages_require_login() {
    let app = TestApp::new().await;
    for path in ["/", "/account", "/users"] {
        assert_eq!(app.get(path, None).await.redirect(), "/login", "{path}");
    }
    assert_eq!(app.post("/logout", None, &[]).await.redirect(), "/login");
    let form = [("username", "x"), ("password", "y"), ("confirm", "y")];
    assert_eq!(app.post("/users", None, &form).await.redirect(), "/login");
}

#[tokio::test]
async fn login_rejects_wrong_password() {
    let app = TestApp::new().await;
    for (user, pw) in [(ROOT_USERNAME, "wrong-password"), ("nobody", ROOT_PW)] {
        let res = app.login_from(IP, user, pw).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
        assert!(res.body.contains("Invalid username or password."));
        assert!(res.header(header::SET_COOKIE.as_str()).is_empty());
    }
}

#[tokio::test]
async fn login_sets_hardened_cookie() {
    let app = TestApp::new().await;
    let res = app.login_from(IP, ROOT_USERNAME, ROOT_PW).await;
    assert_eq!(res.redirect(), "/");
    let cookie = res.header(header::SET_COOKIE.as_str());
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert!(!cookie.contains("Secure"), "{cookie}");

    let session = cookie.split(';').next().unwrap();
    let res = app.get("/", Some(session)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("<strong>root</strong>"));
}

#[tokio::test]
async fn login_page_redirects_when_logged_in() {
    let app = TestApp::new().await;
    assert_eq!(app.get("/login", None).await.status, StatusCode::OK);
    let cookie = app.login_root().await;
    assert_eq!(app.get("/login", Some(&cookie)).await.redirect(), "/");
}

#[tokio::test]
async fn login_throttled_per_ip() {
    let app = TestApp::new().await;
    let attacker = "203.0.113.7";
    for _ in 0..5 {
        let res = app
            .login_from(attacker, ROOT_USERNAME, "wrong-password")
            .await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    }
    // Locked out even with the right password.
    let res = app.login_from(attacker, ROOT_USERNAME, ROOT_PW).await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(res.header(header::SET_COOKIE.as_str()).is_empty());

    // The username limit is higher, so one IP cannot lock the real root out.
    let res = app.login_from("198.51.100.1", ROOT_USERNAME, ROOT_PW).await;
    assert_eq!(res.redirect(), "/");
}

/// Password change with a wrong current password, so a request that gets through only
/// produces a flash message.
async fn change_password(app: &TestApp, cookie: &str, csrf: Option<&str>) -> TestResponse {
    let mut form = vec![
        ("current", "wrong-password"),
        ("new", "new-password-123"),
        ("confirm", "new-password-123"),
    ];
    if let Some(csrf) = csrf {
        form.push(("_csrf", csrf));
    }
    app.post("/account/password", Some(cookie), &form).await
}

#[tokio::test]
async fn csrf_required() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let other = app.login_root().await;
    let other_csrf = app.csrf(&other).await;
    let forged = "0".repeat(64);

    for csrf in [
        None,
        Some(""),
        Some(forged.as_str()),
        Some(other_csrf.as_str()),
    ] {
        let res = change_password(&app, &cookie, csrf).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{csrf:?}");
    }

    let csrf = app.csrf(&cookie).await;
    let res = change_password(&app, &cookie, Some(&csrf)).await;
    assert_eq!(res.redirect(), "/account?msg=pw_wrong");
}

#[tokio::test]
async fn password_change_signs_out_other_sessions() {
    let app = TestApp::new().await;
    let current = app.login_root().await;
    let other = app.login_root().await;
    let csrf = app.csrf(&current).await;
    let form = [
        ("_csrf", csrf.as_str()),
        ("current", ROOT_PW),
        ("new", "new-password-123"),
        ("confirm", "new-password-123"),
    ];
    let res = app.post("/account/password", Some(&current), &form).await;
    assert_eq!(res.redirect(), "/account?msg=pw_changed");

    assert_eq!(app.get("/", Some(&current)).await.status, StatusCode::OK);
    assert_eq!(app.get("/", Some(&other)).await.redirect(), "/login");

    let res = app.login_from(IP, ROOT_USERNAME, ROOT_PW).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    app.login(ROOT_USERNAME, "new-password-123").await;
}

#[tokio::test]
async fn logout_invalidates_session() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let res = app
        .post("/logout", Some(&cookie), &[("_csrf", csrf.as_str())])
        .await;
    assert_eq!(res.redirect(), "/login");
    assert!(
        res.header(header::SET_COOKIE.as_str())
            .contains("Max-Age=0")
    );
    // The old token is dead on the server, not only removed from the browser.
    assert_eq!(app.get("/", Some(&cookie)).await.redirect(), "/login");
}

#[tokio::test]
async fn non_root_cannot_manage_users() {
    let app = TestApp::new().await;
    let root = app.login_root().await;
    app.create_user(&root, "alice", USER_PW).await;

    let alice = app.login("alice", USER_PW).await;
    let res = app.get("/", Some(&alice)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(!res.body.contains(r#"href="/users""#));
    assert_eq!(
        app.get("/users", Some(&alice)).await.status,
        StatusCode::FORBIDDEN
    );

    let csrf = app.csrf(&alice).await;
    let root_id = app.user_id(ROOT_USERNAME).await.to_string();
    let attempts = [
        (
            "/users".to_owned(),
            vec![
                ("username", "mallory"),
                ("password", USER_PW),
                ("confirm", USER_PW),
            ],
        ),
        (
            format!("/users/{root_id}/password"),
            vec![("password", USER_PW), ("confirm", USER_PW)],
        ),
        (format!("/users/{root_id}/delete"), vec![]),
    ];
    for (path, mut form) in attempts {
        form.push(("_csrf", &csrf));
        let res = app.post(&path, Some(&alice), &form).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&app.state.db)
        .await
        .unwrap();
    assert_eq!(users, 2);
    app.login_root().await;
}

#[tokio::test]
async fn user_create_validation() {
    let app = TestApp::new().await;
    let root = app.login_root().await;
    let csrf = app.csrf(&root).await;
    app.create_user(&root, "alice", USER_PW).await;

    let cases = [
        ("a b", USER_PW, USER_PW, "user_invalid"),
        ("<script>", USER_PW, USER_PW, "user_invalid"),
        ("bob", "short", "short", "pw_short"),
        ("bob", USER_PW, "something-else", "pw_mismatch"),
        ("alice", USER_PW, USER_PW, "user_exists"),
        ("ALICE", USER_PW, USER_PW, "user_exists"),
        ("Root", USER_PW, USER_PW, "user_exists"),
    ];
    for (username, password, confirm, code) in cases {
        let form = [
            ("_csrf", csrf.as_str()),
            ("username", username),
            ("password", password),
            ("confirm", confirm),
        ];
        let res = app.post("/users", Some(&root), &form).await;
        assert_eq!(res.redirect(), format!("/users?msg={code}"), "{username}");
    }
}

#[tokio::test]
async fn root_account_protected() {
    let app = TestApp::new().await;
    let root = app.login_root().await;
    let csrf = app.csrf(&root).await;
    let root_id = app.user_id(ROOT_USERNAME).await;

    let form = [
        ("_csrf", csrf.as_str()),
        ("password", USER_PW),
        ("confirm", USER_PW),
    ];
    let res = app
        .post(&format!("/users/{root_id}/password"), Some(&root), &form)
        .await;
    assert_eq!(res.redirect(), "/users?msg=root_protected");
    let res = app
        .post(
            &format!("/users/{root_id}/delete"),
            Some(&root),
            &[("_csrf", csrf.as_str())],
        )
        .await;
    assert_eq!(res.redirect(), "/users?msg=root_protected");
    app.login_root().await;

    let res = app
        .post(
            "/users/9999/delete",
            Some(&root),
            &[("_csrf", csrf.as_str())],
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_or_resetting_user_ends_their_sessions() {
    let app = TestApp::new().await;
    let root = app.login_root().await;
    let csrf = app.csrf(&root).await;
    app.create_user(&root, "alice", USER_PW).await;
    app.create_user(&root, "bob", USER_PW).await;

    let alice = app.login("alice", USER_PW).await;
    let alice_id = app.user_id("alice").await;
    let form = [
        ("_csrf", csrf.as_str()),
        ("password", "reset-password-123"),
        ("confirm", "reset-password-123"),
    ];
    let res = app
        .post(&format!("/users/{alice_id}/password"), Some(&root), &form)
        .await;
    assert_eq!(res.redirect(), "/users?msg=user_pw_reset");
    assert_eq!(app.get("/", Some(&alice)).await.redirect(), "/login");
    app.login("alice", "reset-password-123").await;

    let bob = app.login("bob", USER_PW).await;
    let bob_id = app.user_id("bob").await;
    let res = app
        .post(
            &format!("/users/{bob_id}/delete"),
            Some(&root),
            &[("_csrf", csrf.as_str())],
        )
        .await;
    assert_eq!(res.redirect(), "/users?msg=user_deleted");
    assert_eq!(app.get("/", Some(&bob)).await.redirect(), "/login");
    let res = app.login_from(IP, "bob", USER_PW).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn guild_routes_404_without_discord() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;

    for path in ["/guilds/1", "/guilds/1/modules/greeting"] {
        let res = app.get(path, Some(&cookie)).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
    let posts = [
        ("/guilds/1/modules/greeting/toggle", vec![("enabled", "1")]),
        (
            "/guilds/1/modules/greeting",
            vec![
                ("channel_id", ""),
                ("template", "hi {user}"),
                ("embed_color", "#7f8cff"),
            ],
        ),
        ("/guilds/1/modules/greeting/test", vec![]),
    ];
    for (path, mut form) in posts {
        form.push(("_csrf", &csrf));
        let res = app.post(path, Some(&cookie), &form).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
    let db = &app.state.db;
    assert!(!crate::modules::is_enabled(db, 1, "greeting").await.unwrap());
    let saved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM greeting_config")
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(saved, 0);
}

#[tokio::test]
async fn unknown_routes_and_methods() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let res = app.get("/does-not-exist", Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app.request(Method::PUT, "/", Some(&cookie), &[], IP).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn flash_ignores_unknown_codes() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;

    let res = app.get("/account?msg=pw_changed", Some(&cookie)).await;
    assert!(res.body.contains("Password changed."));

    let res = app
        .get(
            "/account?msg=%3Cscript%3Ealert(1)%3C%2Fscript%3E",
            Some(&cookie),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(!res.body.contains("alert(1)"));
    assert!(!res.body.contains(r#"class="flash"#));
}
