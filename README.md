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

With Docker:

```sh
docker compose up -d --build
docker compose logs bot   # first start prints the generated root password
```

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

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```
