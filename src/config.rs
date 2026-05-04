use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_addr: String,
    pub root: PathBuf,
    pub root_canonical: PathBuf,
    pub index_name: OsString,
    pub assets_prefix: Vec<OsString>,
    pub threads: usize,
    pub queue_size: usize,
    pub gzip_enabled: bool,
    pub shutdown_timeout: Duration,
}

#[derive(Debug)]
pub enum ConfigError {
    Missing(&'static str),
    Invalid(&'static str, String),
    Io(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(k) => write!(f, "missing required env var {k}"),
            Self::Invalid(k, v) => write!(f, "invalid value for {k}: {v}"),
            Self::Io(v) => write!(f, "io error: {v}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let listen_addr =
            env::var("STATIKA_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

        let root = env::var_os("STATIKA_ROOT")
            .map(PathBuf::from)
            .ok_or(ConfigError::Missing("STATIKA_ROOT"))?;

        let index_name =
            env::var_os("STATIKA_INDEX").unwrap_or_else(|| OsString::from("index.html"));
        validate_relative_path("STATIKA_INDEX", &index_name)?;

        let assets_prefix_raw =
            env::var_os("STATIKA_ASSETS_PATH").unwrap_or_else(|| OsString::from("/assets"));
        let assets_prefix = normalize_prefix(&assets_prefix_raw)?;

        let threads = parse_usize_env("STATIKA_THREADS")?
            .unwrap_or_else(default_threads)
            .max(1);
        let queue_size = parse_usize_env("STATIKA_QUEUE_SIZE")?
            .unwrap_or_else(|| threads.saturating_mul(8).max(1));
        let gzip_enabled = parse_bool_env("STATIKA_GZIP").unwrap_or(true);
        let shutdown_timeout = Duration::from_secs(
            parse_usize_env("STATIKA_SHUTDOWN_TIMEOUT_SECS")?.unwrap_or(5) as u64,
        );

        let root_canonical = std::fs::canonicalize(&root).map_err(|e| {
            ConfigError::Io(format!(
                "failed to canonicalize root {}: {e}",
                root.display()
            ))
        })?;

        if !root_canonical.is_dir() {
            return Err(ConfigError::Invalid(
                "STATIKA_ROOT",
                format!("not a directory: {}", root_canonical.display()),
            ));
        }

        let index_path = root_canonical.join(&index_name);
        let index_meta = std::fs::metadata(&index_path)
            .map_err(|e| ConfigError::Io(format!("missing index {}: {e}", index_path.display())))?;
        if !index_meta.is_file() {
            return Err(ConfigError::Invalid(
                "STATIKA_INDEX",
                format!("not a regular file: {}", index_path.display()),
            ));
        }

        Ok(Self {
            listen_addr,
            root,
            root_canonical,
            index_name,
            assets_prefix,
            threads,
            queue_size,
            gzip_enabled,
            shutdown_timeout,
        })
    }

    pub fn root_index_path(&self) -> PathBuf {
        self.root.join(&self.index_name)
    }
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

fn parse_usize_env(name: &'static str) -> Result<Option<usize>, ConfigError> {
    match env::var(name) {
        Ok(v) => v
            .parse::<usize>()
            .map(Some)
            .map_err(|_| ConfigError::Invalid(name, v)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(v)) => {
            Err(ConfigError::Invalid(name, v.to_string_lossy().into_owned()))
        }
    }
}

fn parse_bool_env(name: &'static str) -> Option<bool> {
    let raw = env::var(name).ok()?;
    match raw.as_str() {
        "1" | "true" | "TRUE" | "True" | "yes" | "YES" | "on" | "ON" => Some(true),
        "0" | "false" | "FALSE" | "False" | "no" | "NO" | "off" | "OFF" => Some(false),
        _ => None,
    }
}

fn validate_relative_path(name: &'static str, raw: &OsString) -> Result<(), ConfigError> {
    let p = Path::new(raw);
    for c in p.components() {
        use std::path::Component;
        match c {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(ConfigError::Invalid(
                    name,
                    raw.to_string_lossy().into_owned(),
                ))
            }
        }
    }
    Ok(())
}

fn normalize_prefix(raw: &OsString) -> Result<Vec<OsString>, ConfigError> {
    let p = Path::new(raw);
    let mut out = Vec::new();
    for c in p.components() {
        use std::path::Component;
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(s) => out.push(s.to_os_string()),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(ConfigError::Invalid(
                    "STATIKA_ASSETS_PATH",
                    raw.to_string_lossy().into_owned(),
                ))
            }
        }
    }
    Ok(out)
}
