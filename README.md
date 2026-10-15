# Scorpio Discord bot

Discord bot with a web dashboard. Every module can be switched on or off per server, configured and monitored from the dashboard.

Modules:

- **Greeting**: posts a customizable welcome message to a chosen channel when someone joins the server.

## Discord setup

1. Create an application at https://discord.com/developers/applications and add a bot.
2. On the **Bot** tab, enable **Server Members Intent** (required to see members joining).
3. Copy the bot token into `.env` as `DISCORD_TOKEN`.
4. Invite the bot with **OAuth2 > URL Generator**: scope `bot`, permissions `View Channels`, `Send Messages`, `Embed Links`.

## Running

```sh
cp .env.example .env   # then fill in DISCORD_TOKEN
```

With Docker, using the prebuilt image from Docker Hub (`scorpioorganization/scorpio-discord-bot`, `linux/amd64` and `linux/arm64`). Only `docker-compose.yml` and `.env` are needed on the host, no source checkout:

```sh
mkdir -p data
docker compose pull
docker compose up -d
docker compose logs bot   # first start prints the generated root password
```

Update later with `docker compose pull && docker compose up -d`. Tags are `latest` and `sha-<commit>` for pinning a specific build.

With Docker, building from source:

```sh
mkdir -p data
docker compose up -d --build
```

The database lives in `./data`, a bind mounted directory. Create it before the first start, otherwise Docker refuses to start the container (it would create it owned by root). The container runs as uid/gid 1000 so the files belong to you; set `PUID` and `PGID` in `.env` if `id -u` / `id -g` differ.

The compose file publishes the dashboard on `127.0.0.1:8080` only. Put a reverse proxy with HTTPS in front of it if it should be reachable from other machines, and set `COOKIE_SECURE=true` (and `TRUST_PROXY=true` if the proxy sets `X-Forwarded-For`).

Without Docker:

```sh
cargo run --release
```

Open http://localhost:8080 and sign in as `root`. Without `DISCORD_TOKEN` the dashboard still runs and shows the bot as not configured.

### Configuration

| Variable | Default | Meaning |
|---|---|---|
| `DISCORD_TOKEN` | empty | Bot token. Empty means dashboard only. |
| `DATABASE_URL` | `sqlite://data/scorpio.db?mode=rwc` | SQLite database file. |
| `WEB_BIND` | `0.0.0.0:8080` | Dashboard listen address. |
| `COOKIE_SECURE` | `false` | Mark the session cookie Secure. Turn on behind HTTPS. |
| `TRUST_PROXY` | `false` | Use the last `X-Forwarded-For` entry as client IP for login throttling. |
| `ROOT_PASSWORD` | empty | Initial root password. Only used when the root account is first created. |
| `RUST_LOG` | `info` | Log filter. |

### Accounts

- `root` is created on first start. If `ROOT_PASSWORD` is empty, a random password is printed to the log once.
- Root creates, deletes and resets passwords of other accounts on the **Users** page. No email is involved.
- Every account can view statistics and change bot settings. Only root manages accounts.
- Forgot the root password? Run `scorpio-discord-bot reset-root` (Docker: `docker compose run --rm bot reset-root`). It prints a new password and signs root out everywhere.

### Troubleshooting

- **`Disallowed intent(s)` / close code 4014, or a "Server Members Intent is disabled" warning**: the
  Greeting module needs the privileged Server Members Intent. Enable it on the **Bot** tab of your
  application in the developer portal, then `docker compose restart bot`. Until then the bot still
  connects, but without that intent, so members joining are not seen and no greetings are sent. The
  dashboard shows the same warning on the overview, server and Greeting pages.
- **`DISCORD_TOKEN was rejected by Discord`**: the token is wrong or was reset. Copy a new one from
  the **Bot** tab into `.env` and restart. The dashboard keeps running and shows the error.

## Greeting module

Configured per server under **Overview > server > Greeting**:

- channel, message template, plain text or embed (with color), whether to greet bots
- live preview while editing and a **Send test message** button
- statistics: all time sent and failed counts, last 24 hours and 7 days, last error, recent activity

Placeholders:

| Placeholder | Becomes |
|---|---|
| `{user}` | mention of the new member (pings them) |
| `{display_name}` | server nickname, or global display name |
| `{username}` | Discord username |
| `{user_id}` | Discord user ID |
| `{server}` | server name |
| `{member_count}` | member count after the join |

Only the joining member can ever be pinged. `@everyone`, `@here` and role mentions are blocked even if they appear in the template or in someone's name.

## Security notes

- Passwords hashed with Argon2id. Sessions are random 256 bit tokens, only their SHA-256 is stored, cookie is `HttpOnly` and `SameSite=Lax`.
- Every state changing request requires a per session CSRF token.
- Login is throttled per IP (5 failures in 15 minutes) and per username (20 failures).
- Strict Content Security Policy, no inline scripts or styles, frame embedding denied.

## Adding a module

1. Create `src/modules/<name>/` and implement the `Module` trait from `src/modules/mod.rs` (id, name, description, summary stats, dashboard routes, event hooks).
2. Register it in `modules::registry()`.
3. Add its tables in a new file under `migrations/`.

The guild page lists the module with an enable/disable switch automatically, and the bot only calls its hooks in servers where it is enabled.

## Development

Requires Rust 1.94 or newer (`rust-version` in `Cargo.toml`).

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check   # needs cargo-deny, see deny.toml
```

Tests run without Discord or a database file: dashboard tests drive the real router against an in-memory SQLite database (`src/test_util.rs`, `src/web/tests.rs`).

### CI

GitHub Actions (`.github/workflows/ci.yml`) runs on every push to `master`, on pull requests and weekly:

- `rustfmt`, `clippy` (warnings are errors) and `cargo test` on stable
- `msrv`: `cargo check` with the exact `rust-version` from `Cargo.toml`
- `cargo-deny`: RustSec advisories, license allowlist, banned crates and sources. A new advisory does not fail pull requests, only the weekly run.
- `docker`: builds the image, starts it without a token and checks `/healthz` and a root login
- `publish`: only on pushes to `master` (or a manual run on `master`) and only after every job above passed. Builds `linux/amd64` and `linux/arm64` on native runners and pushes `scorpioorganization/scorpio-discord-bot` to Docker Hub tagged `latest` and `sha-<commit>`. Needs the repository secret `DOCKER_HUB` (a Docker Hub access token with write access); the login name is `DOCKERHUB_USERNAME` in the workflow `env`.
