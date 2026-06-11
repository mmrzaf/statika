use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_THREADS: usize = 256;
const MAX_QUEUE_SIZE: usize = 65_536;
const MAX_TIMEOUT_SECS: u64 = 300;

#[derive(Clone, Debug)]
pub struct Config {
    listen_addr: SocketAddr,
    root: PathBuf,
    index: Vec<Vec<u8>>,
    assets_prefix: Vec<Vec<u8>>,
    threads: usize,
    queue_size: usize,
    gzip_enabled: bool,
    request_timeout: Duration,
    shutdown_timeout: Duration,
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(&'static str),
    Invalid(&'static str, String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(key) => write!(f, "missing required environment variable {key}"),
            Self::Invalid(key, value) => write!(f, "invalid value for {key}: {value}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let listen_addr = env::var("STATIKA_LISTEN_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse::<SocketAddr>()
            .map_err(|_| {
                ConfigError::Invalid("STATIKA_LISTEN_ADDR", value("STATIKA_LISTEN_ADDR"))
            })?;

        let root = env::var_os("STATIKA_ROOT")
            .map(PathBuf::from)
            .ok_or(ConfigError::Missing("STATIKA_ROOT"))?;
        if !root.is_absolute() {
            return Err(ConfigError::Invalid(
                "STATIKA_ROOT",
                root.to_string_lossy().into_owned(),
            ));
        }

        let index_raw =
            env::var_os("STATIKA_INDEX").unwrap_or_else(|| OsString::from("index.html"));
        let index = parse_relative_path("STATIKA_INDEX", &index_raw)?;

        let assets_raw =
            env::var_os("STATIKA_ASSETS_PATH").unwrap_or_else(|| OsString::from("/assets"));
        let assets_prefix = parse_url_prefix("STATIKA_ASSETS_PATH", &assets_raw)?;

        let threads = parse_usize("STATIKA_THREADS", default_threads(), 1, MAX_THREADS)?;
        let queue_size = parse_usize(
            "STATIKA_QUEUE_SIZE",
            threads.saturating_mul(64).clamp(1, MAX_QUEUE_SIZE),
            1,
            MAX_QUEUE_SIZE,
        )?;
        let gzip_enabled = parse_bool("STATIKA_GZIP", true)?;
        let request_timeout = Duration::from_secs(parse_u64(
            "STATIKA_REQUEST_TIMEOUT_SECS",
            5,
            1,
            MAX_TIMEOUT_SECS,
        )?);
        let shutdown_timeout = Duration::from_secs(parse_u64(
            "STATIKA_SHUTDOWN_TIMEOUT_SECS",
            10,
            1,
            MAX_TIMEOUT_SECS,
        )?);

        Ok(Self {
            listen_addr,
            root,
            index,
            assets_prefix,
            threads,
            queue_size,
            gzip_enabled,
            request_timeout,
            shutdown_timeout,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index(&self) -> &[Vec<u8>] {
        &self.index
    }

    pub fn assets_prefix(&self) -> &[Vec<u8>] {
        &self.assets_prefix
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn queue_size(&self) -> usize {
        self.queue_size
    }

    pub fn gzip_enabled(&self) -> bool {
        self.gzip_enabled
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }
}

fn value(key: &'static str) -> String {
    env::var(key).unwrap_or_else(|_| "<non-UTF-8>".to_owned())
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 32)
}

fn parse_usize(
    key: &'static str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ConfigError> {
    match env::var(key) {
        Ok(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|parsed| (min..=max).contains(parsed))
            .ok_or(ConfigError::Invalid(key, raw)),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            Err(ConfigError::Invalid(key, "<non-UTF-8>".to_owned()))
        }
    }
}

fn parse_u64(key: &'static str, default: u64, min: u64, max: u64) -> Result<u64, ConfigError> {
    match env::var(key) {
        Ok(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|parsed| (min..=max).contains(parsed))
            .ok_or(ConfigError::Invalid(key, raw)),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            Err(ConfigError::Invalid(key, "<non-UTF-8>".to_owned()))
        }
    }
}

fn parse_bool(key: &'static str, default: bool) -> Result<bool, ConfigError> {
    match env::var(key) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(ConfigError::Invalid(key, raw)),
        },
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => {
            Err(ConfigError::Invalid(key, "<non-UTF-8>".to_owned()))
        }
    }
}

fn parse_url_prefix(key: &'static str, value: &OsStr) -> Result<Vec<Vec<u8>>, ConfigError> {
    let bytes = value.as_bytes();
    if !bytes.starts_with(b"/") {
        return Err(ConfigError::Invalid(
            key,
            value.to_string_lossy().into_owned(),
        ));
    }

    let trimmed = bytes.strip_prefix(b"/").unwrap_or(bytes);
    let trimmed = trimmed.strip_suffix(b"/").unwrap_or(trimmed);
    parse_components(key, trimmed)
}

fn parse_relative_path(key: &'static str, value: &OsStr) -> Result<Vec<Vec<u8>>, ConfigError> {
    let bytes = value.as_bytes();
    if bytes.starts_with(b"/") {
        return Err(ConfigError::Invalid(
            key,
            value.to_string_lossy().into_owned(),
        ));
    }
    parse_components(key, bytes)
}

fn parse_components(key: &'static str, bytes: &[u8]) -> Result<Vec<Vec<u8>>, ConfigError> {
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'\\') {
        return Err(ConfigError::Invalid(
            key,
            String::from_utf8_lossy(bytes).into_owned(),
        ));
    }

    let mut components = Vec::new();
    for component in bytes.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." || component == b".." {
            return Err(ConfigError::Invalid(
                key,
                String::from_utf8_lossy(bytes).into_owned(),
            ));
        }
        components.push(component.to_vec());
    }
    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::{parse_components, parse_url_prefix};
    use std::ffi::OsStr;

    #[test]
    fn path_components_reject_escape_and_empty_segments() {
        assert!(parse_components("X", b"../secret").is_err());
        assert!(parse_components("X", b"assets//app.js").is_err());
        assert!(parse_components("X", b"assets\\app.js").is_err());
        assert!(parse_components("X", b"assets/app.js").is_ok());
    }

    #[test]
    fn asset_prefix_requires_absolute_url_path() {
        assert!(parse_url_prefix("X", OsStr::new("/assets/")).is_ok());
        assert!(parse_url_prefix("X", OsStr::new("assets")).is_err());
    }
}
