# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project goals

We are creating an automation bot. Integrations with other services (Discord, ClickUp) are modules; work is grouped in projects.

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
cargo run                          # serve: dashboard on WEB_BIND, plus the Discord bots enabled modules use
cargo run -- reset-root            # print a new root password, sign root out everywhere
cargo test
cargo test locks_ip_after_limit    # single test by name (substring match)
cargo test auth::                  # all tests in a module path
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo deny check                   # advisories, licenses, bans, sources (deny.toml); not installed on the host, see README for the Docker one-liner
mkdir -p data                      # bind mounted at /data, must exist before compose up
docker compose up -d --build       # dashboard published on 127.0.0.1:8080
docker compose logs bot            # first start prints the generated root password
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, the MSRV check, cargo-deny, a docker build with a `/healthz` + login smoke test, and an `upgrade` job that starts the previous `:latest` image, seeds `ci/legacy_v6_seed.sql` into its database and checks the new image migrates it. Keep all of them green; CI uses `--locked`, so commit `Cargo.lock` changes. A new license in the dependency tree must be added to `deny.toml`. On `master` pushes, after all of those pass, the `publish` jobs build `linux/amd64` + `linux/arm64` on native runners and push `scorpioorganization/scorpio-discord-bot` (`latest`, `sha-<commit>`) to Docker Hub using the `DOCKER_HUB` secret. The image must keep building for arm64.

Config is read from env / `.env` (see `.env.example`, `src/config.rs`). A new env var must be added to `Config`, `.env.example` and the table in `README.md`. Bot tokens and API keys are not config: they are entered on the dashboard per project. `DISCORD_TOKEN` is deprecated and only read by the one-time import in `upgrade.rs`.

## Architecture

One binary, one tokio runtime. `main.rs` opens the database (migrations, with a backup first, see Database), then `prepare` builds an `AppState` (`src/state.rs`) and runs `upgrade::run`; the state is shared by the axum dashboard (`src/web`), the Discord bots (`src/discord`) and `scheduler::run`, which every minute reconciles the Discord connections and calls `Module::on_tick` (see Modules). An hourly `maintenance` task in `main.rs` prunes sessions, module logs, the login limiter, the member cache and the TTL cache.

`AppState` also carries `vault` (`secrets::Vault`, encrypts stored keys), `clickup` (the shared `ClickUpClient`, pointed at a mock in tests), `discord` (`discord::hub::DiscordHub`, the running bots), `members` (`discord::members::MemberCache`, server member lists for name matching, 10 min, keyed by Discord server id) and `cache` (`cache::TtlCache`, typed key/value cache for data fetched from other services; readers pass a max age).

### Projects

Projects (`src/projects.rs`, table `projects`) are the top level container: module on/off, module config, logs, counters and API keys are all per `project_id` (an `i64`). Any account creates and renames projects; only root deletes (`projects::delete` clears every table in `projects::TABLES`; a test compares that list with the schema, so a new table with `project_id` must be added there). Routes live under `/projects/{pid}`; `web::require_project` 404s unknown ids.

Discord is just another service: its module (`modules::discord`) holds the shared bot token and server, like `modules::clickup` holds the shared ClickUp key.

### Upgrade from per server data

Before projects everything was keyed by Discord guild id and the token came from `DISCORD_TOKEN`. `migrations/0007_projects.sql` creates one project per guild id found in any table (`legacy_guild_id`, placeholder name with `auto_name`, renamed from the guild once a bot sees it), renames every `guild_id` column to `project_id` in place (no table rebuilds: rebuilding `calendar_rules` would cascade into `calendar_sent`), and moves ClickUp keys into `api_keys` with `legacy_guild_id` set. `upgrade::run` (every start, idempotent, never fatal) reseals those keys for their project (`keys::reseal_legacy`; `keys::load` falls back to the old context until then) and imports `DISCORD_TOKEN` once (`app_meta.discord_env_imported`). `db::connect` copies the file to `<db>.pre-v<N>.bak` with `VACUUM INTO` before applying pending migrations. Old `/guilds/{id}/...` GET links redirect to the project. Tests: `upgrade.rs` (fixture `ci/legacy_v6_seed.sql`, `MIGRATOR.run_to(6)`), `db.rs` (backup).

### Discord integration

`src/discord`, used by every module that talks to Discord:

- `hub.rs`: one serenity client per distinct bot token in use (`client_key` = SHA-256 of the token, memory only). `desired` lists the tokens of the Discord module where enabled plus the resolved token of every enabled module that declares `Service::Discord`; `plan` (pure) decides starts/stops, keeping running clients within `MAX_CLIENTS`; failed clients are retried after `RETRY_AFTER` unless the token was rejected. `reconcile` runs at startup and every minute, `DiscordHub::reconcile_soon` after dashboard changes. The hub is not `live` in tests: it never connects, `reconcile` only reports what it would start.
- `connection.rs`: a Discord key is saved with its server (`discord_servers`, same owner as in `api_keys`). `binding` resolves the token and server a module uses (shared or own, see `keys::resolve`); `connection` adds the running client and checks the bot is in the server, else a `DiscordProblem` that pages show. `subscribers` finds the (project, module) pairs a gateway event of one bot in one server goes to.
- `mod.rs`: intents (`wanted_intents`, privileged ones the portal disallows are dropped at connect), `build_client`, the event `Handler` (member joins dispatched through `subscribers`; `ready` fills bot names, `guild_create` names migrated projects), and HTTP helpers for the dashboard (`current_user` to check a token, `guilds` for the server picker, cached). `http(proxy, token)` points serenity at `MockDiscord` in tests.
- `info.rs`, `members.rs`: read only views of a connection's cache. `info::channel_choices` keeps the saved channel selectable while the bot is unreachable, so saving other settings never clears it.

The bots are optional and must never take the dashboard down: failures are stored in the hub and shown on the dashboard. Pages render without any Discord connection; handle `connection(..)` returning a problem.

### Build time embedding

Migrations (`sqlx::migrate!`), templates (askama) and `static/*` (`include_str!` in `src/web/mod.rs`) are compiled into the binary. Changes there need a rebuild, and any new directory needed at compile time must be added to the `COPY` lines in `Dockerfile`. SQL uses runtime `sqlx::query*` functions, not the `query!` macros, so no database is needed to compile; keep it that way.

### Database

SQLite in WAL mode. Migrations (`db::MIGRATOR`) run on startup, after a backup copy when any are pending. Add a new numbered file in `migrations/` for schema changes; never edit an applied migration (sqlx checksums them). Migrations run unattended on the production server (auto deploy from `master`), so test a data migration against a seeded old schema (`MIGRATOR.run_to`). Discord snowflakes are stored as `i64` through `db::sf()`, timestamps as unix seconds from `db::now()`.

### Modules

Every feature is a `Module` (`src/modules/mod.rs`) registered in `modules::registry()`. `greeting` is the reference implementation (`mod.rs` logic and storage, `web.rs` dashboard routes, `template.rs` placeholder rendering); `discord` and `clickup` are the service modules holding shared keys.

- Per project on/off lives in the generic `project_modules` table (`is_enabled` / `set_enabled`). Modules are off by default. The shared toggle route `POST /projects/{pid}/modules/{mid}/toggle` is generic; a module page posts `back=module` to return to itself. Toggling a module that uses Discord reconciles the bots.
- `Module::services()` lists the services (`keys::Service`) a module needs a key for. The project page shows them and, for enabled modules, what is missing; `Service::Discord` also keeps the module's bot connected and routes Discord events to it.
- All time counters go in the generic `module_stats` table via `bump_counter` / `counter`. Module specific config and logs get their own tables. `BotStats` in `state.rs` holds in memory counters that reset on restart.
- `Module::routes()` is merged into the main router; paths must start with `/projects/{pid}/modules/<id>`. Handlers call `web::require_project` first.
- `Module::summary()` feeds the module card on the project page; the project page lists every registered module automatically.
- `Module::intents()` is unioned into the gateway intents of every bot (`discord::wanted_intents`). Privileged intents switched off in the developer portal are dropped at connect time so the bot still comes online; pages warn by comparing with the connection's `handle.intents` (`discord::privileged_names`).
- Event dispatch is explicit: `Handler` in `src/discord/mod.rs` currently only routes `guild_member_addition` to `Module::on_member_join(ctx, state, project_id, member)` through `connection::subscribers`. A new event type needs a default method on the trait plus a matching dispatch in `Handler`.
- Periodic work: a module returns `true` from `ticks()` and implements `on_tick(state, project_id)`. `scheduler` runs it about once a minute for every project the module is enabled in, Discord or not, concurrently with a 50 s timeout; errors are logged. A module that needs Discord resolves its `connection` and returns early without one (calendar does, so nothing is marked handled while the bot is away). Keep ticks cheap when there is nothing to do (no remote calls). `calendar` is the reference (`runner::run` takes a `now` and an `Outbox` so it is testable).
- Log pruning is not part of the trait: `maintenance` in `main.rs` calls `greeting::prune_log`, `calendar::prune` and `recurring::prune` directly, so a new module with a log table needs its own call there.
- Messages sent to Discord must restrict pings with `CreateAllowedMentions` (see `greeting::send`, `calendar::runner::DiscordOutbox`) so templates or user names cannot ping `@everyone` or roles. Roles may only be allowed when they come from the tag mapping and the rule enables it (`OutMessage::mention_roles`).
- User configurable templates use `modules::text` (`render` with a closure, `escape_markdown`, `truncate_chars`, `parse_hex_color`).
- `recurring` is the only module that changes ClickUp. ClickUp's API exposes no recurrence settings, so its rules map a tag to an interval. `plan.rs` (pure) decides which tasks recur, shared by the run and the dashboard preview; `schedule.rs` does the date math in the configured time zone with `jiff`. Safety rails to keep: dry run on by default, at most `MAX_WRITES_PER_RUN` writes and a 30 s budget per run, `recurring_done` markers per (task, old due date), written before a copy is created and only removed when ClickUp clearly refused it, copy mode only for tasks due in the last 60 days, and a run lock in `recurring_config.running_since` so a tick and "Run now" never overlap.

### ClickUp integration

Shared layer in `src/clickup` (client, mapping, mock) plus the generic key layer in `src/keys`, used by the `clickup` module (the shared key, workspace and people mapping), by `calendar` and by `recurring`. Future ClickUp modules must reuse it:

- `client.rs`: the only way to talk to ClickUp. Everything reads except `update_task` (`PUT /task/{id}`: status, due and start date) and `create_task` (`POST /list/{id}/task`), which only the `recurring` module may call. Every other module stays read only, and their tests assert they send only GET requests; do not add write methods without a module that needs them and a dry run for it. Errors never contain the key; a 400 becomes `ClickUpError::Rejected` with ClickUp's short explanation. `mock.rs` (tests) is an in-process fake API that filters and pages tasks like ClickUp (per list and per workspace), accepts those two writes (changing its stored tasks, every other write gets 405) and records requests with their bodies (`writes()`, `Recorded::json`).
- `keys/mod.rs` (all services): per project keys in `api_keys` (`service`, `owner`), sealed by `secrets::Vault` with the row (`key:{service}:{project}:{owner}`) as authenticated data. `owner` is the service's module (`Service::shared_owner`: `clickup`, `discord`) or a module id (own key); `api_key_source` says which one a module uses (shared by default, and only while the service's module is enabled). Use `resolve(state, project, service, module_id)` to get a key and wrap every API call in `track(..)`, which records last success / error and `api_ok` / `api_err` counters. Keys are write-only: never render, log or return them.
- `keys/web.rs`: `key_routes(service, owner)` + `key_view` + `templates/keys/cards.html` (a page puts its views in `keys: Vec<KeyView>`) give a module the key card: save after a check with the service (`GET /user`, Discord `GET /users/@me`), remove, shared or own, and for Discord the server picker (only servers the bot is in) and Reconnect.
- `mapping.rs`: pure name matching (`Resolver`); `modules::clickup::resolver(state, project, conn, people)` combines it with the explicit mappings and the member list of the connection's server.
- Tag to role mappings live in `clickup_tag_roles` (tags lowercase, `modules::clickup::normalize_tag`); `modules::clickup::tag_roles(state, project, conn)` returns them without roles deleted in the connection's server. They back the `{roles}` placeholder. People mapping and tag roles use the project's shared Discord bot (`connection(.., "clickup")`); without it the tag role form refuses to save rather than dropping mappings.
- `secrets.rs`: the key comes from `SECRETS_KEY` or a generated `secret.key` next to the database (ephemeral for `:memory:`). Losing it means keys and bot tokens must be re-entered.

### Tests

- Unit tests live next to the code in `#[cfg(test)] mod tests`. DB tests use `test_util::pool()` (in-memory SQLite, migrated, single connection because every `:memory:` connection is its own database).
- Dashboard tests (`src/web/tests.rs`) use `test_util::TestApp`: the real `web::router` with a root account and one project (`app.pid`, equal to `test_util::PID` in a fresh database), driven with `tower::ServiceExt::oneshot` (no socket, no Discord). Helpers: `login`, `login_from(ip, ..)`, `csrf`, `create_user`, `post`/`get` with a cookie, `TestResponse::redirect()`. `test_util::projects(db, [names])` and `fill_project(db, pid)` (a row in every project table) help unit tests.
- New routes need tests for: login required, CSRF rejected, root only where applicable, and the happy path redirect code.
- `TestApp::with_guild()` (or `test_util::connect_fake_discord(&state, pid)`) gives the project the Discord module (enabled) with `test_util::DISCORD_TOKEN` in server `test_util::GUILD_ID` and inserts a fake running bot for it: text channels, a voice channel, a custom emoji and one member. Every `TestApp` runs a `MockDiscord` (`app.discord`, `discord::mock`) that all Discord HTTP calls go to: it knows `DISCORD_TOKEN` and `OTHER_DISCORD_TOKEN`, lists their servers, accepts messages and reactions and records them (`messages()`, `fail_with`). Without a `TestApp`, Discord HTTP goes to a closed local port and fails fast, never reaching discord.com. Every `TestApp` also runs a `MockClickUp` (`app.clickup`) that the app's `ClickUpClient` talks to; key `mock::TOKEN`, list `mock::LIST_ID`.
- Calendar runner tests use `runner::tests::RecordingOutbox` and fixed timestamps instead of Discord and the clock.
- Recurring runner tests call `runner::run(state, project, now, force)` with fixed timestamps against `MockClickUp` and check `mock.writes()` and `mock.tasks(list)`.
- Askama escapes `<` and `>` as `&#60;` and `&#62;`, check rendered HTML for those.
- Argon2 is built with `opt-level = 3` in the dev profile (`Cargo.toml`), otherwise login heavy tests take seconds.

### Web dashboard and security conventions

- `auth::extract::session_middleware` runs on every request: loads the session and, for every non GET/HEAD/OPTIONS request except `/login`, requires a `_csrf` field in a urlencoded form body (64 KiB limit). Every POST form needs `<input type="hidden" name="_csrf" value="{{ page.csrf }}">`. JSON or multipart endpoints would need middleware changes.
- Use the `CurrentUser` extractor for any logged in user and `RootUser` for account management. Only root manages accounts; every account can configure the bot.
- Handlers return `AppResult<T>`; `AppError::Internal` logs the detail and shows a generic page. Non handler code uses `anyhow`.
- After a POST, redirect with `web::redirect_msg(path, code)`. The code must be added to `Flash::from_code` in `src/web/mod.rs`; only fixed messages are shown, user input is never reflected.
- Askama page structs carry `page: PageCtx` and templates extend `templates/base.html`.
- Strict CSP (`src/web/mod.rs`): no inline scripts, inline styles or `on*` attributes. All JS goes in `static/app.js` as progressive enhancement (pages must work without JS), all CSS in `static/app.css`. External images only from `cdn.discordapp.com`. Placeholder / emoji buttons use `data-insert` with `data-target` (input id) and optional `data-append`.
- Passwords use Argon2id on a blocking thread; session tokens are stored only as SHA-256; login is throttled per IP and per username (`auth/limiter.rs`, in memory).
