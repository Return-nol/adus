# Adus

Adus is a self-hosted AI code review platform: it reviews pull requests on Azure DevOps and GitHub with the LLM provider you choose, and replies to `@adudu` commands. Licensed under AGPL-3.0.

## Development

```sh
docker compose -f docker-compose.dev.yml up -d   # postgres 16 + redis 7
cp .env.example .env                              # fill ADUS_SECRET_KEY
cargo test --workspace
cargo run -p adus                                 # needs the variables from .env exported
curl localhost:8080/healthz                       # -> ok
```
