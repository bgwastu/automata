//! Values under `encrypted_env`: base64 of an age file encrypted with `AUTOMATA_PASSWORD`.
//!
//! Any age tool can make and read them: `echo -n value | age -p | base64` and `base64 -d | age -d`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use age::secrecy::SecretString;
use anyhow::{Context, Result, bail};
use base64::Engine;

pub const VAR: &str = "AUTOMATA_PASSWORD";

static PASSWORD: OnceLock<Option<String>> = OnceLock::new();
/// The infra repo is read every 30 seconds and each scrypt decryption takes about a second.
static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn password() -> Result<SecretString> {
    let p = PASSWORD.get_or_init(|| {
        // `automata encrypt` on the host, run outside the service.
        let file = || {
            let text = std::fs::read_to_string("/etc/automata/env").ok()?;
            text.lines().find_map(|l| l.strip_prefix(VAR)?.strip_prefix('=').map(str::to_string))
        };
        std::env::var(VAR).ok().or_else(file).filter(|p| !p.is_empty())
    });
    Ok(p.clone().with_context(|| format!("set {VAR} in /etc/automata/env to read encrypted_env"))?.into())
}

pub fn decrypt(value: &str) -> Result<String> {
    let value: String = value.split_whitespace().collect();
    if let Some(v) = CACHE.lock().unwrap().get_or_insert_default().get(&value) {
        return Ok(v.clone());
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(&value).context("not base64")?;
    let identity = age::scrypt::Identity::new(password()?);
    let plain = match age::decrypt(&identity, &bytes) {
        Ok(p) => p,
        Err(age::DecryptError::DecryptionFailed | age::DecryptError::NoMatchingKeys) => {
            bail!("{VAR} is not the password it was encrypted with")
        }
        Err(e) => bail!("not an age file encrypted with a password: {e}"),
    };
    let plain = String::from_utf8(plain).context("not UTF-8")?;
    CACHE.lock().unwrap().get_or_insert_default().insert(value, plain.clone());
    Ok(plain)
}

pub fn encrypt(value: &str) -> Result<String> {
    let recipient = age::scrypt::Recipient::new(password()?);
    let bytes = age::encrypt(&recipient, value.as_bytes())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let _ = super::PASSWORD.set(Some("correct horse battery".into()));
        let enc = super::encrypt("s3cret $x").unwrap();
        assert_eq!(super::decrypt(&enc).unwrap(), "s3cret $x");
        let other = age::encrypt(&age::scrypt::Recipient::new("wrong".to_string().into()), b"x").unwrap();
        let other = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, other);
        assert!(super::decrypt(&other).unwrap_err().to_string().contains("not the password"));
    }
}
