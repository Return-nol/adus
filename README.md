# Adus

Adus is a self-hosted AI code review platform. It reviews pull requests on **GitHub** and **Azure DevOps Services** with the LLM provider you choose (OpenAI, Anthropic, Gemini or any OpenAI-compatible endpoint), and answers `@adudu` commands in PR comments. Its bot is called `@adudu`. Adus is inspired by [Kodus](https://github.com/kodustech/kodus-ai) but is a rewrite, not a port.

One installation is one organization. It runs as three containers: `adus` (a single Rust binary with the dashboard embedded), Postgres 18 and Redis 8.

## Quick start

You need Docker with Compose 2.20 or newer (and `curl` on Linux/macOS). Then, from a clone of this repository:

```sh
./scripts/install.sh        # Linux, macOS, Git Bash
.\scripts\install.ps1       # Windows PowerShell 7
```

The installer creates `.env`, generates `ADUS_SECRET_KEY` and the database password, asks for your public URL, validates the configuration, starts the stack and waits until it is healthy. Run it again at any time: values already in `.env` are never overwritten.

It pulls the published `adus` image; if that pull fails, it builds the image from your checkout instead. To run the latest source rather than the latest release, always build it:

```sh
./scripts/install.sh --build
.\scripts\install.ps1 -Build
```

Then:

1. Open `<your URL>/setup` and create the first admin account.
2. Add an AI provider: enter an API key, test the connection, and pick its primary model (and optionally a cheaper fast model).
3. Choose the default provider in Settings.
4. Add a connection (a GitHub or Azure DevOps PAT) and pick the repositories to review.
5. Check that reviews can run: `docker compose exec adus adus doctor`.

Back up `.env`, especially `ADUS_SECRET_KEY`. If it changes, stored PATs and LLM keys can no longer be decrypted until the original key is restored.

## Configuration

Settings live in `.env` (copy of [`.env.example`](.env.example)).

| Variable | Default | Purpose |
|---|---|---|
| `COMPOSE_PROFILES` | `bundled` | `bundled` starts Postgres and Redis in Docker; add `https` for the Caddy proxy. Empty = use your own Postgres and Redis. |
| `ADUS_VERSION` | `latest` | Image tag (`ghcr.io/return-nol/adus`). |
| `ADUS_PORT` | `8000` | Host port for the dashboard and webhooks. |
| `ADUS_PUBLIC_URL` | (required) | Address that browsers and GitHub/Azure use to reach Adus. Webhooks are registered with it. An `https` URL makes the session cookie `Secure`. |
| `ADUS_SECRET_KEY` | (required) | base64 of 32 bytes (`adus gen-secret`). Encrypts PATs and LLM keys. |
| `POSTGRES_PASSWORD` | (required with `bundled`) | Password of the bundled Postgres. |
| `DATABASE_URL` | bundled Postgres | External Postgres only. |
| `REDIS_URL` | bundled Redis | External Redis only. |
| `ADUS_DOMAIN` | `localhost` | Domain served by the `https` profile. |
| `ADUS_WORKERS` | `2` | Review workers inside the process. |
| `RUST_LOG` | `info` | Log level. |
| `ADUS_TRACE` | (off) | `true` logs every LLM prompt and reply of a review, source code included. For debugging only. |

To use an external database or Redis, set `COMPOSE_PROFILES=` (empty), fill `DATABASE_URL` and `REDIS_URL` in `.env`, and run `docker compose up -d`. Run `docker compose run --rm --no-deps adus check-config` to validate the variables.

## HTTPS / public URL

GitHub and Azure DevOps must be able to reach `ADUS_PUBLIC_URL` to deliver webhooks, so `http://localhost:8000` only works for looking around.

- **Your own domain:** point DNS at the host, open ports 80 and 443, then set in `.env`:

  ```
  COMPOSE_PROFILES=bundled,https
  ADUS_DOMAIN=adus.example.com
  ADUS_PUBLIC_URL=https://adus.example.com
  ```

  and run `docker compose up -d`. Caddy gets and renews the certificate automatically.
- **Local testing:** run a tunnel, for example `cloudflared tunnel --url http://localhost:8000`, put the printed `https://...` address in `ADUS_PUBLIC_URL`, and run `docker compose up -d`.
- **Another reverse proxy:** forward to `localhost:${ADUS_PORT}` and set `ADUS_PUBLIC_URL` to the public address.

With the `https` profile, `ADUS_PORT` is still published on all interfaces, so Adus stays reachable over plain HTTP. When Caddy is in front, bind it to localhost with a `docker-compose.override.yml` (`services: adus: ports: !override ["127.0.0.1:8000:8000"]`) or block the port with a firewall.

After changing `ADUS_PUBLIC_URL`, open the connection in the dashboard and use "Resync webhooks".

## Connecting GitHub / Azure DevOps

Create a personal access token (PAT) and add it on the Repositories page. Minimum scopes:

- **GitHub.** Fine-grained: Pull requests (read and write), Contents (read), Webhooks (read and write), Metadata (read). Classic: `repo`, `admin:repo_hook`.
- **Azure DevOps Services** (`https://dev.azure.com/{org}`): Code (read and write), Service Hooks (read, write and manage).

Rotating a PAT never removes repositories; use "Rotate PAT" on the connection.

## Using @adudu

Adus reviews a pull request when it is opened or updated. In a PR comment you can also write:

- `@adudu review` to request a new review.
- `@adudu remember: <text>` to add a note that is injected into every future review.

Commands are matched case-insensitively and only work for registered Adus users. Authors that are not registered are ignored silently. Azure DevOps matches the comment author by email. For GitHub, set your **GitHub login** on the Profile page.

## Operations

**Ports**

| Port | Service | When |
|---|---|---|
| `8000` (`ADUS_PORT`) | Adus dashboard, API, webhooks | always |
| `80`, `443` | Caddy | profile `https` |

Postgres and Redis publish no host ports.

**Backup.** Back up the `pgdata` volume (required: all state lives there) and `.env`. The `redisdata` volume only holds queued jobs and is optional. For example, a Postgres dump:

```sh
docker compose exec -T postgres pg_dump -U adus adus > adus-backup.sql
```

**Update.** Set `ADUS_VERSION` in `.env` (or keep `latest`), then:

```sh
docker compose pull && docker compose up -d
```

To update to the latest source instead, pull the repository and rebuild: `git pull && ./scripts/install.sh --build` (Windows: `.\scripts\install.ps1 -Build`).

Migrations run automatically at start. Stopping waits up to 35 s for running reviews to finish.

**Upgrading.**

- Back up the database first (`pg_dump`, see Backup above). Migrations only go forward: once a newer version has migrated the database, rolling back to an older image fails at start. To go back, restore the backup taken before the upgrade.
- The bundled Postgres moved from 16 to 18, which cannot read 16's data files and refuses to start on them. With `COMPOSE_PROFILES=bundled`, dump the database with the old stack still running (`pg_dump`, see Backup above), then move the old files out of the way, start the new stack and restore the dump:

  ```sh
  docker compose stop postgres
  docker run --rm -v <project>_pgdata:/v alpine sh -c 'mkdir /v/pg16-old && find /v -mindepth 1 -maxdepth 1 ! -name pg16-old -exec mv {} /v/pg16-old/ \; && chown 70:70 /v'
  docker compose up -d postgres
  docker compose exec -T postgres psql -U adus adus < adus-backup.sql
  docker compose up -d
  ```

  `<project>` is the compose project name (by default the folder name; see `docker volume ls`). Once the restore works, `pg16-old` can be deleted. An external `DATABASE_URL` is not affected.
- Adus now skips files on a default ignore list (the Kodus defaults) before review. It covers many file types, including `**/*.json`, `**/*.xml` and `**/doc/**`. After upgrading, review it under Settings → Ignored files and remove anything you want reviewed.

**Troubleshooting.** Start with the doctor. It is read-only and tells you whether reviews can run, and if not, what to fix:

```sh
docker compose exec adus adus doctor      # or: docker compose run --rm adus doctor
docker compose logs -f adus
```

- `adus check-config` lists configuration mistakes.
- If you delete `.env` and the installer generates a new `POSTGRES_PASSWORD`, the existing `pgdata` volume still has the old one. Restore `.env`, or start over with `docker compose down -v` (this deletes all data).
- After changing `ADUS_SECRET_KEY`, re-enter every PAT ("Rotate PAT") and LLM key; the old ones can no longer be decrypted.
- The same checks are shown in the dashboard on the System status page (admins).

## Development

```sh
docker compose -f docker-compose.dev.yml up -d   # Postgres 18 (host port 55432) + Redis 8 (host port 55379)
cargo test --workspace                            # needs the dev Postgres and Redis; env comes from .cargo/config.toml
pnpm -C web install && pnpm -C web build          # the server embeds web/build
ADUS_PUBLIC_URL=http://localhost:8000 cargo run -p adus   # http://localhost:8000
pnpm -C web dev                                   # dashboard with hot reload on :5173, proxies /api to :8000
```

**Tracing a review.** `cargo run -p adus -- serve --trace` (or `ADUS_TRACE=true`) logs every LLM call a review makes: prompts, model reasoning, replies, tool calls and tool results, with secrets redacted but source code included, so keep it to development. Colours are dropped when the output is redirected, e.g. `cargo run -p adus -- serve --trace > review.log 2>&1`. To repeat a review of a pull request without pushing to it, run `cargo run -p adus -- rereview <repository_id> <pr_number>` in another terminal (`repository_id` appears in the review logs); Adus findings already posted on the PR are not posted again.

The tests and the CI need Postgres and Redis, which the dev compose file provides. On Windows, `cargo test --workspace` can run out of paging file; use `cargo test --workspace -j 2`.

To build the production image yourself: `docker compose build` (or `docker build -t adus .`).

### Prerequisites

- Rust stable (via [rustup](https://rustup.rs); on Windows the MSVC toolchain + Visual Studio Build Tools)
- Node.js 22+ and pnpm (dashboard in `web/`)
- Docker with Compose v2 (Postgres + Redis for dev and tests)

### Continuing development with Claude Code

This repo is built task by task with Claude Code from a written spec and plan. Read [`AGENTS.md`](AGENTS.md) first: it has the current status, conventions and how to resume.

Required skill plugin:

- **superpowers** from the `claude-plugins-official` marketplace (`anthropics/claude-plugins-official`). Built with version 7.0.2. Install it in Claude Code with `/plugin`. The workflow uses these skills:
  - `superpowers:subagent-driven-development` runs the plan. One implementer subagent works on each task, a reviewer subagent checks it, and the ledger in `.superpowers/sdd/` tracks progress.
  - `superpowers:writing-plans` and `superpowers:brainstorming` are for changing the spec or plan, or adding features.
  - `superpowers:using-git-worktrees` and `superpowers:finishing-a-development-branch` handle branch setup and finishing.

To resume, ask Claude Code to continue Adus from the next unfinished task in `docs/superpowers/plans/2026-11-19-adus-v1.md`.

No other plugins are required. Avoid browser-automation tools for checking the dashboard. One hung for more than 13 minutes in this project, so verify with `curl` and test by hand in a browser.

## License

[AGPL-3.0](LICENSE).
