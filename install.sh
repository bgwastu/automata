#!/bin/sh
# Install or upgrade Automata on a Linux host with Docker.
#   curl -fsSL https://raw.githubusercontent.com/bgwastu/automata/main/install.sh | sudo sh
set -eu

repo=${RELEASE_REPO:-bgwastu/automata}
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
command -v docker >/dev/null || { echo "install Docker first: https://docs.docker.com/engine/install/" >&2; exit 1; }

name=automata-$arch-linux
url=https://github.com/$repo/releases/latest/download/$name
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$name" "$url"
curl -fsSL -o "$tmp/$name.sha256" "$url.sha256"
(cd "$tmp" && sha256sum -c "$name.sha256" >/dev/null)
install -m 755 "$tmp/$name" /usr/local/bin/automata

mkdir -p /etc/automata
if [ ! -f /etc/automata/env ]; then
  umask 077
  cat > /etc/automata/env <<'ENV'
# Only what is needed to read the infra repo. Every other setting lives in the repo.
# Infra repo with automata.yml and apps/<name>/. Append #branch for a branch other than main.
INFRA_REPO=owner/infra
# Fine-grained token with read access to the infra repo and app repos.
GITHUB_TOKEN=
# Decrypts every encrypted_env in the infra repo.
AUTOMATA_PASSWORD=
ENV
  fresh=1
fi

cat > /etc/systemd/system/automata.service <<'UNIT'
[Unit]
Description=Automata
After=docker.service network-online.target
Wants=docker.service network-online.target

[Service]
EnvironmentFile=/etc/automata/env
ExecStart=/usr/local/bin/automata
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

# cloudflared reaches the dashboard through the automata network's gateway.
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  port=$(sed -n 's/^PORT=//p' /etc/automata/env)
  ufw allow in on automata0 to any port "${port:-8080}" proto tcp >/dev/null
fi

systemctl daemon-reload
if [ "${fresh:-}" = 1 ]; then
  systemctl enable automata >/dev/null
  echo "installed $(/usr/local/bin/automata --version). Fill in /etc/automata/env, then: systemctl start automata"
else
  systemctl restart automata
  echo "upgraded to $(/usr/local/bin/automata --version)"
fi
