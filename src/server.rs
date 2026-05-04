use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{Config, ConfigError};
use crate::ffi;
use crate::http::{
    is_health_endpoint, log_request, mime_for_path, normalize_path, read_request,
    read_request_target, write_file_response, write_simple_response, RequestError, StatusCode,
};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn run(config: Config) -> Result<(), ServerError> {
    install_signal_handlers()?;

    let listener = TcpListener::bind(&config.listen_addr)
        .map_err(|e| ServerError::Startup(format!("bind {} failed: {e}", config.listen_addr)))?;
    listener
        .set_nonblocking(false)
        .map_err(|e| ServerError::Startup(format!("failed to configure listener: {e}")))?;

    let queue = Arc::new(WorkQueue::new(config.queue_size));
    let config = Arc::new(config);

    let mut workers = Vec::with_capacity(config.threads);
    for _ in 0..config.threads {
        let queue = Arc::clone(&queue);
        let config = Arc::clone(&config);
        workers.push(thread::spawn(move || worker_loop(queue, config)));
    }

    let accept_result = accept_loop(&listener, &queue);
    queue.close();

    let deadline = Instant::now() + config.shutdown_timeout;
    while !workers.iter().all(|h| h.is_finished()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if !workers.iter().all(|h| h.is_finished()) {
        std::process::exit(0);
    }

    for worker in workers {
        let _ = worker.join();
    }

    accept_result
}

#[derive(Debug)]
pub enum ServerError {
    Startup(String),
    Config(ConfigError),
    Io(io::Error),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Startup(s) => write!(f, "{s}"),
            Self::Config(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<ConfigError> for ServerError {
    fn from(value: ConfigError) -> Self {
        Self::Config(value)
    }
}

impl From<io::Error> for ServerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

struct WorkQueue {
    inner: Mutex<QueueState>,
    not_empty: Condvar,
    not_full: Condvar,
    capacity: usize,
}

struct QueueState {
    items: VecDeque<TcpStream>,
    closed: bool,
}

impl WorkQueue {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(QueueState {
                items: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    fn close(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.closed = true;
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    fn push(&self, stream: TcpStream) -> Result<(), TcpStream> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed || guard.items.len() >= self.capacity {
            return Err(stream);
        }
        guard.items.push_back(stream);
        self.not_empty.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<TcpStream> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(stream) = guard.items.pop_front() {
                self.not_full.notify_one();
                return Some(stream);
            }
            if guard.closed {
                return None;
            }
            guard = self
                .not_empty
                .wait(guard)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    fn wait_for_space_or_shutdown(&self, shutdown: &AtomicBool) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while guard.items.len() >= self.capacity
            && !shutdown.load(Ordering::Relaxed)
            && !guard.closed
        {
            let res = self
                .not_full
                .wait_timeout(guard, Duration::from_millis(200))
                .unwrap_or_else(|e| e.into_inner());
            guard = res.0;
        }
    }
}

fn accept_loop(listener: &TcpListener, queue: &Arc<WorkQueue>) -> Result<(), ServerError> {
    while !SHUTDOWN.load(Ordering::Relaxed) {
        queue.wait_for_space_or_shutdown(&SHUTDOWN);
        if SHUTDOWN.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let _ = stream.set_nodelay(true);
                if queue.push(stream).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                if SHUTDOWN.load(Ordering::Relaxed) {
                    break;
                }
            }
            Err(e) => return Err(ServerError::Io(e)),
        }
    }
    Ok(())
}

fn worker_loop(queue: Arc<WorkQueue>, config: Arc<Config>) {
    while let Some(mut stream) = queue.pop() {
        let peer = stream.peer_addr().ok();
        let start = Instant::now();
        let outcome = handle_connection(&mut stream, &config);
        let duration = start.elapsed();
        let _ = stream.shutdown(std::net::Shutdown::Both);
        log_request(
            peer,
            "GET",
            &outcome.path,
            outcome.status,
            outcome.bytes,
            duration,
            outcome.error,
        );
    }
}

struct RequestOutcome {
    status: StatusCode,
    bytes: u64,
    error: Option<&'static str>,
    path: String,
}

fn handle_connection(stream: &mut TcpStream, config: &Config) -> RequestOutcome {
    if stream
        .set_read_timeout(Some(crate::http::REQUEST_TIMEOUT))
        .is_err()
    {
        let _ = write_simple_response(
            stream,
            StatusCode::InternalServerError,
            b"internal server error",
            &[],
        );
        return RequestOutcome::internal("timeout_config", String::new());
    }
    if stream
        .set_write_timeout(Some(crate::http::REQUEST_TIMEOUT))
        .is_err()
    {
        let _ = write_simple_response(
            stream,
            StatusCode::InternalServerError,
            b"internal server error",
            &[],
        );
        return RequestOutcome::internal("timeout_config", String::new());
    }

    let request = match read_request(stream) {
        Ok(r) => r,
        Err(RequestError::Timeout) => {
            return RequestOutcome::with_error(StatusCode::BadRequest, "timeout", String::new())
        }
        Err(RequestError::BadRequest) => {
            let _ = write_simple_response(stream, StatusCode::BadRequest, b"bad request", &[]);
            return RequestOutcome::with_error(
                StatusCode::BadRequest,
                "bad_request",
                String::new(),
            );
        }
        Err(RequestError::MethodNotAllowed) => {
            let _ = write_simple_response(
                stream,
                StatusCode::NotAllowed,
                b"method not allowed",
                &[("Allow", "GET")],
            );
            return RequestOutcome::with_error(
                StatusCode::NotAllowed,
                "method_not_allowed",
                String::new(),
            );
        }
        Err(RequestError::Io(_)) => {
            let _ = write_simple_response(
                stream,
                StatusCode::InternalServerError,
                b"internal server error",
                &[],
            );
            return RequestOutcome::with_error(
                StatusCode::InternalServerError,
                "io",
                String::new(),
            );
        }
    };

    let raw_target = request.target;
    let path_for_log = String::from_utf8_lossy(&raw_target).into_owned();

    if is_health_endpoint(&raw_target) {
        match write_simple_response(stream, StatusCode::Ok, b"ok\n", &[]) {
            Ok(bytes) => {
                return RequestOutcome {
                    status: StatusCode::Ok,
                    bytes,
                    error: None,
                    path: path_for_log,
                }
            }
            Err(_) => {
                return RequestOutcome::with_error(
                    StatusCode::InternalServerError,
                    "write",
                    path_for_log,
                )
            }
        }
    }

    let decoded = match read_request_target(&raw_target) {
        Ok(d) => d,
        Err(RequestError::BadRequest) => {
            let _ = write_simple_response(stream, StatusCode::BadRequest, b"bad request", &[]);
            return RequestOutcome::with_error(StatusCode::BadRequest, "bad_request", path_for_log);
        }
        Err(RequestError::Timeout) => {
            return RequestOutcome::with_error(StatusCode::BadRequest, "timeout", path_for_log)
        }
        Err(RequestError::Io(_)) => {
            return RequestOutcome::with_error(StatusCode::InternalServerError, "io", path_for_log)
        }
        Err(RequestError::MethodNotAllowed) => {
            // unreachable in this stage, but must be handled
            let _ = write_simple_response(
                stream,
                StatusCode::NotAllowed,
                b"method not allowed",
                &[("Allow", "GET")],
            );
            return RequestOutcome::with_error(
                StatusCode::NotAllowed,
                "method_not_allowed",
                path_for_log,
            );
        }
    };
    let segments = match normalize_path(&decoded) {
        Ok(s) => s,
        Err(_) => {
            let _ = write_simple_response(stream, StatusCode::BadRequest, b"bad request", &[]);
            return RequestOutcome::with_error(StatusCode::BadRequest, "bad_request", path_for_log);
        }
    };

    let resolved = match resolve_path(config, &segments, request.accept_gzip) {
        Ok(r) => r,
        Err(status) => {
            let _ = write_simple_response(stream, status, b"forbidden", &[]);
            return RequestOutcome::with_error(status, "forbidden", path_for_log);
        }
    };

    let file = match File::open(&resolved.path) {
        Ok(f) => f,
        Err(_) => {
            let _ = write_simple_response(
                stream,
                StatusCode::InternalServerError,
                b"internal server error",
                &[],
            );
            return RequestOutcome::internal("open", path_for_log);
        }
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(_) => {
            let _ = write_simple_response(
                stream,
                StatusCode::InternalServerError,
                b"internal server error",
                &[],
            );
            return RequestOutcome::internal("metadata", path_for_log);
        }
    };
    if !meta.is_file() {
        let _ = write_simple_response(
            stream,
            StatusCode::InternalServerError,
            b"internal server error",
            &[],
        );
        return RequestOutcome::with_error(
            StatusCode::InternalServerError,
            "not_file",
            path_for_log,
        );
    }

    match write_file_response(
        stream,
        StatusCode::Ok,
        meta.len(),
        resolved.mime,
        resolved.immutable_cache,
        resolved.gzip,
        &file,
    ) {
        Ok(bytes) => RequestOutcome {
            status: StatusCode::Ok,
            bytes,
            error: None,
            path: path_for_log,
        },
        Err(_) => {
            let _ = write_simple_response(
                stream,
                StatusCode::InternalServerError,
                b"internal server error",
                &[],
            );
            RequestOutcome::internal("write", path_for_log)
        }
    }
}

impl RequestOutcome {
    fn with_error(status: StatusCode, error: &'static str, path: String) -> Self {
        Self {
            status,
            bytes: 0,
            error: Some(error),
            path,
        }
    }

    fn internal(error: &'static str, path: String) -> Self {
        Self {
            status: StatusCode::InternalServerError,
            bytes: 0,
            error: Some(error),
            path,
        }
    }
}

#[derive(Debug)]
struct ResolvedPath {
    path: PathBuf,
    mime: &'static str,
    gzip: bool,
    immutable_cache: bool,
}

fn resolve_path(
    config: &Config,
    segments: &[Vec<u8>],
    accept_gzip: bool,
) -> Result<ResolvedPath, StatusCode> {
    let candidate = join_segments(&config.root, segments);
    match canonicalize_within_root(&candidate, &config.root_canonical) {
        Ok(Some(path)) => {
            if is_dir(&path) {
                let dir_index = path.join(&config.index_name);
                match canonicalize_within_root(&dir_index, &config.root_canonical) {
                    Ok(Some(index_path)) => Ok(select_variant(index_path, config, accept_gzip)),
                    Ok(None) => fallback_index(config, accept_gzip),
                    Err(status) => Err(status),
                }
            } else {
                Ok(select_variant(path, config, accept_gzip))
            }
        }
        Ok(None) => fallback_index(config, accept_gzip),
        Err(status) => Err(status),
    }
}

fn fallback_index(config: &Config, accept_gzip: bool) -> Result<ResolvedPath, StatusCode> {
    let index = config.root_index_path();
    match canonicalize_within_root(&index, &config.root_canonical)? {
        Some(path) => Ok(select_variant(path, config, accept_gzip)),
        None => Err(StatusCode::InternalServerError),
    }
}

fn select_variant(path: PathBuf, config: &Config, accept_gzip: bool) -> ResolvedPath {
    let mime = mime_for_path(&path);
    let immutable_cache = path_is_under_assets_from_path(
        path.strip_prefix(&config.root_canonical).unwrap_or(&path),
        &config.assets_prefix,
    );

    if config.gzip_enabled && accept_gzip && !path_ends_with_gz(&path) {
        let gz = append_gz(&path);
        if let Ok(Some(gz_path)) = canonicalize_within_root(&gz, &config.root_canonical) {
            return ResolvedPath {
                path: gz_path,
                mime,
                gzip: true,
                immutable_cache,
            };
        }
    }

    ResolvedPath {
        path,
        mime,
        gzip: false,
        immutable_cache,
    }
}

fn canonicalize_within_root(
    path: &Path,
    root_canonical: &Path,
) -> Result<Option<PathBuf>, StatusCode> {
    let canon = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    if !canon.starts_with(root_canonical) {
        return Err(StatusCode::Forbidden);
    }
    let meta = match fs::metadata(&canon) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if meta.is_dir() {
        return Ok(Some(canon));
    }
    if meta.is_file() {
        return Ok(Some(canon));
    }
    Ok(None)
}

fn is_dir(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn join_segments(root: &Path, segments: &[Vec<u8>]) -> PathBuf {
    let mut out = root.to_path_buf();
    for seg in segments {
        out.push(OsString::from_vec(seg.clone()));
    }
    out
}

fn path_is_under_assets_from_path(path: &Path, prefix: &[OsString]) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let mut comps = path.components().filter_map(|c| match c {
        std::path::Component::Normal(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    });
    for pref in prefix {
        match comps.next() {
            Some(seg) if seg.as_slice() == pref.as_os_str().as_bytes() => {}
            _ => return false,
        }
    }
    true
}

fn append_gz(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".gz");
    PathBuf::from(os)
}

fn path_ends_with_gz(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.ends_with(".gz"))
        .unwrap_or(false)
}

unsafe extern "C" fn handle_signal(_signum: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() -> Result<(), ServerError> {
    unsafe {
        let mut action: ffi::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_signal as usize;
        action.sa_flags = 0;
        if ffi::sigemptyset(&mut action.sa_mask as *mut ffi::sigset_t) != 0 {
            return Err(ServerError::Startup("sigemptyset failed".to_string()));
        }
        if ffi::sigaction(
            ffi::SIGINT,
            &action as *const ffi::sigaction,
            std::ptr::null_mut(),
        ) != 0
        {
            return Err(ServerError::Startup("sigaction(SIGINT) failed".to_string()));
        }
        if ffi::sigaction(
            ffi::SIGTERM,
            &action as *const ffi::sigaction,
            std::ptr::null_mut(),
        ) != 0
        {
            return Err(ServerError::Startup(
                "sigaction(SIGTERM) failed".to_string(),
            ));
        }
    }
    Ok(())
}
