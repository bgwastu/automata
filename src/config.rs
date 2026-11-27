//! The infra repo: a root `automata.yml` and one folder per app under `apps/`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::sched;

/// What the host needs to read the infra repo, set in `/etc/automata/env`. Everything else lives in the repo.
pub struct Env {
    pub data: PathBuf,
    pub infra: String,
    pub github_token: Option<String>,
    pub port: u16,
    pub release_repo: String,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

impl Env {
    pub fn load() -> Result<Env> {
        Ok(Env {
            data: var("DATA_DIR").unwrap_or_else(|| "/var/lib/automata".into()).into(),
            infra: var("INFRA_REPO").context("INFRA_REPO is required: owner/repo[#branch], or a local path")?,
            github_token: var("GITHUB_TOKEN"),
            port: var("PORT").map(|p| p.parse()).transpose().context("PORT")?.unwrap_or(8080),
            release_repo: var("RELEASE_REPO").unwrap_or_else(|| "bgwastu/automata".into()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Automata's own hostname: the dashboard at `/`, and apps that set `path`.
    pub domain: Option<String>,
    pub tunnel: String,
    pub backup: Option<BackupRepo>,
    /// When to look for new `:latest` images.
    pub image_check: String,
    pub self_update: bool,
    /// Defaults to the only account the token can see.
    pub cloudflare_account: Option<String>,
    pub secrets: Secrets,
    pub apps: Vec<App>,
    /// Apps that could not be loaded, with the reason.
    pub invalid: BTreeMap<String, String>,
}

/// `encrypted_env` in the root automata.yml.
#[derive(Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    #[serde(rename = "CLOUDFLARE_API_TOKEN")]
    pub cloudflare_token: Option<String>,
    #[serde(rename = "DASHBOARD_PASSWORD")]
    pub password: Option<String>,
}

/// An app's environment, with `encrypted_env` decrypted. Never printed.
#[derive(Clone, Default, PartialEq)]
pub struct Vars(pub BTreeMap<String, String>);

impl std::fmt::Debug for Vars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Vars({:?})", self.0.keys().collect::<Vec<_>>())
    }
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(..)")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackupRepo {
    /// R2 bucket in the Cloudflare account, created on the first backup.
    pub bucket: String,
    pub schedule: String,
    pub keep: Keep,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keep {
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
}

impl Default for Keep {
    fn default() -> Self {
        Keep { daily: 7, weekly: 4, monthly: 6 }
    }
}

#[derive(Debug, Clone)]
pub struct App {
    pub name: String,
    /// The app's folder inside the infra checkout.
    pub dir: PathBuf,
    pub source: Option<Source>,
    /// Run this image instead of a compose file or Dockerfile in the folder.
    pub image: Option<String>,
    /// Volumes for `image`, like `data:/app/data`.
    pub volumes: Vec<String>,
    /// A path under Automata's domain, like `/blog`.
    pub path: Option<String>,
    /// Hostnames routed through the tunnel. Their DNS records are up to the user.
    pub domains: Vec<String>,
    pub service: Option<String>,
    pub port: u16,
    /// Environment for the main service.
    pub env: Vars,
    pub backup: Option<AppBackup>,
    /// Hash of every file in the app's infra folder. A change redeploys the app.
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub github: String,
    pub branch: String,
    /// Pin to this commit instead of following the branch.
    pub commit: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppBackup {
    pub schedule: String,
    pub before: Option<String>,
    pub service: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRoot {
    domain: Option<String>,
    tunnel: Option<String>,
    backup: Option<RawRepo>,
    image_check: Option<String>,
    self_update: Option<bool>,
    cloudflare_account: Option<String>,
    #[serde(default)]
    encrypted_env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepo {
    bucket: String,
    schedule: Option<String>,
    #[serde(default)]
    keep: Keep,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawApp {
    source: Option<RawSource>,
    image: Option<String>,
    volumes: Option<Vec<String>>,
    path: Option<String>,
    domains: Option<Vec<String>>,
    service: Option<String>,
    port: Option<u16>,
    #[serde(default)]
    env: BTreeMap<String, serde_yaml_ng::Value>,
    #[serde(default)]
    encrypted_env: BTreeMap<String, String>,
    backup: Option<RawAppBackup>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    github: String,
    branch: Option<String>,
    commit: Option<String>,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawAppBackup {
    On(bool),
    Options(RawBackupOptions),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackupOptions {
    schedule: Option<String>,
    before: Option<String>,
    service: Option<String>,
}

const DEFAULT_BACKUP: &str = "0 3 * * *";

pub fn load(root: &Path) -> Result<Config> {
    let raw: RawRoot = yaml(&root.join("automata.yml"))?.unwrap_or_default();
    let image_check = raw.image_check.unwrap_or_else(|| "0 * * * *".into());
    sched::parse(&image_check).context("image_check")?;
    let secrets: Secrets = serde_json::from_value(serde_json::to_value(decrypt_all(&raw.encrypted_env)?)?)
        .context("encrypted_env takes CLOUDFLARE_API_TOKEN and DASHBOARD_PASSWORD")?;
    let backup = raw
        .backup
        .map(|b| -> Result<_> {
            let schedule = b.schedule.unwrap_or_else(|| DEFAULT_BACKUP.into());
            sched::parse(&schedule).context("backup.schedule")?;
            let n = &b.bucket;
            if n.len() < 3
                || n.len() > 63
                || !n.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            {
                bail!("backup.bucket must be 3 to 63 lowercase letters, digits and dashes");
            }
            if b.keep.daily == 0 {
                bail!("backup.keep.daily must be at least 1");
            }
            Ok(BackupRepo { bucket: b.bucket, schedule, keep: b.keep })
        })
        .transpose()?;

    let mut config = Config {
        domain: raw.domain,
        tunnel: raw.tunnel.unwrap_or_else(|| "automata".into()),
        backup,
        image_check,
        self_update: raw.self_update.unwrap_or(true),
        cloudflare_account: raw.cloudflare_account,
        secrets,
        apps: vec![],
        invalid: BTreeMap::new(),
    };

    let mut names: Vec<String> = match std::fs::read_dir(root.join("apps")) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => vec![],
    };
    names.sort();

    let mut owners: HashMap<String, String> = HashMap::new();
    if let Some(d) = &config.domain {
        owners.insert(d.clone(), "the dashboard".into());
    }
    for name in names {
        let app = load_app(&root.join("apps").join(&name), &name, &config).and_then(|app| {
            for d in app.domains.iter().chain(&app.path) {
                if let Some(owner) = owners.get(d) {
                    bail!("{d} is also used by {owner}");
                }
            }
            Ok(app)
        });
        match app {
            Ok(app) => {
                for d in app.domains.iter().chain(&app.path) {
                    owners.insert(d.clone(), app.name.clone());
                }
                config.apps.push(app);
            }
            Err(e) => {
                config.invalid.insert(name, format!("{e:#}"));
            }
        }
    }
    Ok(config)
}

fn load_app(dir: &Path, name: &str, config: &Config) -> Result<App> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-');
    if !valid {
        bail!("name must be lowercase letters, digits and dashes");
    }
    if name == "automata" {
        bail!("the name automata is reserved");
    }
    let raw: RawApp = yaml(&dir.join("automata.yml"))?.unwrap_or_default();
    let fallback = config.backup.as_ref().map_or(DEFAULT_BACKUP, |b| &b.schedule).to_string();
    let backup = match raw.backup {
        None | Some(RawAppBackup::On(false)) => None,
        Some(RawAppBackup::On(true)) => Some(AppBackup { schedule: fallback, before: None, service: None }),
        Some(RawAppBackup::Options(o)) => {
            Some(AppBackup { schedule: o.schedule.unwrap_or(fallback), before: o.before, service: o.service })
        }
    };
    if let Some(b) = &backup {
        if config.backup.is_none() {
            bail!("backup is on but the root automata.yml has no backup.bucket");
        }
        sched::parse(&b.schedule).context("backup.schedule")?;
    }
    let backup = backup.map(|b| AppBackup { service: b.service.or_else(|| raw.service.clone()), ..b });
    if backup.as_ref().is_some_and(|b| b.before.is_some() && b.service.is_none()) {
        bail!("backup.before needs backup.service or service");
    }
    let source = raw.source.map(|s| Source {
        github: s.github,
        branch: s.branch.unwrap_or_else(|| "main".into()),
        commit: s.commit,
        path: s.path.unwrap_or_else(|| ".".into()),
    });
    if let Some(c) = source.as_ref().and_then(|s| s.commit.as_ref())
        && (c.len() != 40 || !c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("source.commit must be a full 40-character sha");
    }
    if let Some(image) = &raw.image {
        if image.trim().is_empty() || image.contains(char::is_whitespace) {
            bail!("image must be an image reference like ghcr.io/you/app:latest");
        }
        if source.is_some() {
            bail!("use either image or source, not both");
        }
        let files = ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml", "Dockerfile"];
        if let Some(f) = files.iter().find(|f| dir.join(f).exists()) {
            bail!("image is set, so remove {f} or drop image");
        }
    }
    let volumes = raw.volumes.unwrap_or_default();
    if !volumes.is_empty() && raw.image.is_none() {
        bail!("volumes only applies with image; put them in the compose file");
    }
    for v in &volumes {
        let mut parts = v.split(':');
        let (Some(from), Some(to)) = (parts.next(), parts.next()) else {
            bail!("volume {v:?} must look like name:/path/in/container");
        };
        if from.is_empty() || !to.starts_with('/') || parts.next().is_some_and(|o| o != "ro" && o != "rw") {
            bail!("volume {v:?} must look like name:/path/in/container");
        }
    }
    if let Some(p) = &raw.path {
        check_path(p)?;
        if config.domain.is_none() {
            bail!("path needs domain in the root automata.yml");
        }
    }
    let mut env = decrypt_all(&raw.encrypted_env)?;
    for (k, v) in raw.env {
        let v = match v {
            serde_yaml_ng::Value::String(s) => s,
            serde_yaml_ng::Value::Bool(b) => b.to_string(),
            serde_yaml_ng::Value::Number(n) => n.to_string(),
            _ => bail!("env.{k} must be a string, number or boolean"),
        };
        if env.insert(k.clone(), v).is_some() {
            bail!("{k} is in both env and encrypted_env");
        }
    }
    Ok(App {
        name: name.into(),
        dir: dir.into(),
        source,
        image: raw.image,
        volumes,
        path: raw.path,
        domains: raw.domains.unwrap_or_default(),
        service: raw.service,
        port: raw.port.unwrap_or(3000),
        env: Vars(env),
        backup,
        hash: hash_dir(dir)?,
    })
}

fn decrypt_all(vars: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    vars.iter()
        .map(|(k, v)| Ok((k.clone(), crate::secret::decrypt(v).with_context(|| format!("encrypted_env.{k}"))?)))
        .collect()
}

/// Paths are `/` and one or more segments, and must not shadow the dashboard's own routes.
fn check_path(path: &str) -> Result<()> {
    let segments: Vec<&str> = path.strip_prefix('/').unwrap_or("").split('/').collect();
    let ok = path.starts_with('/')
        && segments.iter().all(|s| {
            !s.is_empty()
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
                && *s != "."
                && *s != ".."
        });
    if !ok {
        bail!("path must look like /blog: letters, digits, - _ . ~ and no trailing slash");
    }
    if ["api", "app.js", "style.css"].contains(&segments[0]) {
        bail!("path {path} is used by the dashboard");
    }
    Ok(())
}

fn hash_dir(dir: &Path) -> Result<String> {
    let mut h = Sha256::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry?;
        if entry.file_type().is_file() {
            h.update(entry.path().strip_prefix(dir)?.to_string_lossy().as_bytes());
            h.update([0]);
            h.update(std::fs::read(entry.path())?);
        }
    }
    Ok(hex::encode(h.finalize()))
}

fn yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| path.display().to_string()),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let shown = path.components().rev().take(2).collect::<Vec<_>>();
    let shown: PathBuf = shown.into_iter().rev().collect();
    serde_yaml_ng::from_str(&text).map(Some).with_context(|| shown.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::{check_path, load};

    fn infra(files: &[(&str, &str)]) -> (tempdir::Dir, super::Config) {
        let dir = tempdir::Dir::new();
        for (path, text) in files {
            let p = dir.0.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let config = load(&dir.0).unwrap();
        (dir, config)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!("automata-test-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn image_apps() {
        let (_d, c) = infra(&[("apps/web/automata.yml", "image: ghcr.io/a/b:latest\nvolumes: [data:/app/data]\n")]);
        let app = &c.apps[0];
        assert_eq!(app.image.as_deref(), Some("ghcr.io/a/b:latest"));
        assert_eq!(app.volumes, ["data:/app/data"]);

        let bad = [
            ("image: a/b\nsource: {github: a/b}\n", None),
            ("image: a/b\n", Some("compose.yaml")),
            ("image: a/b\n", Some("Dockerfile")),
            ("volumes: [data:/x]\n", Some("compose.yaml")),
            ("image: a/b\nvolumes: [data]\n", None),
            ("image: a/b\nvolumes: [data:relative]\n", None),
        ];
        for (yml, extra) in bad {
            let mut files = vec![("apps/web/automata.yml", yml)];
            if let Some(f) = extra {
                files.push((if f == "Dockerfile" { "apps/web/Dockerfile" } else { "apps/web/compose.yaml" }, "x"));
            }
            let (_d, c) = infra(&files);
            assert!(c.apps.is_empty() && c.invalid.contains_key("web"), "{yml} {extra:?}");
        }
    }

    #[test]
    fn paths() {
        for ok in ["/blog", "/a/b-c", "/v1.2", "/x_y~z"] {
            assert!(check_path(ok).is_ok(), "{ok}");
        }
        for bad in ["", "/", "blog", "/blog/", "/a//b", "/..", "/a b", "/api", "/api/x", "/app.js", "/style.css"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }
}
