# Scorpio Discord bot

Discord bot with a web dashboard. Every module can be switched on or off per server, configured and monitored from the dashboard.

Modules:

- **Greeting**: posts a customizable welcome message to a chosen channel when someone joins the server.
- **ClickUp**: stores the ClickUp API key other modules can share, matches ClickUp people to server members and maps ClickUp tags to server roles.
- **Calendar**: reads meetings and events from a ClickUp list and posts scheduled agendas and reminders before each event.

## Discord setup

1. Create an application at https://discord.com/developers/applications and add a bot.
2. On the **Bot** tab, enable **Server Members Intent** (required to see members joining, and to match ClickUp people to members).
3. Copy the bot token into `.env` as `DISCORD_TOKEN`.
4. Invite the bot with **OAuth2 > URL Generator**: scope `bot`, permissions `View Channels`, `Send Messages`, `Embed Links`, `Add Reactions`, `Read Message History` (the last two let the Calendar module add reactions). To let the Calendar module ping roles that are not mentionable, also grant `Mention Everyone` (shown as "Mention @everyone, @here, and All Roles"); the bot still never pings `@everyone` or `@here`.

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

The database lives in `./data`, a bind mounted directory, together with `secret.key` (see [ClickUp setup](#clickup-setup)). Create it before the first start, otherwise Docker refuses to start the container (it would create it owned by root). The container runs as uid/gid 1000 so the files belong to you; set `PUID` and `PGID` in `.env` if `id -u` / `id -g` differ.

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
| `SECRETS_KEY` | empty | 64 hex characters (`openssl rand -hex 32`) used to encrypt stored API keys. Empty means a key is generated once into `secret.key` next to the database; back it up with the database. |
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
- **"The saved ClickUp API key cannot be decrypted"**: `SECRETS_KEY` changed, or `secret.key` was lost
  or replaced. Save the API key again on the ClickUp (or Calendar) page.
- **The calendar posts nothing**: check, in this order, that the Calendar module is enabled, a ClickUp
  list is selected, the API key works (the key card shows the last error; with the shared key the
  ClickUp module must be enabled too), the rule is active and has a channel the bot can post in, and
  the events have the date the calendar uses (due date, or start date in start date mode). The
  **Events** table on the Calendar page lists what the bot reads (including the last 12 hours) and,
  for every reminder rule, when it reminds or why it did not; **Recent activity** shows every post.
- **A task is listed but never reminded**: give it a due date *with a time*. ClickUp stores a date
  without a time as 04:00 in the time zone of whoever set it, so for the bot that event already
  started early in the morning. The Events table marks such tasks with "no time set in ClickUp?".
  This works the same on every ClickUp plan, including Free Forever.

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

## ClickUp setup

The bot reads ClickUp with a personal API key: in ClickUp open your avatar, **Settings**, **Apps** and copy the **API Token** (it starts with `pk_`). The key acts as your ClickUp account, so the bot sees what you can see. It only ever reads; the ClickUp client in the bot has no way to create, change or delete anything.

When a key is saved it is first checked with ClickUp, then stored encrypted (XChaCha20-Poly1305). The dashboard never shows it again, only the ClickUp account name and the last four characters.

The encryption key comes from `SECRETS_KEY` (64 hex characters, `openssl rand -hex 32`). Without it, a random key is created once in `secret.key` next to the database (`./data/secret.key` with Docker). Back that file up together with the database. If `SECRETS_KEY` or `secret.key` changes, saved API keys can no longer be read and have to be entered again; nothing else is affected.

## ClickUp module

Configured per server under **Overview > server > ClickUp**:

- **API key**: the shared key other ClickUp modules use by default. Those modules can also keep a separate key of their own. Other modules can only use the shared key while this module is enabled.
- **Workspace**: the ClickUp workspace whose people are listed for matching.
- **People**: every person of the workspace with the server member they are matched to.
- **Tags to roles**: ClickUp tags (those of the workspace's spaces, or typed in) mapped to server roles. Modules use them for the `{roles}` placeholder. Tags are matched ignoring case.

People are matched to server members by name automatically. Accents, dots, underscores and word order are ignored (`Igor Zawórski` matches `igor_zaworski` and `Zaworski Igor`), the email address before the `@` counts too, and small typos are tolerated. A first name alone is never enough, and when two members fit equally well the person stays unmatched instead of guessing. Each member is matched to at most one person.

To pin a person, type a Discord username, display name, `@name`, user ID or `<@id>` into their **Pin to** field. `-` means never match this person, an empty field means automatic. Pinned members are never matched to anyone else automatically.

## Calendar module

Configured per server under **Overview > server > Calendar**. The bot reads one ClickUp list (the list behind your calendar or board) and never changes it.

**Source**: the list (picked from your workspaces, or pasted as an ID or list URL), whether an event happens at its due date or its start date (falling back to the due date), the default time zone (for new digest rules and the times shown on the page), an optional tag filter (tasks with any of up to 5 tags), and whether subtasks and closed tasks count. Tasks without the chosen date are ignored.

**Rules** (up to 20 per server), each with its own channel:

- **Digest**: on the chosen weekdays at a time of day in the rule's own time zone, posts the events of the next N hours, either as one message listing them or as one message per event. The optional header starts the listing message, or is posted as its own first message in the one message per event layout. Optionally posts a message when there are no events. A new digest rule takes the time zone of your browser (or the server default without JavaScript).
- **Reminder**: posts once for every event, a set number of minutes before it starts. If the event is moved, it is reminded again before the new time.

Every rule has a message template, a description length, an embed option with color, up to 5 reactions the bot adds to its messages (any emoji, or custom emojis of the server), whether matched assignees are pinged, and whether roles mapped from task tags are pinged.

Placeholders for each event:

| Placeholder | Becomes |
|---|---|
| `{title}` | task name |
| `{link}` | task name linking to the task in ClickUp |
| `{url}` | link to the task |
| `{when}`, `{date}`, `{time}` | date and time, date, time |
| `{relative}` | relative time, like "in 2 hours" |
| `{start}`, `{due}` | start and due date, when set |
| `{assignees}` | assignees, as member mentions when matched, else their ClickUp names |
| `{description}` | task description, cut to the configured length |
| `{status}`, `{tags}`, `{priority}`, `{list}` | task status, tags, priority and list name |
| `{roles}` | roles mapped from the task's tags (ClickUp page), as role mentions |

The digest header and the "no events" message know `{count}`, `{list}`, `{date}` and `{roles}` (the roles of all listed events). Times are Discord timestamps, so every reader sees them in their own time zone. A template line whose placeholders all come out empty is left out.

Behavior worth knowing:

- Matched assignees and mapped roles always appear as mentions, but they are only pinged when the rule says so. Only roles mapped from the event's tags can ping, and only if the role is mentionable or the bot has the `Mention Everyone` permission in the channel (the page warns otherwise). `@everyone` and `@here` never ping. Test messages never ping anyone.
- A digest that could not be posted within 2 hours of its time (bot offline, ClickUp down) is skipped and shown as missed, not posted late.
- If Discord refuses a message (missing permission, deleted channel) it is logged and counted, and not retried every minute. A reaction that cannot be added does not make the post count as failed.
- At most 10 reminders per rule are posted per minute, so a backlog cannot flood a channel.
- While a reminder rule exists the bot reads the list about once a minute, so new or moved tasks are noticed within a minute. A task created after its reminder time but before it starts is reminded right away; one that already started is not.
- Reminders need a due time. A ClickUp date without a time is stored as 04:00, see Troubleshooting.
- Tasks are read in pages; if a list has more than about 500 matching tasks in the time window, some can be missed and the page says so. A tag filter helps.
- The page shows the upcoming events of the next 7 days with matched members, each rule's next run, statistics (digests, reminders, failed posts, missed digests, ClickUp errors) and recent activity. The rule page shows a preview of what would be posted now and has a **Send test message** button.

## Security notes

- Passwords hashed with Argon2id. Sessions are random 256 bit tokens, only their SHA-256 is stored, cookie is `HttpOnly` and `SameSite=Lax`.
- Every state changing request requires a per session CSRF token.
- Login is throttled per IP (5 failures in 15 minutes) and per username (20 failures).
- Strict Content Security Policy, no inline scripts or styles, frame embedding denied.
- Third party API keys are encrypted at rest with a key that is not in the database, write-only in the dashboard and never logged.
- ClickUp access is read-only by construction: the client only implements GET requests.
- Messages restrict pings to the users and roles a module explicitly allows, never `@everyone` or `@here`.

## Adding a module

1. Create `src/modules/<name>/` and implement the `Module` trait from `src/modules/mod.rs` (id, name, description, summary stats, dashboard routes, event hooks).
2. Register it in `modules::registry()`.
3. Add its tables in a new file under `migrations/`.

The guild page lists the module with an enable/disable switch automatically, and the bot only calls its hooks in servers where it is enabled. Modules that work on a schedule return `true` from `ticks()` and get `on_tick` about once a minute per server.

A module that reads ClickUp reuses the shared pieces instead of handling keys itself: `clickup::web::token_routes("<id>")` and `templates/clickup/token_card.html` give it the API key card (shared or own key), `clickup::tokens::resolve` and `track` give it a key and record each call, `modules::clickup::resolver` matches ClickUp people to members and `modules::clickup::tag_roles` gives the tag to role mappings. The Calendar module is the example.

## Development

Requires Rust 1.94 or newer (`rust-version` in `Cargo.toml`).

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check   # needs cargo-deny, see deny.toml
```

Without cargo-deny installed, run it in Docker:

```sh
docker run --rm -v "$PWD":/src:ro -w /src rust:1 sh -c 'cargo install cargo-deny --locked && cargo deny check'
```

Tests run without Discord, ClickUp or a database file: dashboard tests drive the real router against an in-memory SQLite database (`src/test_util.rs`, `src/web/tests.rs`), a fake connected server stands in for Discord, and a small in-process server fakes the ClickUp API.

### CI

GitHub Actions (`.github/workflows/ci.yml`) runs on every push to `master`, on pull requests and weekly:

- `rustfmt`, `clippy` (warnings are errors) and `cargo test` on stable
- `msrv`: `cargo check` with the exact `rust-version` from `Cargo.toml`
- `cargo-deny`: RustSec advisories, license allowlist, banned crates and sources. A new advisory does not fail pull requests, only the weekly run.
- `docker`: builds the image, starts it without a token and checks `/healthz` and a root login
- `publish`: only on pushes to `master` (or a manual run on `master`) and only after every job above passed. Builds `linux/amd64` and `linux/arm64` on native runners and pushes `scorpioorganization/scorpio-discord-bot` to Docker Hub tagged `latest` and `sha-<commit>`. Needs the repository secret `DOCKER_HUB` (a Docker Hub access token with write access); the login name is `DOCKERHUB_USERNAME` in the workflow `env`.
