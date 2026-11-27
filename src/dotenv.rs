//! `.env` files, with dotenvx-encrypted values decrypted using `DOTENV_PRIVATE_KEY`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use base64::Engine;

fn keys() -> Result<Vec<Vec<u8>>> {
    std::env::var("DOTENV_PRIVATE_KEY")
        .unwrap_or_default()
        .split(',')
        .filter(|k| !k.trim().is_empty())
        .map(|k| hex::decode(k.trim()).context("DOTENV_PRIVATE_KEY is not hex"))
        .collect()
}

pub fn read(path: &std::path::Path) -> Result<BTreeMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    parse(&text, &keys()?).context(".env")
}

/// A secret for `purpose` derived from the first `DOTENV_PRIVATE_KEY`, so the host key is the only one to keep.
pub fn derive(purpose: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let keys = keys()?;
    let key = keys.first().context("DOTENV_PRIVATE_KEY is not set")?;
    Ok(hex::encode(Sha256::new().chain_update(b"automata:").chain_update(purpose).chain_update(key).finalize()))
}

/// Parse dotenv syntax: `KEY=value`, `export KEY=value`, single quotes taken literally,
/// double quotes with escapes, `${VAR}` and `$VAR` expanded from earlier keys in the file.
pub fn parse(src: &str, keys: &[Vec<u8>]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut rest = src;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.starts_with('#') || rest.is_empty() {
            rest = rest.split_once('\n').map_or("", |(_, r)| r);
            continue;
        }
        rest = rest.strip_prefix("export ").unwrap_or(rest).trim_start();
        let end =
            rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')).unwrap_or(rest.len());
        let key = &rest[..end];
        let after = rest[end..].trim_start_matches([' ', '\t']);
        let Some(after) = after.strip_prefix('=').filter(|_| !key.is_empty()) else {
            bail!("expected KEY=value near \"{}\"", rest.lines().next().unwrap_or(""));
        };
        let after = after.trim_start_matches([' ', '\t']);
        let (raw, expand, remaining) = match after.chars().next() {
            Some(q @ ('\'' | '"' | '`')) => {
                let body = &after[1..];
                let close = find_close(body, q).with_context(|| format!("{key}: missing closing {q}"))?;
                let value = &body[..close];
                let tail = &body[close + 1..];
                let value = if q == '"' { unescape(value) } else { value.to_string() };
                (value, q == '"', tail.split_once('\n').map_or("", |(_, r)| r))
            }
            _ => {
                let (line, tail) = after.split_once('\n').unwrap_or((after, ""));
                let line = line.split(" #").next().unwrap_or("").trim();
                (line.to_string(), true, tail)
            }
        };
        rest = remaining;
        if key.starts_with("DOTENV_PUBLIC_KEY") {
            continue;
        }
        let value = if let Some(cipher) = raw.strip_prefix("encrypted:") {
            decrypt(key, cipher, keys)?
        } else if expand {
            expand_vars(&raw, &out)
        } else {
            raw
        };
        out.insert(key.to_string(), value);
    }
    Ok(out)
}

fn find_close(body: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            '\\' if quote == '"' && !escaped => escaped = true,
            c if c == quote && !escaped => return Some(i),
            _ => escaped = false,
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(c) => out.push(c),
            None => out.push('\\'),
        }
    }
    out
}

fn expand_vars(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(inner) = after.strip_prefix('{')
            && let Some(close) = inner.find('}')
        {
            let expr = &inner[..close];
            let (name, default) = expr.split_once(":-").map_or((expr, None), |(n, d)| (n, Some(d)));
            match vars.get(name).filter(|v| !v.is_empty()) {
                Some(v) => out.push_str(v),
                None => out.push_str(default.unwrap_or("")),
            }
            rest = &inner[close + 1..];
            continue;
        }
        let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
        if end == 0 {
            out.push('$');
        } else {
            out.push_str(vars.get(&after[..end]).map_or("", String::as_str));
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn decrypt(key: &str, cipher: &str, keys: &[Vec<u8>]) -> Result<String> {
    let bytes =
        base64::engine::general_purpose::STANDARD.decode(cipher).with_context(|| format!("{key}: bad ciphertext"))?;
    for k in keys {
        if let Ok(plain) = ecies::decrypt(k, &bytes) {
            return String::from_utf8(plain).with_context(|| format!("{key}: not UTF-8"));
        }
    }
    bail!("cannot decrypt {key}: it was encrypted with a key other than DOTENV_PRIVATE_KEY")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quotes_and_expansion() {
        let src =
            "# c\nexport A=1\nB=\"x ${A} \\n\"\nC='lit $A'\nD=plain # comment\nE=\"multi\nline\"\nF=${MISSING:-def}\n";
        let env = parse(src, &[]).unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "x 1 \n");
        assert_eq!(env["C"], "lit $A");
        assert_eq!(env["D"], "plain");
        assert_eq!(env["E"], "multi\nline");
        assert_eq!(env["F"], "def");
    }

    #[test]
    fn decrypts_ecies_values() {
        let (sk, pk) = ecies::utils::generate_keypair();
        let cipher = ecies::encrypt(&pk.serialize(), b"s3cret").unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(cipher);
        let env =
            parse(&format!("DOTENV_PUBLIC_KEY=\"abc\"\nX=\"encrypted:{b64}\"\n"), &[sk.serialize().to_vec()]).unwrap();
        assert_eq!(env["X"], "s3cret");
        assert!(!env.contains_key("DOTENV_PUBLIC_KEY"));
    }
}
