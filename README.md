# automata

Your own small Heroku, run from a GitHub repository.

You describe your apps in a repo. Automata runs them on your server, keeps them in sync with the repo, puts them online through Cloudflare, and backs them up. To change anything, you change the repo.

## Features

- **Runs whatever you have.** An image like `ghcr.io/you/app`, a Docker Compose file, a Dockerfile, or the code in another GitHub repo.
- **Deploys on push.** Commit to the repo and the server picks it up within 30 seconds. No CI or webhooks to set up.
- **Rolls back bad deploys.** If a new version crashes or turns unhealthy, the previous one keeps running.
- **Online without opening ports.** Apps go out through a Cloudflare Tunnel, at a path like `deploy.example.com/blog` or on their own domain. Websockets and streaming work.
- **Backups to Cloudflare R2.** Nightly, kept daily, weekly and monthly. They are plain `.tar.gz` files you can download and open anywhere.
- **Encrypted secrets in the repo.** Commit secrets safely next to the app that uses them.
- **Stays up to date.** It pulls new `:latest` images and updates itself.
- **A simple dashboard.** See status and logs, and restart, back up or restore an app.

## Install

On a Linux server with Docker:

```sh
curl -fsSL https://raw.githubusercontent.com/bgwastu/automata/main/install.sh | sudo sh
sudoedit /etc/automata/env
sudo systemctl start automata
```

## The repo

```
automata.yml        # settings for the whole server
apps/
  blog/
    automata.yml    # optional settings for this app
    compose.yaml    # or a Dockerfile, or image: in automata.yml
```

Every folder in `apps/` is an app. Add one to deploy it, delete it to stop it.

```yaml
# automata.yml
domain: deploy.example.com
backup:
  bucket: my-backups
encrypted_env:
  CLOUDFLARE_API_TOKEN: <encrypted token>
  DASHBOARD_PASSWORD: <encrypted password>  # optional
```

```yaml
# apps/blog/automata.yml
path: /blog         # or domains: [blog.example.com]
port: 3000
backup: true
env:
  NODE_ENV: production
encrypted_env:
  DATABASE_URL: <encrypted database URL>
```

An app that is just an image needs no other file:

```yaml
# apps/api/automata.yml
image: ghcr.io/you/api:latest
volumes: [data:/app/data]
domains: [api.example.com]
port: 8080
```

`image` cannot be combined with `source` or with a compose file or Dockerfile in the folder. A `:latest` image is pulled again when it changes, like any other in a compose file.

An app without `path` or `domains` is not put online, which suits background workers and databases. Automata strips the path before passing a request on, so `/blog/post` reaches the app as `/post`, and sets `X-Forwarded-Prefix: /blog`. For your own `domains`, point a CNAME at the `automata` tunnel in Cloudflare.

## Signing in to the dashboard

There are two ways to lock the dashboard. Use either or both.

**Cloudflare Access.** Add a self-hosted Access application for exactly the `domain` in `automata.yml`. Automata finds it with its Cloudflare token, along with your team domain, so there is nothing to configure. From then on every dashboard request needs a token Access signed for that application, and Automata checks the signature itself. That lets you keep Access to `/` only: add Bypass applications for the paths your apps use, and those stay public, while the dashboard API still refuses anyone Access did not sign in. Automata never passes the Access token on to path apps. If Automata cannot tell whether Access is on, it refuses every dashboard request until it can.

**A password.** With `DASHBOARD_PASSWORD` set, the dashboard asks for it on a sign-in page and keeps you signed in for 30 days. Changing the password signs everyone out. After five wrong passwords, an address has to wait 15 minutes.

With neither, the dashboard is open to anyone who can reach it, and it shows a warning.

## Cloudflare token

The token needs these permissions:

- Cloudflare Tunnel Edit
- Workers R2 Storage Edit
- DNS Edit
- Zone Read
- Access: Apps and Policies Read
- Access: Organizations, Identity Providers, and Groups Read

Automata only needs the two Access permissions to find an Access application.

## Secrets

Everything under `encrypted_env` is encrypted with one password, the `AUTOMATA_PASSWORD` you put in `/etc/automata/env`. Each value is a standard [age](https://age-encryption.org) file in base64, so you never need Automata to read your own secrets:

```sh
echo -n 'the value' | age -p | base64      # encrypt
echo 'YWdlLWVu...' | base64 -d | age -d    # decrypt
```

On the server, `echo -n 'the value' | automata encrypt` does the same with the password it already has.

## Develop

```sh
INFRA_REPO=./infra DATA_DIR=/tmp/automata cargo run
```

Push a `v*` tag matching `Cargo.toml` to release.
