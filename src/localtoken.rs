//! Cybercore local-token/v1: lets a client on this machine (Nightwatch's
//! gateway, `nightwatchd`) call this service without a browser page.
//!
//! At startup the service writes a fresh random token to
//! `$RUNTIME_DIRECTORY/local-token` (systemd `RuntimeDirectory=`), or else
//! `$XDG_RUNTIME_DIR/<service>/local-token`: directory 0700, file 0600, so
//! only the service's own user can read it. A client sends it as
//! `Authorization: Bearer <token>`. Browsers never attach that header on
//! their own, so a bearer request needs no Origin/CSRF check; the Host check
//! still applies. `X-Cybercore-Actor` says who asked ("nightwatch:Pixel")
//! and is trusted only on a request with a valid bearer token.
//!
//! The same file is copied into every service that speaks the contract;
//! the spec is `docs/local-token-v1.md` in nightwatchd.

use axum::http::{HeaderMap, header};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "local-token";
pub const ACTOR_HEADER: &str = "x-cybercore-actor";

pub struct LocalToken {
    token: String,
    path: Option<PathBuf>,
}

impl LocalToken {
    /// A fresh token, written where local clients will look for it. If no
    /// runtime directory is available the token still exists but nobody can
    /// read it, so bearer requests are effectively disabled.
    pub fn create(service: &str) -> LocalToken {
        let token = random_hex(32);
        let path = runtime_dir(service).and_then(|dir| match write(&dir, &token) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("[{service}] local-token: can't write {}: {e}", dir.display());
                None
            }
        });
        if path.is_none() {
            eprintln!("[{service}] local-token: no runtime directory; local clients disabled");
        }
        LocalToken { token, path }
    }

    #[cfg(test)]
    pub fn fixed(token: &str) -> LocalToken {
        LocalToken {
            token: token.into(),
            path: None,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// True when the request carries this run's token as a bearer.
    pub fn verify(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| constant_time_eq(t.trim().as_bytes(), self.token.as_bytes()))
    }

    /// Who the local client says it is acting for. Only meaningful after
    /// `verify` succeeded; trimmed to printable ASCII and 80 characters.
    pub fn actor(headers: &HeaderMap) -> Option<String> {
        let raw = headers.get(ACTOR_HEADER)?.to_str().ok()?;
        let clean: String = raw
            .chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .take(80)
            .collect();
        let clean = clean.trim().to_string();
        (!clean.is_empty()).then_some(clean)
    }
}

fn runtime_dir(service: &str) -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("RUNTIME_DIRECTORY") {
        // systemd may list several, separated by ':'; ours is the first.
        let first = d.to_string_lossy().split(':').next()?.to_string();
        return Some(PathBuf::from(first));
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(base).join(service))
}

fn write(dir: &Path, token: &str) -> std::io::Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(FILE_NAME);
    let tmp = dir.join(format!("{FILE_NAME}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    writeln!(f, "{token}")?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("read /dev/urandom");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn bearer_must_match_exactly() {
        let t = LocalToken::fixed("abc123");
        assert!(t.verify(&headers(&[("authorization", "Bearer abc123")])));
        assert!(!t.verify(&headers(&[("authorization", "Bearer abc124")])));
        assert!(!t.verify(&headers(&[("authorization", "Basic abc123")])));
        assert!(!t.verify(&headers(&[])));
    }

    #[test]
    fn actor_is_sanitized() {
        let h = headers(&[(ACTOR_HEADER, "  nightwatch:Pixel 9  ")]);
        assert_eq!(LocalToken::actor(&h).as_deref(), Some("nightwatch:Pixel 9"));
        let long = "x".repeat(200);
        let h = headers(&[(ACTOR_HEADER, long.as_str())]);
        assert_eq!(LocalToken::actor(&h).unwrap().len(), 80);
        assert!(LocalToken::actor(&headers(&[])).is_none());
    }

    #[test]
    fn file_is_private() {
        let dir = std::env::temp_dir().join(format!("localtoken-{}", random_hex(4)));
        let path = write(&dir.join("svc"), "deadbeef").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "deadbeef");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // A second run replaces the token.
        write(&dir.join("svc"), "cafe").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "cafe");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
