# automata

A small self-hosted PaaS. Automata deploys every app in an infra repo as a Docker Compose project, routes each one to its own domain through a Cloudflare Tunnel, and backs up volumes to R2.

The infra repo is the only way to change what runs. The dashboard shows state and logs, and can restart, back up and restore.

## Install

On a Linux host with Docker:

```sh
curl -fsSL https://raw.githubusercontent.com/bgwastu/automata/main/install.sh | sudo sh
sudoedit /etc/automata/env   # INFRA_REPO, GITHUB_TOKEN, DOTENV_PRIVATE_KEY
sudo systemctl start automata
```

## Infra repo

```
automata.yml
.env                 # CLOUDFLARE_API_TOKEN, optional PASSWORD
apps/
  blog/
    automata.yml
    compose.yaml     # or a Dockerfile, or source: in automata.yml
    .env             # optional, passed to the app
```

`automata.yml`:

```yaml
domain: example.com            # apps default to <name>.example.com
dashboard: deploy.example.com  # optional
backup:                        # optional
  bucket: backups              # R2 bucket, created if missing
  keep: { daily: 7, weekly: 4, monthly: 6 }
```

`apps/<name>/automata.yml`, every key optional:

```yaml
source: { github: owner/blog, branch: main }  # default: this folder
domains: [blog.example.com]
service: web    # gets the domains and .env, default the first service
port: 3000
backup: true    # or { schedule: "0 */6 * * *", service: db, before: "pg_dump -U app app > /backup/db.sql" }
```

The Cloudflare token needs Cloudflare Tunnel Edit, Workers R2 Storage Edit, DNS Edit and Zone Read.

## Secrets

Every `.env` is encrypted with [dotenvx](https://dotenvx.com) and committed. Run `dotenvx encrypt` at the root, and put `DOTENV_PRIVATE_KEY` from `.env.keys` on the host. To encrypt an app's `.env` with the same key, start it with the root `DOTENV_PUBLIC_KEY` line and run `dotenvx encrypt -f apps/<name>/.env`.

Automata derives the R2 credentials from the Cloudflare token and the backup password from `DOTENV_PRIVATE_KEY`. Keep a copy of the key: restoring backups on a new host needs it.

## Behavior

- Automata checks the repos every 30 seconds. Press `s` on the dashboard to check now.
- A deploy that restarts, turns unhealthy or exits within 10 seconds is rolled back. To roll back by hand, revert the commit or pin `source.commit`.
- Images tagged `:latest` are pulled hourly, and a bad new image is rolled back.
- Backups run nightly with restic at low priority. Restoring stops the app and replaces its volumes.
- Deleting an app's folder stops it and keeps its volumes.
- Automata updates itself from GitHub releases. Set `self_update: false` to turn this off.

## Develop

```sh
INFRA_REPO=./infra DATA_DIR=/tmp/automata cargo run
```

Push a `v*` tag matching `Cargo.toml` to release.
