use axum::http::{Method, StatusCode, header};

use crate::auth::ROOT_USERNAME;
use crate::clickup::mapping::Explicit;
use crate::clickup::mock;
use crate::keys::{self, Service};
use crate::modules::calendar::{self, Kind, Rule};
use crate::modules::clickup::{explicit_mappings, set_tag_role, tag_role_rows};
use crate::modules::recurring;
use crate::projects;
use crate::secrets::SecretString;
use crate::test_util::{
    CHANNEL_ID, EMOJI_ID, GUILD_ID, GUILD_NAME, IP, LEADS_ROLE_ID, MEMBER_ID, PID, ROLE_ID,
    ROOT_PW, TestApp, TestResponse, VOICE_CHANNEL_ID,
};

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
async fn unknown_projects_404() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;

    for path in [
        "/projects/999",
        "/projects/999/modules/greeting",
        "/projects/999/modules/clickup",
        "/projects/999/modules/calendar",
        "/projects/999/modules/calendar/rules/new?kind=digest",
        "/projects/999/modules/calendar/rules/1",
        "/projects/999/modules/recurring",
        "/projects/999/modules/recurring/rules/new",
        "/projects/999/modules/recurring/rules/1",
    ] {
        let res = app.get(path, Some(&cookie)).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
    let posts = [
        (
            "/projects/999/modules/greeting/toggle",
            vec![("enabled", "1")],
        ),
        (
            "/projects/999/modules/greeting",
            vec![
                ("channel_id", ""),
                ("template", "hi {user}"),
                ("embed_color", "#7f8cff"),
            ],
        ),
        ("/projects/999/modules/greeting/test", vec![]),
        (
            "/projects/999/modules/clickup/toggle",
            vec![("enabled", "1")],
        ),
        (
            "/projects/999/modules/clickup/keys/clickup",
            vec![("token", mock::TOKEN)],
        ),
        ("/projects/999/modules/clickup/keys/clickup/delete", vec![]),
        ("/projects/999/modules/clickup/test", vec![]),
        (
            "/projects/999/modules/clickup/workspace",
            vec![("team_id", mock::TEAM_ID)],
        ),
        (
            "/projects/999/modules/clickup/mapping",
            vec![("map_3", "-")],
        ),
        (
            "/projects/999/modules/clickup/tag-roles",
            vec![("new_tag", "meeting"), ("new_role", "5")],
        ),
        ("/projects/999/modules/clickup/members/refresh", vec![]),
        (
            "/projects/999/modules/calendar/toggle",
            vec![("enabled", "1")],
        ),
        (
            "/projects/999/modules/calendar",
            vec![("time_field", "due"), ("timezone", "UTC")],
        ),
        ("/projects/999/modules/calendar/refresh", vec![]),
        (
            "/projects/999/modules/calendar/keys/clickup",
            vec![("token", mock::TOKEN)],
        ),
        (
            "/projects/999/modules/calendar/keys/clickup/source",
            vec![("source", "own")],
        ),
        (
            "/projects/999/modules/calendar/rules",
            vec![("kind", "digest")],
        ),
        (
            "/projects/999/modules/calendar/rules/1",
            vec![("name", "x")],
        ),
        ("/projects/999/modules/calendar/rules/1/delete", vec![]),
        ("/projects/999/modules/calendar/rules/1/test", vec![]),
        (
            "/projects/999/modules/recurring/toggle",
            vec![("enabled", "1")],
        ),
        (
            "/projects/999/modules/recurring",
            vec![("timezone", "UTC"), ("check_minutes", "15")],
        ),
        ("/projects/999/modules/recurring/refresh", vec![]),
        ("/projects/999/modules/recurring/run", vec![]),
        (
            "/projects/999/modules/recurring/keys/clickup",
            vec![("token", mock::TOKEN)],
        ),
        ("/projects/999/modules/recurring/rules", vec![("name", "x")]),
        (
            "/projects/999/modules/recurring/rules/1",
            vec![("name", "x")],
        ),
        ("/projects/999/modules/recurring/rules/1/delete", vec![]),
    ];
    for (path, mut form) in posts {
        form.push(("_csrf", &csrf));
        let res = app.post(path, Some(&cookie), &form).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
    let db = &app.state.db;
    assert!(!crate::modules::is_enabled(db, 1, "greeting").await.unwrap());
    let saved: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM greeting_config) + (SELECT COUNT(*) FROM api_keys)
              + (SELECT COUNT(*) FROM clickup_config) + (SELECT COUNT(*) FROM clickup_user_map)
              + (SELECT COUNT(*) FROM api_key_source)
              + (SELECT COUNT(*) FROM clickup_tag_roles)
              + (SELECT COUNT(*) FROM calendar_config) + (SELECT COUNT(*) FROM calendar_rules)
              + (SELECT COUNT(*) FROM recurring_config) + (SELECT COUNT(*) FROM recurring_rules)",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert_eq!(saved, 0);
    assert!(app.clickup.requests().is_empty());
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

#[tokio::test]
async fn project_pages_render_with_connected_bot() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;

    let res = app.get("/", Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(GUILD_NAME));

    let res = app.get(&format!("/projects/{PID}"), Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(GUILD_NAME));
    assert!(
        res.body
            .contains(&format!("/projects/{PID}/modules/greeting"))
    );

    let res = app
        .get(&format!("/projects/{PID}/modules/greeting"), Some(&cookie))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("#events"));
    assert!(
        !res.body.contains("Voice"),
        "only text channels are offered"
    );

    let res = app
        .get("/projects/999/modules/greeting", Some(&cookie))
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn greeting_toggle_and_save() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let base = format!("/projects/{PID}/modules/greeting");
    let db = &app.state.db;

    let form = [
        ("_csrf", csrf.as_str()),
        ("enabled", "1"),
        ("back", "module"),
    ];
    let res = app
        .post(&format!("{base}/toggle"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("{base}?msg=enabled"));
    assert!(
        crate::modules::is_enabled(db, PID, "greeting")
            .await
            .unwrap()
    );

    let form = [("_csrf", csrf.as_str()), ("enabled", "0")];
    let res = app
        .post(&format!("{base}/toggle"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("/projects/{PID}?msg=disabled"));

    let channel = CHANNEL_ID.to_string();
    let voice = VOICE_CHANNEL_ID.to_string();
    let long = "x".repeat(1501);
    let cases = [
        (channel.as_str(), "Hi {user}", "#123456", "saved"),
        (voice.as_str(), "Hi {user}", "#123456", "invalid_channel"),
        ("", "   ", "#123456", "template_empty"),
        ("", long.as_str(), "#123456", "template_long"),
        ("", "Hi", "red", "invalid_color"),
    ];
    for (channel_id, template, color, code) in cases {
        let form = [
            ("_csrf", csrf.as_str()),
            ("channel_id", channel_id),
            ("template", template),
            ("embed_color", color),
            ("use_embed", "on"),
        ];
        let res = app.post(&base, Some(&cookie), &form).await;
        assert_eq!(res.redirect(), format!("{base}?msg={code}"), "{code}");
    }

    let cfg = crate::modules::greeting::load_config(db, PID)
        .await
        .unwrap();
    assert_eq!(cfg.channel_id, Some(CHANNEL_ID as i64));
    assert_eq!(cfg.template, "Hi {user}");
    assert!(cfg.use_embed);
    assert_eq!(cfg.embed_color, 0x123456);
}

const CLICKUP_ROUTES: [&str; 7] = [
    "/keys/clickup",
    "/keys/clickup/delete",
    "/test",
    "/workspace",
    "/mapping",
    "/members/refresh",
    "/tag-roles",
];

fn clickup_path(suffix: &str) -> String {
    format!("/projects/{PID}/modules/clickup{suffix}")
}

/// Saves the mock's valid key through the dashboard.
async fn save_clickup_key(app: &TestApp, cookie: &str, csrf: &str) {
    let form = [("_csrf", csrf), ("token", mock::TOKEN)];
    let res = app
        .post(&clickup_path("/keys/clickup"), Some(cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=token_saved"));
}

#[tokio::test]
async fn clickup_routes_require_login_and_csrf() {
    let app = TestApp::with_guild().await;
    assert_eq!(app.get(&clickup_path(""), None).await.redirect(), "/login");
    let cookie = app.login_root().await;
    for suffix in CLICKUP_ROUTES {
        let path = clickup_path(suffix);
        assert_eq!(
            app.post(&path, None, &[]).await.redirect(),
            "/login",
            "{path}"
        );
        let res = app
            .post(&path, Some(&cookie), &[("token", mock::TOKEN)])
            .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
    // The shared key has no shared/own choice.
    let csrf = app.csrf(&cookie).await;
    let form = [("_csrf", csrf.as_str()), ("source", "own")];
    let res = app
        .post(&clickup_path("/keys/clickup/source"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(app.clickup.requests().is_empty());
}

#[tokio::test]
async fn clickup_key_is_checked_stored_encrypted_and_never_shown() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;

    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("No API key saved yet."));

    let cases = [
        ("garbage", "token_invalid"),
        ("pk_1_WRONG_KEY_VALUE", "token_rejected"),
    ];
    for (token, code) in cases {
        let form = [("_csrf", csrf.as_str()), ("token", token)];
        let res = app
            .post(&clickup_path("/keys/clickup"), Some(&cookie), &form)
            .await;
        assert_eq!(
            res.redirect(),
            clickup_path(&format!("?msg={code}")),
            "{token}"
        );
    }
    app.clickup.fail_with(Some(503));
    let form = [("_csrf", csrf.as_str()), ("token", mock::TOKEN)];
    let res = app
        .post(&clickup_path("/keys/clickup"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=clickup_unreachable"));
    app.clickup.fail_with(None);
    assert!(
        keys::info(db, PID, Service::ClickUp, "clickup")
            .await
            .unwrap()
            .is_none()
    );

    save_clickup_key(&app, &cookie, &csrf).await;
    let sealed: Vec<u8> = sqlx::query_scalar("SELECT sealed FROM api_keys")
        .fetch_one(db)
        .await
        .unwrap();
    assert!(
        !sealed
            .windows(mock::TOKEN.len())
            .any(|w| w == mock::TOKEN.as_bytes())
    );

    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(res.body.contains("Test Owner"));
    assert!(res.body.contains(&mock::TOKEN[mock::TOKEN.len() - 4..]));
    assert!(!res.body.contains(mock::TOKEN));
    assert!(res.body.contains("Scorpio Team"), "workspaces are listed");

    let form = [("_csrf", csrf.as_str())];
    let res = app.post(&clickup_path("/test"), Some(&cookie), &form).await;
    assert_eq!(res.redirect(), clickup_path("?msg=token_ok"));

    let res = app
        .post(&clickup_path("/keys/clickup/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=token_removed"));
    assert!(
        keys::info(db, PID, Service::ClickUp, "clickup")
            .await
            .unwrap()
            .is_none()
    );
    let res = app.post(&clickup_path("/test"), Some(&cookie), &form).await;
    assert_eq!(res.redirect(), clickup_path("?msg=token_missing"));

    let requests = app.clickup.requests();
    assert!(requests.iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn clickup_workspace_and_mappings() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;

    let form = [("_csrf", csrf.as_str()), ("team_id", mock::TEAM_ID)];
    let res = app
        .post(&clickup_path("/workspace"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=token_missing"));

    save_clickup_key(&app, &cookie, &csrf).await;
    let form = [("_csrf", csrf.as_str()), ("team_id", "999")];
    let res = app
        .post(&clickup_path("/workspace"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=workspace_invalid"));
    let form = [("_csrf", csrf.as_str()), ("team_id", mock::TEAM_ID)];
    let res = app
        .post(&clickup_path("/workspace"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=workspace_saved"));

    // The fixture member "Anna Nowak" is matched automatically.
    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(res.body.contains("Igor Zawórski"));
    assert!(
        res.body.contains("Anna Nowak (auto, same name)"),
        "{}",
        res.body
    );

    let form = [
        ("_csrf", csrf.as_str()),
        ("name_2", "Igor Zawórski"),
        ("map_2", "-"),
        ("name_3", "Anna Nowak"),
        ("map_3", "@anna.nowak"),
        ("map_1", ""),
    ];
    let res = app
        .post(&clickup_path("/mapping"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=mapping_saved"));
    let map = explicit_mappings(db, PID).await.unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(map[&2], Explicit::Never);
    assert_eq!(map[&3], Explicit::Member(MEMBER_ID));

    // One unknown member: nothing changes.
    let form = [
        ("_csrf", csrf.as_str()),
        ("map_3", ""),
        ("map_1", "nobody-here"),
    ];
    let res = app
        .post(&clickup_path("/mapping"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=mapping_unknown"));
    assert_eq!(explicit_mappings(db, PID).await.unwrap(), map);

    let form = [("_csrf", csrf.as_str()), ("map_abc", "-")];
    let res = app
        .post(&clickup_path("/mapping"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(res.body.contains("Never mapped"));
    assert!(res.body.contains(r#"value="anna.nowak""#));

    // Reloading members falls back to the gateway cache when HTTP fails.
    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(&clickup_path("/members/refresh"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=members_refreshed"));
    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Reload server members (1)"));

    let summary = app.get(&format!("/projects/{PID}"), Some(&cookie)).await;
    assert!(summary.body.contains("Scorpio Team"));
}

#[tokio::test]
async fn clickup_tag_roles() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let role = ROLE_ID.to_string();
    let leads = LEADS_ROLE_ID.to_string();
    let everyone = PID.to_string();

    // Without a key or workspace only the "other tag" row is there.
    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(res.body.contains("Tags to roles"));
    assert!(res.body.contains("@Team"));
    assert!(res.body.contains("@Leads (not mentionable)"));
    assert!(!res.body.contains("@@everyone"), "@everyone is not offered");
    assert!(!res.body.contains(r#"name="tag_0""#));

    let form = [
        ("_csrf", csrf.as_str()),
        ("new_tag", " Meeting "),
        ("new_role", role.as_str()),
    ];
    let res = app
        .post(&clickup_path("/tag-roles"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=tag_roles_saved"));
    let rows = tag_role_rows(db, PID).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tag, "meeting");
    assert_eq!(rows[0].role_id as u64, ROLE_ID);

    // Tags of the selected workspace are listed next to the saved ones.
    save_clickup_key(&app, &cookie, &csrf).await;
    let form = [("_csrf", csrf.as_str()), ("team_id", mock::TEAM_ID)];
    let res = app
        .post(&clickup_path("/workspace"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=workspace_saved"));
    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(
        res.body.contains(r#"name="tag_0" value="meeting""#),
        "{}",
        res.body
    );
    assert!(res.body.contains(r#"name="tag_1" value="team sync""#));
    assert!(
        res.body
            .contains(&format!(r#"<option value="{ROLE_ID}" selected>"#))
    );

    // Remap, add and remove in one save; an unknown role or bad tag changes nothing.
    for (tag, role_raw, code) in [
        ("team sync", "12345", "role_unknown"),
        ("team sync", everyone.as_str(), "role_unknown"),
        ("team sync", "abc", "role_unknown"),
        (" ", leads.as_str(), "tag_invalid"),
    ] {
        let form = [
            ("_csrf", csrf.as_str()),
            ("tag_0", "meeting"),
            ("role_0", ""),
            ("tag_1", tag),
            ("role_1", role_raw),
        ];
        let res = app
            .post(&clickup_path("/tag-roles"), Some(&cookie), &form)
            .await;
        assert_eq!(
            res.redirect(),
            clickup_path(&format!("?msg={code}")),
            "{role_raw}"
        );
        assert_eq!(tag_role_rows(db, PID).await.unwrap().len(), 1);
    }
    let form = [
        ("_csrf", csrf.as_str()),
        ("tag_0", "meeting"),
        ("role_0", ""),
        ("tag_1", "team sync"),
        ("role_1", leads.as_str()),
        ("new_tag", "standup"),
        ("new_role", role.as_str()),
    ];
    let res = app
        .post(&clickup_path("/tag-roles"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=tag_roles_saved"));
    let rows = tag_role_rows(db, PID).await.unwrap();
    let pairs: Vec<(&str, u64)> = rows
        .iter()
        .map(|r| (r.tag.as_str(), r.role_id as u64))
        .collect();
    assert_eq!(pairs, [("standup", ROLE_ID), ("team sync", LEADS_ROLE_ID)]);

    // A mapping to a role deleted in Discord is flagged.
    set_tag_role(db, PID, "old", Some(999), "root")
        .await
        .unwrap();
    let res = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(res.body.contains("mapped role was deleted"));
}

#[tokio::test]
async fn every_account_can_configure_clickup() {
    let app = TestApp::with_guild().await;
    let root = app.login_root().await;
    app.create_user(&root, "alice", USER_PW).await;
    let alice = app.login("alice", USER_PW).await;
    let csrf = app.csrf(&alice).await;
    save_clickup_key(&app, &alice, &csrf).await;
    let info = keys::info(&app.state.db, PID, Service::ClickUp, "clickup")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.updated_by.as_deref(), Some("alice"));
}

fn calendar_path(suffix: &str) -> String {
    format!("/projects/{PID}/modules/calendar{suffix}")
}

/// Enables the ClickUp module and saves the mock's key as the shared key.
async fn shared_clickup_key(app: &TestApp, cookie: &str, csrf: &str) {
    save_clickup_key(app, cookie, csrf).await;
    let form = [("_csrf", csrf), ("enabled", "1")];
    app.post(&clickup_path("/toggle"), Some(cookie), &form)
        .await;
}

/// Saves the calendar source with the mock's list.
async fn select_list(app: &TestApp, cookie: &str, csrf: &str) {
    let form = [
        ("_csrf", csrf),
        ("list_select", mock::LIST_ID),
        ("list_manual", ""),
        ("time_field", "due"),
        ("timezone", "Europe/Warsaw"),
        ("tag_filter", ""),
    ];
    let res = app.post(&calendar_path(""), Some(cookie), &form).await;
    assert_eq!(res.redirect(), calendar_path("?msg=saved"));
}

fn digest_form(csrf: &str) -> Vec<(&'static str, String)> {
    vec![
        ("_csrf", csrf.to_owned()),
        ("kind", "digest".into()),
        ("name", "Morning agenda".into()),
        ("enabled", "on".into()),
        ("channel_id", CHANNEL_ID.to_string()),
        ("day", "0".into()),
        ("day", "4".into()),
        ("time", "08:30".into()),
        ("lookahead_hours", "24".into()),
        ("offset_minutes", "240".into()),
        ("layout", "combined".into()),
        ("max_events", "10".into()),
        ("header_template", "**Today** ({count})".into()),
        ("empty_template", "Nothing today.".into()),
        ("item_template", "{when} {link}\n{assignees}".into()),
        ("embed_color", "#7f8cff".into()),
        ("description_max", "200".into()),
        ("reactions", format!("✅ <:party:{EMOJI_ID}>")),
        ("timezone", "Europe/Warsaw".into()),
    ]
}

fn with_field(
    form: &[(&'static str, String)],
    key: &'static str,
    value: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut out: Vec<_> = form.iter().filter(|(k, _)| *k != key).cloned().collect();
    if let Some(v) = value {
        out.push((key, v.to_owned()));
    }
    out
}

async fn post_owned(
    app: &TestApp,
    path: &str,
    cookie: &str,
    form: &[(&'static str, String)],
) -> TestResponse {
    let pairs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
    app.post(path, Some(cookie), &pairs).await
}

#[tokio::test]
async fn calendar_routes_require_login_and_csrf() {
    let app = TestApp::with_guild().await;
    for path in [calendar_path(""), calendar_path("/rules/new?kind=digest")] {
        assert_eq!(app.get(&path, None).await.redirect(), "/login", "{path}");
    }
    let cookie = app.login_root().await;
    for suffix in [
        "",
        "/refresh",
        "/rules",
        "/rules/1",
        "/rules/1/delete",
        "/rules/1/test",
        "/keys/clickup",
        "/keys/clickup/delete",
        "/keys/clickup/source",
    ] {
        let path = calendar_path(suffix);
        assert_eq!(
            app.post(&path, None, &[]).await.redirect(),
            "/login",
            "{path}"
        );
        let res = app.post(&path, Some(&cookie), &[("name", "x")]).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn calendar_source_uses_shared_or_own_key() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;

    // Shared key by default, but the ClickUp module is off.
    let form = [
        ("_csrf", csrf.as_str()),
        ("list_select", mock::LIST_ID),
        ("time_field", "due"),
        ("timezone", "UTC"),
    ];
    let res = app.post(&calendar_path(""), Some(&cookie), &form).await;
    assert_eq!(res.redirect(), calendar_path("?msg=clickup_disabled"));

    shared_clickup_key(&app, &cookie, &csrf).await;
    select_list(&app, &cookie, &csrf).await;
    let cfg = calendar::load_config(db, PID).await.unwrap();
    assert_eq!(cfg.list_id.as_deref(), Some(mock::LIST_ID));
    assert_eq!(cfg.list_name.as_deref(), Some("Events / Team / Calendar"));
    assert_eq!(cfg.timezone, "Europe/Warsaw");

    let cases = [
        ("list_manual", "999", "list_not_found"),
        ("list_manual", "../x", "list_not_found"),
        ("timezone", "Mars/Base", "timezone_invalid"),
        ("tag_filter", "a,b,c,d,e,f", "tags_invalid"),
    ];
    for (field, value, code) in cases {
        let mut form = vec![
            ("_csrf", csrf.as_str()),
            ("time_field", "due"),
            ("timezone", "UTC"),
        ];
        form.retain(|(k, _)| *k != field);
        form.push((field, value));
        let res = app.post(&calendar_path(""), Some(&cookie), &form).await;
        assert_eq!(
            res.redirect(),
            calendar_path(&format!("?msg={code}")),
            "{field}"
        );
    }
    let form = [
        ("_csrf", csrf.as_str()),
        ("list_select", mock::LIST_ID),
        ("time_field", "sideways"),
        ("timezone", "UTC"),
    ];
    let res = app.post(&calendar_path(""), Some(&cookie), &form).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // A list URL works too.
    let url = format!("https://app.clickup.com/100/v/li/{}", mock::OTHER_LIST_ID);
    let form = [
        ("_csrf", csrf.as_str()),
        ("list_manual", url.as_str()),
        ("time_field", "start"),
        ("timezone", "UTC"),
        ("tag_filter", " meeting ,event"),
        ("include_subtasks", "on"),
    ];
    let res = app.post(&calendar_path(""), Some(&cookie), &form).await;
    assert_eq!(res.redirect(), calendar_path("?msg=saved"));
    let cfg = calendar::load_config(db, PID).await.unwrap();
    assert_eq!(cfg.list_id.as_deref(), Some(mock::OTHER_LIST_ID));
    assert_eq!(cfg.list_name.as_deref(), Some("Events / Misc"));
    assert_eq!(cfg.tag_filter, "meeting, event");
    assert!(cfg.start_mode() && cfg.include_subtasks && !cfg.include_closed);

    // Switching to its own key: none saved yet, so ClickUp is not used.
    let form = [("_csrf", csrf.as_str()), ("source", "own")];
    let res = app
        .post(&calendar_path("/keys/clickup/source"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), calendar_path("?msg=source_saved"));
    let form = [
        ("_csrf", csrf.as_str()),
        ("list_select", mock::LIST_ID),
        ("time_field", "due"),
        ("timezone", "UTC"),
    ];
    let res = app.post(&calendar_path(""), Some(&cookie), &form).await;
    assert_eq!(res.redirect(), calendar_path("?msg=token_missing"));

    let form = [("_csrf", csrf.as_str()), ("source", "sideways")];
    let res = app
        .post(&calendar_path("/keys/clickup/source"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let form = [("_csrf", csrf.as_str()), ("token", mock::TOKEN)];
    let res = app
        .post(&calendar_path("/keys/clickup"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), calendar_path("?msg=token_saved"));
    assert!(
        !keys::use_shared(db, PID, Service::ClickUp, "calendar")
            .await
            .unwrap()
    );
    select_list(&app, &cookie, &csrf).await;

    let page = app.get(&calendar_path(""), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains(mock::TOKEN));
    assert!(
        page.body.contains(r#"<optgroup label="Events / Team">"#),
        "list picker"
    );
    assert!(app.clickup.requests().iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn calendar_page_lists_upcoming_events_and_rules() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    shared_clickup_key(&app, &cookie, &csrf).await;
    select_list(&app, &cookie, &csrf).await;

    let now = crate::db::now();
    let mut task = mock::task("t1", "Sprint <review>", Some((now + 3600) * 1000));
    task["assignees"] = serde_json::json!([
        {"id": 3, "username": "Anna Nowak", "email": "a@x.io"},
        {"id": 9, "username": "Guest", "email": "g@x.io"}
    ]);
    // A date without a time: ClickUp stores 04:00 local time.
    let tz = jiff::tz::TimeZone::get("Europe/Warsaw").unwrap();
    let tomorrow = jiff::Timestamp::now()
        .to_zoned(tz.clone())
        .date()
        .tomorrow()
        .unwrap();
    let four = tomorrow.at(4, 0, 0, 0).to_zoned(tz).unwrap().timestamp();
    app.clickup.set_tasks(
        mock::LIST_ID,
        vec![
            task,
            mock::task("done", "Reminded", Some((now - 3600) * 1000)),
            mock::task("late", "Missed it", Some((now - 7200) * 1000)),
            mock::task("allday", "Offsite", Some(four.as_millisecond())),
        ],
    );
    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(&calendar_path("/refresh"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), calendar_path("?msg=refreshed"));

    let mut rule = Rule::new(Kind::Reminder);
    rule.channel_id = Some(VOICE_CHANNEL_ID);
    let rid = calendar::insert_rule(&app.state.db, PID, &rule, "root")
        .await
        .unwrap();
    calendar::mark_sent(&app.state.db, rid, "done", now - 3600)
        .await
        .unwrap();

    let page = app.get(&calendar_path(""), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("Sprint &#60;review&#62;"),
        "escaped title"
    );
    assert!(page.body.contains("@Anna Nowak"));
    assert!(page.body.contains("Guest (no member)"));
    assert!(page.body.contains("15 minutes before each event"));
    assert!(
        page.body.contains("The channel of rule"),
        "voice channel is not a text channel"
    );
    // Recently started events are listed, with what each reminder rule did or will do.
    assert!(page.body.contains("Reminded"));
    assert!(page.body.contains("Event reminder: sent"));
    assert!(
        page.body
            .contains("Event reminder: not sent, the bot did not see it")
    );
    assert!(page.body.contains("Event reminder: at "));
    assert_eq!(page.body.matches(">started<").count(), 2);
    assert_eq!(page.body.matches("no time set in ClickUp?").count(), 1);
}

#[tokio::test]
async fn calendar_rule_create_validate_edit_delete() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;

    let res = app
        .get(&calendar_path("/rules/new?kind=digest"), Some(&cookie))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Daily agenda"));
    assert!(
        res.body.contains(&format!("&#60;:party:{EMOJI_ID}&#62;")),
        "emoji chips"
    );
    assert!(
        res.body
            .contains(r#"id="rule-tz" value="UTC" list="time-zones" required autocomplete="off" data-detect-tz"#),
        "new rules start from the default zone and take the browser's"
    );
    let res = app
        .get(&calendar_path("/rules/new?kind=weekly"), Some(&cookie))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let base = digest_form(&csrf);
    let long = "x".repeat(1501);
    let voice = VOICE_CHANNEL_ID.to_string();
    let cases: Vec<(&str, Option<&str>, &str)> = vec![
        ("name", Some(" "), "rule_name"),
        ("channel_id", Some(voice.as_str()), "invalid_channel"),
        ("channel_id", Some("abc"), "invalid_channel"),
        ("day", None, "rule_no_days"),
        ("time", Some("25:00"), "rule_time"),
        ("lookahead_hours", Some("0"), "rule_lookahead"),
        ("offset_minutes", Some("10081"), "rule_offset"),
        ("offset_minutes", Some("-5"), "rule_offset"),
        ("offset_minutes", Some("4h"), "rule_offset"),
        ("max_events", Some("51"), "rule_max_events"),
        ("item_template", Some("  "), "template_empty"),
        ("header_template", Some(long.as_str()), "template_long"),
        ("empty_template", Some(" "), "template_empty"),
        ("timezone", Some("Mars/Olympus"), "timezone_invalid"),
        ("timezone", None, "timezone_invalid"),
        ("embed_color", Some("red"), "invalid_color"),
        ("description_max", Some("5000"), "rule_description_max"),
        ("reactions", Some("nope"), "reactions_invalid"),
        ("reactions", Some("<:other:5>"), "reactions_invalid"),
    ];
    for (field, value, code) in cases {
        let form = with_field(&base, field, value);
        let res = post_owned(&app, &calendar_path("/rules"), &cookie, &form).await;
        assert_eq!(
            res.redirect(),
            calendar_path(&format!("/rules/new?kind=digest&msg={code}")),
            "{field}"
        );
    }
    assert_eq!(calendar::count_rules(db, PID).await.unwrap(), 0);
    let flash = app
        .get(
            &calendar_path("/rules/new?kind=digest&msg=rule_time"),
            Some(&cookie),
        )
        .await;
    assert!(flash.body.contains("The time must look like 09:00."));

    let res = post_owned(&app, &calendar_path("/rules"), &cookie, &base).await;
    let location = res.redirect().to_owned();
    assert!(location.ends_with("?msg=rule_created"), "{location}");
    let rules = calendar::list_rules(db, PID).await.unwrap();
    let rule = &rules[0];
    assert_eq!(rule.name, "Morning agenda");
    assert_eq!(rule.days, 0b1_0001);
    assert_eq!(rule.time_of_day, 8 * 60 + 30);
    assert_eq!(rule.channel_id, Some(CHANNEL_ID));
    assert_eq!(rule.reactions.len(), 2);
    assert_eq!(rule.timezone, "Europe/Warsaw");
    assert_eq!(rule.offset_minutes, 240);
    assert!(rule.enabled && !rule.mention_assignees && !rule.mention_roles);
    assert_eq!(
        location,
        calendar_path(&format!("/rules/{}?msg=rule_created", rule.id))
    );

    let page = app
        .get(
            &calendar_path(&format!("/rules/{}", rule.id)),
            Some(&cookie),
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("Weekly sync"),
        "preview with a sample event"
    );

    // Edit: the kind cannot change, fields of the other kind are ignored.
    let edit = with_field(&base, "name", Some("Renamed"));
    let edit = with_field(&edit, "kind", Some("reminder"));
    let edit = with_field(&edit, "mention_assignees", Some("on"));
    let edit = with_field(&edit, "mention_roles", Some("on"));
    let edit = with_field(&edit, "header_template", Some("  "));
    let edit = with_field(&edit, "timezone", Some(" America/New_York "));
    let edit = with_field(&edit, "offset_minutes", None);
    let path = calendar_path(&format!("/rules/{}", rule.id));
    let res = post_owned(&app, &path, &cookie, &edit).await;
    assert_eq!(res.redirect(), format!("{path}?msg=saved"));
    let saved = calendar::load_rule(db, PID, rule.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.name, "Renamed");
    assert_eq!(saved.kind, Kind::Digest);
    assert!(saved.mention_assignees && saved.mention_roles);
    assert_eq!(saved.header_template, "", "the header is optional");
    assert_eq!(saved.timezone, "America/New_York");
    assert_eq!(saved.offset_minutes, 0, "no offset when left out");

    // Role pings without a {roles} placeholder or a mapped tag are pointed out.
    let page = app.get(&path, Some(&cookie)).await;
    assert!(
        page.body.contains("have no {roles} placeholder"),
        "{}",
        page.body
    );
    assert!(
        !page.body.contains("data-detect-tz"),
        "existing rules keep their zone"
    );
    let edit = with_field(&edit, "item_template", Some("{title} {roles}"));
    post_owned(&app, &path, &cookie, &edit).await;
    let page = app.get(&path, Some(&cookie)).await;
    assert!(page.body.contains("no ClickUp tag is mapped"));
    set_tag_role(db, PID, "meeting", Some(ROLE_ID), "root")
        .await
        .unwrap();
    let page = app.get(&path, Some(&cookie)).await;
    assert!(!page.body.contains("no ClickUp tag is mapped"));
    let list = app.get(&calendar_path(""), Some(&cookie)).await;
    assert!(list.body.contains("Mon, Fri at 08:30 America/New_York"));
    let bad = with_field(&base, "time", Some("noon"));
    let res = post_owned(&app, &path, &cookie, &bad).await;
    assert_eq!(res.redirect(), format!("{path}?msg=rule_time"));

    // A reminder only needs its own fields.
    let reminder = vec![
        ("_csrf", csrf.clone()),
        ("kind", "reminder".to_owned()),
        ("name", "Heads up".to_owned()),
        ("channel_id", CHANNEL_ID.to_string()),
        ("lead_minutes", "30".to_owned()),
        ("item_template", "{link} {relative}".to_owned()),
        ("embed_color", "#000000".to_owned()),
        ("description_max", "0".to_owned()),
    ];
    let res = post_owned(&app, &calendar_path("/rules"), &cookie, &reminder).await;
    assert!(res.redirect().ends_with("?msg=rule_created"));
    let bad = with_field(&reminder, "lead_minutes", Some("0"));
    let res = post_owned(&app, &calendar_path("/rules"), &cookie, &bad).await;
    assert_eq!(
        res.redirect(),
        calendar_path("/rules/new?kind=reminder&msg=rule_lead")
    );

    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(&format!("{path}/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), calendar_path("?msg=rule_deleted"));
    assert_eq!(calendar::count_rules(db, PID).await.unwrap(), 1);
    let res = app
        .post(&format!("{path}/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn calendar_rules_of_other_guilds_are_not_reachable() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let foreign = calendar::insert_rule(db, 42, &Rule::new(Kind::Digest), "x")
        .await
        .unwrap();

    let path = calendar_path(&format!("/rules/{foreign}"));
    assert_eq!(
        app.get(&path, Some(&cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    let form = digest_form(&csrf);
    for target in [
        path.clone(),
        format!("{path}/delete"),
        format!("{path}/test"),
    ] {
        let res = post_owned(&app, &target, &cookie, &form).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{target}");
    }
    assert_eq!(calendar::count_rules(db, 42).await.unwrap(), 1);
    let res = app.get(&calendar_path("/rules/abc"), Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn calendar_rule_limit() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    for _ in 0..calendar::MAX_RULES {
        calendar::insert_rule(&app.state.db, PID, &Rule::new(Kind::Digest), "root")
            .await
            .unwrap();
    }
    let res = post_owned(&app, &calendar_path("/rules"), &cookie, &digest_form(&csrf)).await;
    assert_eq!(res.redirect(), calendar_path("?msg=too_many_rules"));
    let page = app.get(&calendar_path(""), Some(&cookie)).await;
    assert!(!page.body.contains("New digest"));
}

#[tokio::test]
async fn calendar_test_message_reports_failures() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let form = [("_csrf", csrf.as_str())];

    let mut rule = Rule::new(Kind::Reminder);
    let no_channel = calendar::insert_rule(db, PID, &rule, "root").await.unwrap();
    rule.channel_id = Some(CHANNEL_ID);
    let rid = calendar::insert_rule(db, PID, &rule, "root").await.unwrap();
    let test_path = |id: i64| calendar_path(&format!("/rules/{id}/test"));

    let res = app.post(&test_path(no_channel), Some(&cookie), &form).await;
    assert!(res.redirect().ends_with("?msg=no_channel"));
    let res = app.post(&test_path(rid), Some(&cookie), &form).await;
    assert!(
        res.redirect().ends_with("?msg=calendar_no_list"),
        "{}",
        res.location()
    );

    // With a list, the post reaches Discord, which refuses it.
    shared_clickup_key(&app, &cookie, &csrf).await;
    select_list(&app, &cookie, &csrf).await;
    app.discord.fail_with(Some(403));
    let res = app.post(&test_path(rid), Some(&cookie), &form).await;
    assert!(
        res.redirect().ends_with("?msg=test_failed"),
        "{}",
        res.location()
    );
    let log = calendar::recent_log(db, PID, 1).await.unwrap();
    assert!(log[0].is_test && !log[0].success);
    assert_eq!(
        crate::modules::counter(db, Some(PID), "calendar", "failed")
            .await
            .unwrap(),
        0,
        "tests are not counted"
    );

    // Accepted: posted to the rule's channel by the project's bot, pinging nobody.
    app.discord.fail_with(None);
    let res = app.post(&test_path(rid), Some(&cookie), &form).await;
    assert!(
        res.redirect().ends_with("?msg=test_sent"),
        "{}",
        res.location()
    );
    let sent = app.discord.messages();
    let last = sent.last().unwrap();
    assert_eq!(last.path, format!("/channels/{CHANNEL_ID}/messages"));
    assert_eq!(
        last.auth.as_deref(),
        Some(format!("Bot {}", crate::test_util::DISCORD_TOKEN).as_str())
    );
    let mentions = &last.json()["allowed_mentions"];
    assert_eq!(mentions["parse"], serde_json::json!([]));
    assert!(mentions["users"].as_array().is_none_or(|u| u.is_empty()));
    assert!(mentions["roles"].as_array().is_none_or(|r| r.is_empty()));

    // Without a connected bot nothing is sent.
    app.state.discord.forget(&crate::discord::client_key(
        &crate::secrets::SecretString::new(crate::test_util::DISCORD_TOKEN),
    ));
    let res = app.post(&test_path(rid), Some(&cookie), &form).await;
    assert!(res.redirect().ends_with("?msg=bot_offline"));
    assert_eq!(app.discord.messages().len(), sent.len());
}

fn recurring_path(suffix: &str) -> String {
    format!("/projects/{PID}/modules/recurring{suffix}")
}

/// Saves the mock's workspace in the recurring module, live or in dry run.
async fn recurring_settings(app: &TestApp, cookie: &str, csrf: &str, dry_run: bool) {
    let mut form = vec![
        ("_csrf", csrf),
        ("team_id", mock::TEAM_ID),
        ("timezone", "Europe/Warsaw"),
        ("check_minutes", "30"),
    ];
    if dry_run {
        form.push(("dry_run", "on"));
    }
    let res = app.post(&recurring_path(""), Some(cookie), &form).await;
    assert_eq!(res.redirect(), recurring_path("?msg=saved"));
}

fn recurring_rule_form(csrf: &str) -> Vec<(&'static str, String)> {
    vec![
        ("_csrf", csrf.to_owned()),
        ("name", "Weekly sync".into()),
        ("enabled", "on".into()),
        ("tag", "Weekly".into()),
        ("every", "1".into()),
        ("unit", "week".into()),
        ("trigger", "close".into()),
        ("mode", "update".into()),
        ("reopen_status", "".into()),
        ("shift_start", "on".into()),
    ]
}

#[tokio::test]
async fn recurring_routes_require_login_and_csrf() {
    let app = TestApp::with_guild().await;
    for path in [recurring_path(""), recurring_path("/rules/new")] {
        assert_eq!(app.get(&path, None).await.redirect(), "/login", "{path}");
    }
    let cookie = app.login_root().await;
    for suffix in [
        "",
        "/refresh",
        "/run",
        "/rules",
        "/rules/1",
        "/rules/1/delete",
        "/keys/clickup",
        "/keys/clickup/delete",
        "/keys/clickup/source",
    ] {
        let path = recurring_path(suffix);
        assert_eq!(
            app.post(&path, None, &[]).await.redirect(),
            "/login",
            "{path}"
        );
        let res = app.post(&path, Some(&cookie), &[("name", "x")]).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert!(app.clickup.requests().is_empty());
}

#[tokio::test]
async fn recurring_settings_are_validated() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;

    // Shared key by default, but the ClickUp module is off.
    let form = [
        ("_csrf", csrf.as_str()),
        ("team_id", mock::TEAM_ID),
        ("timezone", "UTC"),
        ("check_minutes", "15"),
    ];
    let res = app.post(&recurring_path(""), Some(&cookie), &form).await;
    assert_eq!(res.redirect(), recurring_path("?msg=clickup_disabled"));

    shared_clickup_key(&app, &cookie, &csrf).await;
    let page = app.get(&recurring_path(""), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains(&format!(
            r#"<option value="{}" selected>Scorpio Team"#,
            mock::TEAM_ID
        )),
        "the only workspace is offered first"
    );
    assert!(page.body.contains("No ClickUp workspace is saved yet"));
    assert!(page.body.contains("Dry run is on"));

    recurring_settings(&app, &cookie, &csrf, false).await;
    let cfg = recurring::load_config(db, PID).await.unwrap();
    assert_eq!(cfg.team_id.as_deref(), Some(mock::TEAM_ID));
    assert_eq!(cfg.team_name.as_deref(), Some("Scorpio Team"));
    assert_eq!(cfg.timezone, "Europe/Warsaw");
    assert_eq!(cfg.check_minutes, 30);
    assert!(!cfg.dry_run, "unchecked box turns dry run off");

    let cases = [
        ("team_id", "999", "workspace_invalid"),
        ("timezone", "Mars/Base", "timezone_invalid"),
        ("check_minutes", "4", "recurring_interval"),
        ("check_minutes", "1441", "recurring_interval"),
        ("check_minutes", "soon", "recurring_interval"),
    ];
    for (field, value, code) in cases {
        let mut form = vec![
            ("_csrf", csrf.as_str()),
            ("team_id", mock::TEAM_ID),
            ("timezone", "UTC"),
            ("check_minutes", "15"),
        ];
        form.retain(|(k, _)| *k != field);
        form.push((field, value));
        let res = app.post(&recurring_path(""), Some(&cookie), &form).await;
        assert_eq!(
            res.redirect(),
            recurring_path(&format!("?msg={code}")),
            "{field}={value}"
        );
    }
    assert_eq!(recurring::load_config(db, PID).await.unwrap(), cfg);

    // No workspace is allowed and stops the module from reading anything.
    let form = [
        ("_csrf", csrf.as_str()),
        ("team_id", ""),
        ("timezone", "UTC"),
        ("check_minutes", "15"),
        ("dry_run", "on"),
    ];
    let res = app.post(&recurring_path(""), Some(&cookie), &form).await;
    assert_eq!(res.redirect(), recurring_path("?msg=saved"));
    let cfg = recurring::load_config(db, PID).await.unwrap();
    assert_eq!((cfg.team_id, cfg.dry_run), (None, true));
    assert!(app.clickup.writes().is_empty());
}

#[tokio::test]
async fn recurring_rule_create_validate_edit_delete() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    shared_clickup_key(&app, &cookie, &csrf).await;
    recurring_settings(&app, &cookie, &csrf, true).await;

    let page = app.get(&recurring_path("/rules/new"), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains(r#"<option value="team sync">"#),
        "workspace tags offered"
    );
    assert!(page.body.contains("moves to"));

    let base = recurring_rule_form(&csrf);
    let cases = [
        ("name", Some(""), "rule_name"),
        ("tag", Some(""), "rule_tag"),
        ("every", Some("0"), "rule_every"),
        ("unit", Some("fortnight"), "rule_invalid"),
        ("trigger", None, "rule_invalid"),
        ("mode", Some("delete"), "rule_invalid"),
    ];
    for (key, value, code) in cases {
        let form = with_field(&base, key, value);
        let res = post_owned(&app, &recurring_path("/rules"), &cookie, &form).await;
        assert_eq!(
            res.redirect(),
            recurring_path(&format!("/rules/new?msg={code}")),
            "{key}"
        );
    }
    assert_eq!(recurring::count_rules(db, PID).await.unwrap(), 0);

    let res = post_owned(&app, &recurring_path("/rules"), &cookie, &base).await;
    let rules = recurring::list_rules(db, PID).await.unwrap();
    assert_eq!(rules.len(), 1);
    let rule = &rules[0];
    assert_eq!(
        res.redirect(),
        recurring_path(&format!("/rules/{}?msg=rule_created", rule.id))
    );
    assert_eq!((rule.tag.as_str(), rule.every), ("weekly", 1));
    assert!(rule.enabled && rule.shift_start);

    // One rule per tag, ignoring case.
    let dup = with_field(&base, "tag", Some("WEEKLY"));
    let res = post_owned(&app, &recurring_path("/rules"), &cookie, &dup).await;
    assert_eq!(
        res.redirect(),
        recurring_path("/rules/new?msg=rule_tag_taken")
    );
    let other = with_field(&base, "tag", Some("monthly"));
    post_owned(&app, &recurring_path("/rules"), &cookie, &other).await;
    let monthly = recurring::list_rules(db, PID).await.unwrap()[1].id;
    let path = recurring_path(&format!("/rules/{monthly}"));
    let res = post_owned(&app, &path, &cookie, &base).await;
    assert_eq!(res.redirect(), format!("{path}?msg=rule_tag_taken"));

    let path = recurring_path(&format!("/rules/{}", rule.id));
    let page = app.get(&path, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(r#"value="weekly""#));
    let mut edit = with_field(&base, "mode", Some("create"));
    edit = with_field(&edit, "trigger", Some("schedule"));
    edit = with_field(&edit, "every", Some("2"));
    edit = with_field(&edit, "enabled", None);
    let res = post_owned(&app, &path, &cookie, &edit).await;
    assert_eq!(res.redirect(), format!("{path}?msg=saved"));
    let saved = recurring::load_rule(db, PID, rule.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.mode, recurring::Mode::Create);
    assert_eq!(saved.trigger, recurring::Trigger::Schedule);
    assert_eq!(saved.every, 2);
    assert!(!saved.enabled);
    let page = app.get(&path, Some(&cookie)).await;
    assert!(page.body.contains("gets a copy due"));
    assert!(page.body.contains("not copied"));

    let list = app.get(&recurring_path(""), Some(&cookie)).await;
    assert!(list.body.contains("Every 2 weeks"));
    assert!(list.body.contains("<code>monthly</code>"));

    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(&format!("{path}/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), recurring_path("?msg=rule_deleted"));
    assert_eq!(recurring::count_rules(db, PID).await.unwrap(), 1);
    let res = app
        .post(&format!("{path}/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(app.clickup.writes().is_empty());
}

#[tokio::test]
async fn recurring_rules_of_other_guilds_are_not_reachable() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let mut rule = recurring::Rule::new();
    rule.name = "Foreign".into();
    rule.tag = "weekly".into();
    let foreign = recurring::insert_rule(db, 42, &rule, "x").await.unwrap();

    let path = recurring_path(&format!("/rules/{foreign}"));
    assert_eq!(
        app.get(&path, Some(&cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    let form = recurring_rule_form(&csrf);
    for target in [path.clone(), format!("{path}/delete")] {
        let res = post_owned(&app, &target, &cookie, &form).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{target}");
    }
    assert_eq!(recurring::count_rules(db, 42).await.unwrap(), 1);
    // A tag used in another guild is free here.
    let res = post_owned(&app, &recurring_path("/rules"), &cookie, &form).await;
    assert!(res.redirect().ends_with("?msg=rule_created"));
}

#[tokio::test]
async fn recurring_rule_limit() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    for i in 0..recurring::MAX_RULES {
        let mut rule = recurring::Rule::new();
        rule.name = format!("Rule {i}");
        rule.tag = format!("tag{i}");
        recurring::insert_rule(&app.state.db, PID, &rule, "root")
            .await
            .unwrap();
    }
    let form = with_field(&recurring_rule_form(&csrf), "tag", Some("fresh"));
    let res = post_owned(&app, &recurring_path("/rules"), &cookie, &form).await;
    assert_eq!(res.redirect(), recurring_path("?msg=too_many_rules"));
    let page = app.get(&recurring_path(""), Some(&cookie)).await;
    assert!(page.body.contains("maximum number of rules"));
}

#[tokio::test]
async fn recurring_preview_and_run_now() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let form = [("_csrf", csrf.as_str())];

    let res = app
        .post(&recurring_path("/run"), Some(&cookie), &form)
        .await;
    assert_eq!(
        res.redirect(),
        recurring_path("?msg=recurring_no_workspace")
    );

    shared_clickup_key(&app, &cookie, &csrf).await;
    recurring_settings(&app, &cookie, &csrf, true).await;
    let res = app
        .post(&recurring_path("/run"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), recurring_path("?msg=recurring_no_rules"));
    post_owned(
        &app,
        &recurring_path("/rules"),
        &cookie,
        &recurring_rule_form(&csrf),
    )
    .await;

    let now = crate::db::now();
    let mut done = mock::task("t1", "Weekly <report>", Some((now - 86_400) * 1000));
    done["status"] = serde_json::json!({"status": "complete", "type": "closed"});
    done["tags"] = serde_json::json!([{"name": "weekly"}]);
    let mut open = mock::task("t2", "Still open", Some((now - 86_400) * 1000));
    open["tags"] = serde_json::json!([{"name": "weekly"}]);
    app.clickup.set_tasks(mock::LIST_ID, vec![done, open]);

    let page = app.get(&recurring_path(""), Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("Weekly &#60;report&#62;"),
        "escaped task name in preview"
    );
    assert!(!page.body.contains("Still open"));
    assert!(page.body.contains("Dry run now"));

    let res = app
        .post(&recurring_path("/run"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), recurring_path("?msg=recurring_dry_ran"));
    assert!(app.clickup.writes().is_empty());
    let page = app
        .get(&recurring_path("?msg=recurring_dry_ran"), Some(&cookie))
        .await;
    assert!(page.body.contains("Dry run finished"));
    assert!(
        page.body
            .contains(r#"<span class="badge badge-muted">Dry run</span>"#)
    );

    recurring_settings(&app, &cookie, &csrf, false).await;
    let page = app.get(&recurring_path(""), Some(&cookie)).await;
    assert!(
        page.body
            .contains(r#"data-confirm="Change these tasks in ClickUp now?""#)
    );
    let res = app
        .post(&recurring_path("/run"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), recurring_path("?msg=recurring_ran"));
    let writes = app.clickup.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].path, "/task/t1");
    assert_eq!(writes[0].json()["status"], "to do");
    assert_eq!(
        crate::modules::counter(db, Some(PID), "recurring", "moved")
            .await
            .unwrap(),
        1
    );
    let page = app.get(&recurring_path(""), Some(&cookie)).await;
    assert!(
        page.body
            .contains(r#"<span class="badge badge-on">Moved</span>"#)
    );
    assert!(page.body.contains("No tagged task is due to recur"));

    let res = app
        .post(&recurring_path("/refresh"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), recurring_path("?msg=refreshed"));
}

#[tokio::test]
async fn key_cards_say_when_a_key_changes_clickup() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    shared_clickup_key(&app, &cookie, &csrf).await;
    let reads_only = "The bot only reads from ClickUp with this key";

    let page = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(page.body.contains(reads_only));
    let page = app.get(&recurring_path(""), Some(&cookie)).await;
    assert!(
        page.body
            .contains("This module changes tasks in ClickUp with this key")
    );

    let form = [("_csrf", csrf.as_str()), ("enabled", "1")];
    app.post(&recurring_path("/toggle"), Some(&cookie), &form)
        .await;
    let page = app.get(&clickup_path(""), Some(&cookie)).await;
    assert!(
        page.body
            .contains("The Recurring tasks module uses this key")
    );
    assert!(!page.body.contains(reads_only));
    let page = app.get(&calendar_path(""), Some(&cookie)).await;
    assert!(!page.body.contains("changes tasks"), "calendar only reads");
}

// ---------------------------------------------------------------- projects

#[tokio::test]
async fn project_routes_require_login_csrf_and_root_for_delete() {
    let app = TestApp::new().await;
    let root = app.login_root().await;
    app.create_user(&root, "bob", USER_PW).await;
    let bob = app.login("bob", USER_PW).await;
    let csrf = app.csrf(&bob).await;

    let rename = format!("/projects/{PID}/rename");
    let delete = format!("/projects/{PID}/delete");
    for path in ["/projects", rename.as_str(), delete.as_str()] {
        let form = [("name", "x")];
        assert_eq!(
            app.post(path, None, &form).await.redirect(),
            "/login",
            "{path}"
        );
        let form = [("name", "x"), ("_csrf", "wrong")];
        let res = app.post(path, Some(&bob), &form).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(
        app.get(&format!("/projects/{PID}"), None).await.redirect(),
        "/login"
    );

    // Every account configures projects, only root deletes them.
    let page = app.get(&format!("/projects/{PID}"), Some(&bob)).await;
    assert!(page.body.contains("Rename"));
    assert!(!page.body.contains("Delete project"));
    let res = app
        .post(&delete, Some(&bob), &[("_csrf", csrf.as_str())])
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert!(projects::get(&app.state.db, PID).await.unwrap().is_some());
    let page = app.get(&format!("/projects/{PID}"), Some(&root)).await;
    assert!(page.body.contains("Delete project"));
}

#[tokio::test]
async fn projects_create_rename_and_delete() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;

    let form = [("_csrf", csrf.as_str()), ("name", "  Marketing   team ")];
    let res = app.post("/projects", Some(&cookie), &form).await;
    let id = projects::list(db)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.name == "Marketing team")
        .unwrap()
        .id;
    assert_eq!(
        res.redirect(),
        format!("/projects/{id}?msg=project_created")
    );
    let long = "x".repeat(61);
    for bad in ["", "  ", long.as_str()] {
        let form = [("_csrf", csrf.as_str()), ("name", bad)];
        let res = app.post("/projects", Some(&cookie), &form).await;
        assert_eq!(res.redirect(), "/?msg=project_name");
    }
    let overview = app.get("/", Some(&cookie)).await;
    assert!(overview.body.contains("Marketing team"));
    assert!(overview.body.contains(crate::test_util::PROJECT_NAME));

    let rename = format!("/projects/{id}/rename");
    let form = [("_csrf", csrf.as_str()), ("name", "Sales <b>")];
    let res = app.post(&rename, Some(&cookie), &form).await;
    assert_eq!(
        res.redirect(),
        format!("/projects/{id}?msg=project_renamed")
    );
    let page = app.get(&format!("/projects/{id}"), Some(&cookie)).await;
    assert!(page.body.contains("Sales &#60;b&#62;"));
    let form = [("_csrf", csrf.as_str()), ("name", " ")];
    let res = app.post(&rename, Some(&cookie), &form).await;
    assert_eq!(res.redirect(), format!("/projects/{id}?msg=project_name"));
    let res = app.post("/projects/999/rename", Some(&cookie), &form).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    crate::test_util::fill_project(db, id).await;
    crate::test_util::fill_project(db, PID).await;
    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(&format!("/projects/{id}/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), "/?msg=project_deleted");
    let res = app.get(&format!("/projects/{id}"), Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    for table in projects::TABLES {
        let count = |pid: i64| {
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE project_id = {pid}"
            )))
            .fetch_one(db)
        };
        assert_eq!(count(id).await.unwrap(), 0, "{table}");
        assert!(
            count(PID).await.unwrap() > 0,
            "{table} of the other project"
        );
    }
    let res = app
        .post(&format!("/projects/{id}/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn project_limit() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    for i in 1..projects::MAX_PROJECTS {
        projects::create(&app.state.db, &format!("p{i}"), "root")
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        !app.get("/", Some(&cookie))
            .await
            .body
            .contains("New project")
    );
    let form = [("_csrf", csrf.as_str()), ("name", "one more")];
    let res = app.post("/projects", Some(&cookie), &form).await;
    assert_eq!(res.redirect(), "/?msg=too_many_projects");
}

#[tokio::test]
async fn module_pages_work_without_discord() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    for module in ["discord", "greeting", "clickup", "calendar", "recurring"] {
        let res = app
            .get(&format!("/projects/{PID}/modules/{module}"), Some(&cookie))
            .await;
        assert_eq!(res.status, StatusCode::OK, "{module}");
    }

    // Enabled modules say what they miss on the project page.
    for module in ["greeting", "calendar", "recurring"] {
        let form = [("_csrf", csrf.as_str()), ("enabled", "1")];
        let path = format!("/projects/{PID}/modules/{module}/toggle");
        app.post(&path, Some(&cookie), &form).await.redirect();
    }
    let page = app.get(&format!("/projects/{PID}"), Some(&cookie)).await;
    assert!(page.body.contains("Uses ClickUp, Discord"));
    assert!(
        page.body
            .contains("but the Discord module is disabled in this project")
    );
    assert!(
        page.body
            .contains("but the ClickUp module is disabled in this project")
    );

    let greeting = app
        .get(&format!("/projects/{PID}/modules/greeting"), Some(&cookie))
        .await;
    assert!(greeting.body.contains("Discord bot token"));
    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(
            &format!("/projects/{PID}/modules/greeting/test"),
            Some(&cookie),
            &form,
        )
        .await;
    assert!(res.redirect().ends_with("?msg=bot_offline"));
    assert!(app.discord.requests().is_empty());
}

// ---------------------------------------------------------------- Discord

const DISCORD_KEY_ROUTES: [&str; 4] = [
    "/keys/discord",
    "/keys/discord/delete",
    "/keys/discord/server",
    "/keys/discord/reconnect",
];

#[tokio::test]
async fn discord_routes_require_login_and_csrf() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let module = |m: &str| format!("/projects/{PID}/modules/{m}");
    assert_eq!(app.get(&module("discord"), None).await.redirect(), "/login");
    let mut paths: Vec<String> = Vec::new();
    for m in ["discord", "greeting", "calendar"] {
        paths.extend(
            DISCORD_KEY_ROUTES
                .iter()
                .map(|r| format!("{}{r}", module(m))),
        );
    }
    paths.push(format!("{}/keys/discord/source", module("greeting")));
    for path in &paths {
        let form = [("token", crate::test_util::DISCORD_TOKEN)];
        assert_eq!(
            app.post(path, None, &form).await.redirect(),
            "/login",
            "{path}"
        );
        let form = [("token", crate::test_util::DISCORD_TOKEN), ("_csrf", "x")];
        let res = app.post(path, Some(&cookie), &form).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}");
    }
    // The Discord module holds the shared token; it has no "use the shared one" choice.
    let csrf = app.csrf(&cookie).await;
    let form = [("_csrf", csrf.as_str()), ("source", "own")];
    let res = app
        .post(
            &format!("{}/keys/discord/source", module("discord")),
            Some(&cookie),
            &form,
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(app.discord.requests().is_empty());
}

#[tokio::test]
async fn discord_token_is_checked_stored_encrypted_and_never_shown() {
    use crate::discord::mock::{BOT_NAME, OTHER_GUILD_NAME};
    use crate::test_util::DISCORD_TOKEN;

    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let base = format!("/projects/{PID}/modules/discord");
    let key = format!("{base}/keys/discord");
    let save = |token: &'static str| {
        let form = [("_csrf", csrf.clone()), ("token", token.to_owned())];
        let (app, cookie, key) = (&app, cookie.clone(), key.clone());
        async move {
            let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
            app.post(&key, Some(&cookie), &form).await
        }
    };

    let res = save("not a token").await;
    assert_eq!(res.redirect(), format!("{base}?msg=discord_token_invalid"));
    assert!(
        app.discord.requests().is_empty(),
        "checked before calling Discord"
    );
    let res =
        save("MTExMTExMTExMTExMTExMTEx.Gaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").await;
    assert_eq!(res.redirect(), format!("{base}?msg=discord_token_rejected"));
    app.discord.fail_with(Some(502));
    let res = save(DISCORD_TOKEN).await;
    assert_eq!(res.redirect(), format!("{base}?msg=discord_unreachable"));
    app.discord.fail_with(None);
    assert!(
        keys::info(db, PID, Service::Discord, "discord")
            .await
            .unwrap()
            .is_none()
    );

    let res = save(DISCORD_TOKEN).await;
    assert_eq!(res.redirect(), format!("{base}?msg=discord_token_saved"));
    let info = keys::info(db, PID, Service::Discord, "discord")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.account, BOT_NAME);
    assert_eq!(info.hint, &DISCORD_TOKEN[DISCORD_TOKEN.len() - 4..]);
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT sealed FROM api_keys WHERE service = 'discord'")
            .fetch_one(db)
            .await
            .unwrap();
    assert!(
        !sealed
            .windows(DISCORD_TOKEN.len())
            .any(|w| w == DISCORD_TOKEN.as_bytes())
    );

    // The bot is in two servers, so none is picked yet.
    let page = app.get(&base, Some(&cookie)).await;
    assert!(!page.body.contains(DISCORD_TOKEN));
    assert!(page.body.contains(BOT_NAME));
    assert!(page.body.contains(GUILD_NAME) && page.body.contains(OTHER_GUILD_NAME));
    assert!(page.body.contains("No Discord server is selected"));

    let server = format!("{key}/server");
    for (id, code) in [
        ("12345", "server_invalid"),
        (&*GUILD_ID.to_string(), "server_saved"),
    ] {
        let form = [("_csrf", csrf.as_str()), ("guild_id", id)];
        let res = app.post(&server, Some(&cookie), &form).await;
        assert_eq!(res.redirect(), format!("{base}?msg={code}"), "{id}");
    }
    let form = [("_csrf", csrf.as_str()), ("guild_id", "abc")];
    assert_eq!(
        app.post(&server, Some(&cookie), &form).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        crate::discord::connection::server(db, PID, "discord")
            .await
            .unwrap(),
        Some((GUILD_ID, GUILD_NAME.to_owned()))
    );

    // Reconnect drops the client so the next reconcile starts it again.
    crate::test_util::connect_fake_discord(&app.state, PID).await;
    let client = crate::discord::client_key(&crate::secrets::SecretString::new(DISCORD_TOKEN));
    assert!(app.state.discord.handle(&client).is_some());
    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(&format!("{key}/reconnect"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("{base}?msg=reconnecting"));
    assert!(app.state.discord.status(&client).is_none());

    let res = app
        .post(&format!("{key}/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("{base}?msg=token_removed"));
    assert!(
        keys::info(db, PID, Service::Discord, "discord")
            .await
            .unwrap()
            .is_none()
    );
    let res = app
        .post(&format!("{key}/reconnect"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("{base}?msg=discord_token_missing"));

    // Wrong token in the request log never leaks into the page or the database.
    for r in app.discord.requests() {
        assert!(r.auth.is_some());
    }
}

#[tokio::test]
async fn modules_can_use_their_own_bot() {
    use crate::discord::connection::{binding, server};
    use crate::test_util::{DISCORD_TOKEN, OTHER_DISCORD_TOKEN};

    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let base = format!("/projects/{PID}/modules/greeting");
    let key = format!("{base}/keys/discord");

    let bot = |token: &str| crate::discord::client_key(&SecretString::new(token));
    let b = binding(&app.state, PID, "greeting").await.unwrap().unwrap();
    assert_eq!(b.key, bot(DISCORD_TOKEN));

    let form = [("_csrf", csrf.as_str()), ("token", OTHER_DISCORD_TOKEN)];
    let res = app.post(&key, Some(&cookie), &form).await;
    assert_eq!(res.redirect(), format!("{base}?msg=discord_token_saved"));
    assert!(
        !keys::use_shared(db, PID, Service::Discord, "greeting")
            .await
            .unwrap()
    );
    // That bot is in a single server, which is picked right away.
    assert_eq!(
        server(db, PID, "greeting").await.unwrap(),
        Some((GUILD_ID, GUILD_NAME.to_owned()))
    );
    let b = binding(&app.state, PID, "greeting").await.unwrap().unwrap();
    assert_eq!(b.key, bot(OTHER_DISCORD_TOKEN));
    // Its bot is not connected (only the shared one is faked).
    let page = app.get(&base, Some(&cookie)).await;
    assert!(page.body.contains("The Discord bot is not connected."));

    let form = [("_csrf", csrf.as_str()), ("source", "shared")];
    let res = app
        .post(&format!("{key}/source"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("{base}?msg=source_saved"));
    let b = binding(&app.state, PID, "greeting").await.unwrap().unwrap();
    assert_eq!(b.key, bot(DISCORD_TOKEN));
    let page = app.get(&base, Some(&cookie)).await;
    assert!(page.body.contains("#events"));
    let form = [("_csrf", csrf.as_str()), ("source", "neither")];
    let res = app
        .post(&format!("{key}/source"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn greeting_test_message_goes_through_the_project_bot() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let base = format!("/projects/{PID}/modules/greeting");
    let channel = CHANNEL_ID.to_string();
    let form = [
        ("_csrf", csrf.as_str()),
        ("channel_id", channel.as_str()),
        ("template", "Hi {user}, welcome to {server} @everyone"),
        ("embed_color", "#7f8cff"),
    ];
    app.post(&base, Some(&cookie), &form).await.redirect();

    let form = [("_csrf", csrf.as_str())];
    let res = app
        .post(&format!("{base}/test"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("{base}?msg=test_sent"));
    let sent = app.discord.messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].path, format!("/channels/{CHANNEL_ID}/messages"));
    let body = sent[0].json();
    assert!(body["content"].as_str().unwrap().contains(GUILD_NAME));
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));

    app.discord.fail_with(Some(403));
    let res = app
        .post(&format!("{base}/test"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("{base}?msg=test_failed"));
    let stats = crate::modules::greeting::load_stats(&app.state.db, PID)
        .await
        .unwrap();
    assert_eq!(stats.recent.len(), 2);
    assert_eq!(stats.sent_total, 0, "tests are not counted");
}

#[tokio::test]
async fn saving_while_the_bot_is_offline_keeps_discord_settings() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let db = &app.state.db;
    let base = format!("/projects/{PID}/modules/greeting");
    let channel = CHANNEL_ID.to_string();
    let save = |channel_id: String, template: &'static str| {
        let form = [
            ("_csrf", csrf.clone()),
            ("channel_id", channel_id),
            ("template", template.to_owned()),
            ("embed_color", "#7f8cff".to_owned()),
        ];
        let (app, cookie, base) = (&app, cookie.clone(), base.clone());
        async move {
            let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
            app.post(&base, Some(&cookie), &form).await
        }
    };
    save(channel.clone(), "Hi").await.redirect();
    set_tag_role(db, PID, "meeting", Some(ROLE_ID), "root")
        .await
        .unwrap();

    app.state.discord.forget(&crate::discord::client_key(
        &crate::secrets::SecretString::new(crate::test_util::DISCORD_TOKEN),
    ));
    let page = app.get(&base, Some(&cookie)).await;
    assert!(
        page.body
            .contains(&format!("{CHANNEL_ID} (saved, bot not connected)"))
    );

    // The saved channel stays, another one cannot be checked and is refused.
    let res = save(channel, "Hello").await;
    assert_eq!(res.redirect(), format!("{base}?msg=saved"));
    let cfg = crate::modules::greeting::load_config(db, PID)
        .await
        .unwrap();
    assert_eq!(
        (cfg.channel_id, cfg.template.as_str()),
        (Some(CHANNEL_ID as i64), "Hello")
    );
    let res = save(crate::test_util::OTHER_CHANNEL_ID.to_string(), "Hello").await;
    assert_eq!(res.redirect(), format!("{base}?msg=invalid_channel"));

    // Tag roles cannot be checked either, and are left alone.
    let form = [
        ("_csrf", csrf.as_str()),
        ("tag_0", "meeting"),
        ("role_0", ""),
    ];
    let res = app
        .post(
            &format!("/projects/{PID}/modules/clickup/tag-roles"),
            Some(&cookie),
            &form,
        )
        .await;
    assert!(res.redirect().ends_with("?msg=bot_offline"));
    assert_eq!(tag_role_rows(db, PID).await.unwrap().len(), 1);
    let page = app
        .get(&format!("/projects/{PID}/modules/clickup"), Some(&cookie))
        .await;
    assert!(!page.body.contains("mapped role was deleted"));
}

#[tokio::test]
async fn old_server_links_redirect_to_their_project() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let guild: u64 = 1187654321098765432;
    sqlx::query(
        "INSERT INTO projects (name, legacy_guild_id, auto_name, created_at, updated_at)
         VALUES ('Old', ?, 1, 0, 0)",
    )
    .bind(guild as i64)
    .execute(&app.state.db)
    .await
    .unwrap();
    let id = projects::by_legacy_guild(&app.state.db, guild)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        app.get(&format!("/guilds/{guild}"), None).await.redirect(),
        "/login"
    );
    for (from, to) in [
        (format!("/guilds/{guild}"), format!("/projects/{id}")),
        (
            format!("/guilds/{guild}/modules/calendar/rules/3"),
            format!("/projects/{id}/modules/calendar/rules/3"),
        ),
    ] {
        assert_eq!(app.get(&from, Some(&cookie)).await.redirect(), to);
    }
    for missing in [
        format!("/guilds/{}", guild + 1),
        format!("/guilds/{guild}/users"),
        format!("/guilds/{guild}/modules/x%2F..%2F..%2Fusers"),
        format!("/guilds/{guild}/modules//x"),
        "/guilds/abc".to_owned(),
    ] {
        let res = app.get(&missing, Some(&cookie)).await;
        assert!(
            matches!(res.status, StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST),
            "{missing}: {}",
            res.status
        );
    }
}

#[tokio::test]
async fn overview_shows_projects_and_bots() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let res = app.get("/", Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Discord bots"));
    assert!(res.body.contains(&format!("Discord: {GUILD_NAME}")));
    assert!(res.body.contains(crate::test_util::PROJECT_NAME));
}
