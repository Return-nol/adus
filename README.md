# Adus

Adus is a self-hosted AI code review platform: it reviews pull requests on Azure DevOps and GitHub with the LLM provider you choose, and replies to `@adudu` commands. Licensed under AGPL-3.0.

## Development

```sh
docker compose -f docker-compose.dev.yml up -d   # postgres 16 (host port 55432) + redis 7 (host port 55379)
cp .env.example .env                              # fill ADUS_SECRET_KEY
cargo test --workspace
cargo run -p adus                                 # needs the variables from .env exported
curl localhost:8080/healthz                       # -> ok
```

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
