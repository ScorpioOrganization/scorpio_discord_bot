use axum::http::{Method, StatusCode, header};

use crate::auth::ROOT_USERNAME;
use crate::clickup::mapping::Explicit;
use crate::clickup::{mock, tokens};
use crate::modules::calendar::{self, Kind, Rule};
use crate::modules::clickup::{explicit_mappings, set_tag_role, tag_role_rows};
use crate::test_util::{
    CHANNEL_ID, EMOJI_ID, GUILD_ID, GUILD_NAME, IP, LEADS_ROLE_ID, MEMBER_ID, ROLE_ID, ROOT_PW,
    TestApp, TestResponse, VOICE_CHANNEL_ID,
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
async fn guild_routes_404_without_discord() {
    let app = TestApp::new().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;

    for path in [
        "/guilds/1",
        "/guilds/1/modules/greeting",
        "/guilds/1/modules/clickup",
        "/guilds/1/modules/calendar",
        "/guilds/1/modules/calendar/rules/new?kind=digest",
        "/guilds/1/modules/calendar/rules/1",
    ] {
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
        ("/guilds/1/modules/clickup/toggle", vec![("enabled", "1")]),
        (
            "/guilds/1/modules/clickup/token",
            vec![("token", mock::TOKEN)],
        ),
        ("/guilds/1/modules/clickup/token/delete", vec![]),
        ("/guilds/1/modules/clickup/test", vec![]),
        (
            "/guilds/1/modules/clickup/workspace",
            vec![("team_id", mock::TEAM_ID)],
        ),
        ("/guilds/1/modules/clickup/mapping", vec![("map_3", "-")]),
        (
            "/guilds/1/modules/clickup/tag-roles",
            vec![("new_tag", "meeting"), ("new_role", "5")],
        ),
        ("/guilds/1/modules/clickup/members/refresh", vec![]),
        ("/guilds/1/modules/calendar/toggle", vec![("enabled", "1")]),
        (
            "/guilds/1/modules/calendar",
            vec![("time_field", "due"), ("timezone", "UTC")],
        ),
        ("/guilds/1/modules/calendar/refresh", vec![]),
        (
            "/guilds/1/modules/calendar/token",
            vec![("token", mock::TOKEN)],
        ),
        (
            "/guilds/1/modules/calendar/token/source",
            vec![("source", "own")],
        ),
        ("/guilds/1/modules/calendar/rules", vec![("kind", "digest")]),
        ("/guilds/1/modules/calendar/rules/1", vec![("name", "x")]),
        ("/guilds/1/modules/calendar/rules/1/delete", vec![]),
        ("/guilds/1/modules/calendar/rules/1/test", vec![]),
    ];
    for (path, mut form) in posts {
        form.push(("_csrf", &csrf));
        let res = app.post(path, Some(&cookie), &form).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}");
    }
    let db = &app.state.db;
    assert!(!crate::modules::is_enabled(db, 1, "greeting").await.unwrap());
    let saved: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM greeting_config) + (SELECT COUNT(*) FROM clickup_tokens)
              + (SELECT COUNT(*) FROM clickup_config) + (SELECT COUNT(*) FROM clickup_user_map)
              + (SELECT COUNT(*) FROM clickup_token_source)
              + (SELECT COUNT(*) FROM clickup_tag_roles)
              + (SELECT COUNT(*) FROM calendar_config) + (SELECT COUNT(*) FROM calendar_rules)",
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
async fn guild_pages_render_with_connected_bot() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;

    let res = app.get("/", Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(GUILD_NAME));

    let res = app.get(&format!("/guilds/{GUILD_ID}"), Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(GUILD_NAME));
    assert!(
        res.body
            .contains(&format!("/guilds/{GUILD_ID}/modules/greeting"))
    );

    let res = app
        .get(
            &format!("/guilds/{GUILD_ID}/modules/greeting"),
            Some(&cookie),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("#events"));
    assert!(
        !res.body.contains("Voice"),
        "only text channels are offered"
    );

    let res = app.get("/guilds/1/modules/greeting", Some(&cookie)).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn greeting_toggle_and_save() {
    let app = TestApp::with_guild().await;
    let cookie = app.login_root().await;
    let csrf = app.csrf(&cookie).await;
    let base = format!("/guilds/{GUILD_ID}/modules/greeting");
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
        crate::modules::is_enabled(db, GUILD_ID, "greeting")
            .await
            .unwrap()
    );

    let form = [("_csrf", csrf.as_str()), ("enabled", "0")];
    let res = app
        .post(&format!("{base}/toggle"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), format!("/guilds/{GUILD_ID}?msg=disabled"));

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

    let cfg = crate::modules::greeting::load_config(db, GUILD_ID)
        .await
        .unwrap();
    assert_eq!(cfg.channel_id, Some(CHANNEL_ID as i64));
    assert_eq!(cfg.template, "Hi {user}");
    assert!(cfg.use_embed);
    assert_eq!(cfg.embed_color, 0x123456);
}

const CLICKUP_ROUTES: [&str; 7] = [
    "/token",
    "/token/delete",
    "/test",
    "/workspace",
    "/mapping",
    "/members/refresh",
    "/tag-roles",
];

fn clickup_path(suffix: &str) -> String {
    format!("/guilds/{GUILD_ID}/modules/clickup{suffix}")
}

/// Saves the mock's valid key through the dashboard.
async fn save_clickup_key(app: &TestApp, cookie: &str, csrf: &str) {
    let form = [("_csrf", csrf), ("token", mock::TOKEN)];
    let res = app.post(&clickup_path("/token"), Some(cookie), &form).await;
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
        .post(&clickup_path("/token/source"), Some(&cookie), &form)
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
    assert!(res.body.contains("No key saved yet."));

    let cases = [
        ("garbage", "token_invalid"),
        ("pk_1_WRONG_KEY_VALUE", "token_rejected"),
    ];
    for (token, code) in cases {
        let form = [("_csrf", csrf.as_str()), ("token", token)];
        let res = app
            .post(&clickup_path("/token"), Some(&cookie), &form)
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
        .post(&clickup_path("/token"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=clickup_unreachable"));
    app.clickup.fail_with(None);
    assert!(
        tokens::info(db, GUILD_ID, "clickup")
            .await
            .unwrap()
            .is_none()
    );

    save_clickup_key(&app, &cookie, &csrf).await;
    let sealed: Vec<u8> = sqlx::query_scalar("SELECT sealed FROM clickup_tokens")
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
        .post(&clickup_path("/token/delete"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), clickup_path("?msg=token_removed"));
    assert!(
        tokens::info(db, GUILD_ID, "clickup")
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
    let map = explicit_mappings(db, GUILD_ID).await.unwrap();
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
    assert_eq!(explicit_mappings(db, GUILD_ID).await.unwrap(), map);

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

    let summary = app.get(&format!("/guilds/{GUILD_ID}"), Some(&cookie)).await;
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
    let everyone = GUILD_ID.to_string();

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
    let rows = tag_role_rows(db, GUILD_ID).await.unwrap();
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
        assert_eq!(tag_role_rows(db, GUILD_ID).await.unwrap().len(), 1);
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
    let rows = tag_role_rows(db, GUILD_ID).await.unwrap();
    let pairs: Vec<(&str, u64)> = rows
        .iter()
        .map(|r| (r.tag.as_str(), r.role_id as u64))
        .collect();
    assert_eq!(pairs, [("standup", ROLE_ID), ("team sync", LEADS_ROLE_ID)]);

    // A mapping to a role deleted in Discord is flagged.
    set_tag_role(db, GUILD_ID, "old", Some(999), "root")
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
    let info = tokens::info(&app.state.db, GUILD_ID, "clickup")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.updated_by.as_deref(), Some("alice"));
}

fn calendar_path(suffix: &str) -> String {
    format!("/guilds/{GUILD_ID}/modules/calendar{suffix}")
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
        "/token",
        "/token/delete",
        "/token/source",
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
    let cfg = calendar::load_config(db, GUILD_ID).await.unwrap();
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
    let cfg = calendar::load_config(db, GUILD_ID).await.unwrap();
    assert_eq!(cfg.list_id.as_deref(), Some(mock::OTHER_LIST_ID));
    assert_eq!(cfg.list_name.as_deref(), Some("Events / Misc"));
    assert_eq!(cfg.tag_filter, "meeting, event");
    assert!(cfg.start_mode() && cfg.include_subtasks && !cfg.include_closed);

    // Switching to its own key: none saved yet, so ClickUp is not used.
    let form = [("_csrf", csrf.as_str()), ("source", "own")];
    let res = app
        .post(&calendar_path("/token/source"), Some(&cookie), &form)
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
        .post(&calendar_path("/token/source"), Some(&cookie), &form)
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let form = [("_csrf", csrf.as_str()), ("token", mock::TOKEN)];
    let res = app
        .post(&calendar_path("/token"), Some(&cookie), &form)
        .await;
    assert_eq!(res.redirect(), calendar_path("?msg=token_saved"));
    assert!(!tokens::use_shared(db, GUILD_ID, "calendar").await.unwrap());
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
    let rid = calendar::insert_rule(&app.state.db, GUILD_ID, &rule, "root")
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
    assert_eq!(calendar::count_rules(db, GUILD_ID).await.unwrap(), 0);
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
    let rules = calendar::list_rules(db, GUILD_ID).await.unwrap();
    let rule = &rules[0];
    assert_eq!(rule.name, "Morning agenda");
    assert_eq!(rule.days, 0b1_0001);
    assert_eq!(rule.time_of_day, 8 * 60 + 30);
    assert_eq!(rule.channel_id, Some(CHANNEL_ID));
    assert_eq!(rule.reactions.len(), 2);
    assert_eq!(rule.timezone, "Europe/Warsaw");
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
    let path = calendar_path(&format!("/rules/{}", rule.id));
    let res = post_owned(&app, &path, &cookie, &edit).await;
    assert_eq!(res.redirect(), format!("{path}?msg=saved"));
    let saved = calendar::load_rule(db, GUILD_ID, rule.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.name, "Renamed");
    assert_eq!(saved.kind, Kind::Digest);
    assert!(saved.mention_assignees && saved.mention_roles);
    assert_eq!(saved.header_template, "", "the header is optional");
    assert_eq!(saved.timezone, "America/New_York");

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
    set_tag_role(db, GUILD_ID, "meeting", Some(ROLE_ID), "root")
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
    assert_eq!(calendar::count_rules(db, GUILD_ID).await.unwrap(), 1);
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
        calendar::insert_rule(&app.state.db, GUILD_ID, &Rule::new(Kind::Digest), "root")
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
    let no_channel = calendar::insert_rule(db, GUILD_ID, &rule, "root")
        .await
        .unwrap();
    rule.channel_id = Some(CHANNEL_ID);
    let rid = calendar::insert_rule(db, GUILD_ID, &rule, "root")
        .await
        .unwrap();
    let test_path = |id: i64| calendar_path(&format!("/rules/{id}/test"));

    let res = app.post(&test_path(no_channel), Some(&cookie), &form).await;
    assert!(res.redirect().ends_with("?msg=no_channel"));
    let res = app.post(&test_path(rid), Some(&cookie), &form).await;
    assert!(
        res.redirect().ends_with("?msg=calendar_no_list"),
        "{}",
        res.location()
    );

    // With a list, the post reaches the (closed) Discord port and fails.
    shared_clickup_key(&app, &cookie, &csrf).await;
    select_list(&app, &cookie, &csrf).await;
    let res = app.post(&test_path(rid), Some(&cookie), &form).await;
    assert!(
        res.redirect().ends_with("?msg=test_failed"),
        "{}",
        res.location()
    );
    let log = calendar::recent_log(db, GUILD_ID, 1).await.unwrap();
    assert!(log[0].is_test && !log[0].success);
    assert_eq!(
        crate::modules::counter(db, Some(GUILD_ID), "calendar", "failed")
            .await
            .unwrap(),
        0,
        "tests are not counted"
    );
}
