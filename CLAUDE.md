# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project goals

We are creating a discord bot.

We want it to:
 * have a hosted web server with login and dashboard
 * login may be very simple, but should be secure
 * dashboard should display bot statistics and allow configuration of bot settings
 * root account should exist with the ability to manage other user accounts
 * new user accounts may be created by the root account
 * no mailing or email verification is required for user accounts
 * while adding new features, we want to ensure that the bot remains secure and user-friendly and has proper configuration options in web dashboard
 * Every feature/module/command may be enabled or disabled from the web dashboard - and has some useful configuration options and statistics displayed in the dashboard

## Commands

Rust edition 2024, MSRV 1.94 (`rust-version` in `Cargo.toml`, set by sqlx 0.9; the CI `msrv` job reads it from there). Cargo is installed on the host.

```sh
cargo build
cargo run                          # serve: dashboard on WEB_BIND, bot if DISCORD_TOKEN is set
cargo run -- reset-root            # print a new root password, sign root out everywhere
cargo test
cargo test locks_ip_after_limit    # single test by name (substring match)
cargo test auth::                  # all tests in a module path
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo deny check                   # advisories, licenses, bans, sources (deny.toml)
mkdir -p data                      # bind mounted at /data, must exist before compose up
docker compose up -d --build       # dashboard published on 127.0.0.1:8080
docker compose logs bot            # first start prints the generated root password
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, the MSRV check, cargo-deny and a docker build with a `/healthz` + login smoke test. Keep all of them green; CI uses `--locked`, so commit `Cargo.lock` changes. A new license in the dependency tree must be added to `deny.toml`. On `master` pushes, after all of those pass, the `publish` jobs build `linux/amd64` + `linux/arm64` on native runners and push `scorpioorganization/scorpio-discord-bot` (`latest`, `sha-<commit>`) to Docker Hub using the `DOCKER_HUB` secret. The image must keep building for arm64.

Config is read from env / `.env` (see `.env.example`, `src/config.rs`). A new env var must be added to `Config`, `.env.example` and the table in `README.md`. Without `DISCORD_TOKEN` only the dashboard runs, which is enough to work on the web side.

## Architecture

One binary, one tokio runtime. `main.rs` builds an `AppState` (`src/state.rs`) and shares it between the serenity bot client and the axum dashboard (`src/web`). An hourly `maintenance` task in `main.rs` prunes sessions, module logs and the login limiter.

The bot is optional and must never take the dashboard down. Token rejection, disallowed intents or a client crash are stored with `AppState::set_bot_error` and shown on the dashboard instead of exiting. `AppState::discord()` is `None` when no client exists, so web code must handle that.

### Build time embedding

Migrations (`sqlx::migrate!`), templates (askama) and `static/*` (`include_str!` in `src/web/mod.rs`) are compiled into the binary. Changes there need a rebuild, and any new directory needed at compile time must be added to the `COPY` lines in `Dockerfile`. SQL uses runtime `sqlx::query*` functions, not the `query!` macros, so no database is needed to compile; keep it that way.

### Database

SQLite in WAL mode. Migrations run on startup. Add a new numbered file in `migrations/` for schema changes; never edit an applied migration (sqlx checksums them). Discord snowflakes are stored as `i64` through `db::sf()`, timestamps as unix seconds from `db::now()`.

### Modules

Every bot feature is a `Module` (`src/modules/mod.rs`) registered in `modules::registry()`. `greeting` is the reference implementation (`mod.rs` logic and storage, `web.rs` dashboard routes, `template.rs` placeholder rendering).

- Per guild on/off lives in the generic `guild_modules` table (`is_enabled` / `set_enabled`). Modules are off by default. The shared toggle route `POST /guilds/{gid}/modules/{mid}/toggle` is generic; a module page posts `back=module` to return to itself.
- All time counters go in the generic `module_stats` table via `bump_counter` / `counter`. Module specific config and logs get their own tables. `BotStats` in `state.rs` holds in memory counters that reset on restart.
- `Module::routes()` is merged into the main router; paths must start with `/guilds/{gid}/modules/<id>`. Handlers call `web::require_guild` first, which 404s for guilds the bot is not in (looked up from the serenity cache).
- `Module::summary()` feeds the module card on the guild page; the guild page lists every registered module automatically.
- `Module::intents()` is unioned into the gateway intents (`bot::wanted_intents`). Privileged intents switched off in the developer portal are dropped at connect time so the bot still comes online; the dashboard warns using `AppState::missing_intents` + `bot::privileged_names`.
- Event dispatch is explicit: `Handler` in `src/bot/mod.rs` currently only routes `guild_member_addition` to `Module::on_member_join`, checking `is_enabled` per module. A new event type needs a default method on the trait plus a matching dispatch loop in `Handler`.
- Log pruning is not part of the trait: `maintenance` in `main.rs` calls `greeting::prune_log` directly, so a new module with a log table needs its own call there.
- Messages sent to Discord must restrict pings with `CreateAllowedMentions` (see `greeting::send`) so templates or user names cannot ping `@everyone` or roles.

### Tests

- Unit tests live next to the code in `#[cfg(test)] mod tests`. DB tests use `test_util::pool()` (in-memory SQLite, migrated, single connection because every `:memory:` connection is its own database).
- Dashboard tests (`src/web/tests.rs`) use `test_util::TestApp`: the real `web::router` with a root account, driven with `tower::ServiceExt::oneshot` (no socket, no Discord, so `require_guild` 404s). Helpers: `login`, `login_from(ip, ..)`, `csrf`, `create_user`, `post`/`get` with a cookie, `TestResponse::redirect()`.
- New routes need tests for: login required, CSRF rejected, root only where applicable, and the happy path redirect code.
- Argon2 is built with `opt-level = 3` in the dev profile (`Cargo.toml`), otherwise login heavy tests take seconds.

### Web dashboard and security conventions

- `auth::extract::session_middleware` runs on every request: loads the session and, for every non GET/HEAD/OPTIONS request except `/login`, requires a `_csrf` field in a urlencoded form body (64 KiB limit). Every POST form needs `<input type="hidden" name="_csrf" value="{{ page.csrf }}">`. JSON or multipart endpoints would need middleware changes.
- Use the `CurrentUser` extractor for any logged in user and `RootUser` for account management. Only root manages accounts; every account can configure the bot.
- Handlers return `AppResult<T>`; `AppError::Internal` logs the detail and shows a generic page. Non handler code uses `anyhow`.
- After a POST, redirect with `web::redirect_msg(path, code)`. The code must be added to `Flash::from_code` in `src/web/mod.rs`; only fixed messages are shown, user input is never reflected.
- Askama page structs carry `page: PageCtx` and templates extend `templates/base.html`.
- Strict CSP (`src/web/mod.rs`): no inline scripts, inline styles or `on*` attributes. All JS goes in `static/app.js` as progressive enhancement (pages must work without JS), all CSS in `static/app.css`. External images only from `cdn.discordapp.com`.
- Passwords use Argon2id on a blocking thread; session tokens are stored only as SHA-256; login is throttled per IP and per username (`auth/limiter.rs`, in memory).
