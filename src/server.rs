use crate::config::Config;
use crate::fs::{gzip_path, is_not_found, RootDir};
use crate::http::{
    content_type, decode_target, has_fingerprint, if_none_match_matches, read_request,
    response_head, DecodedPath, Method, Request, RequestError, Status,
};
use crate::net;
use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ACCEPT_TRANSIENT_BACKOFF: Duration = Duration::from_millis(10);
const ACCEPT_RESOURCE_BACKOFF: Duration = Duration::from_millis(100);
const ERROR_RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum ServerError {
    Io(io::Error),
    ShutdownTimeout,
    WorkerPanic,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::ShutdownTimeout => write!(f, "workers did not stop before shutdown timeout"),
            Self::WorkerPanic => write!(f, "worker thread panicked"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<io::Error> for ServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn run(config: Config) -> Result<(), ServerError> {
    SHUTDOWN.store(false, Ordering::Release);
    install_signal_handlers()?;

    let root = Arc::new(RootDir::open(config.root())?);
    let listener = TcpListener::bind(config.listen_addr())?;
    listener.set_nonblocking(true)?;

    let config = Arc::new(config);
    let queue = Arc::new(WorkQueue::new(config.queue_size()));
    let (finished_tx, finished_rx) = mpsc::channel();
    let mut workers = Vec::with_capacity(config.threads());
    for worker_id in 0..config.threads() {
        let queue = Arc::clone(&queue);
        let config = Arc::clone(&config);
        let root = Arc::clone(&root);
        let finished_tx = finished_tx.clone();
        workers.push(thread::spawn(move || {
            worker_loop(queue, root, config);
            let _ = finished_tx.send(worker_id);
        }));
    }
    drop(finished_tx);

    log_started(&config);
    let accept_result = accept_loop(&listener, &queue, config.request_timeout());
    queue.close();
    let shutdown_result = join_workers(workers, finished_rx, config.shutdown_timeout());
    log_stopped(accept_result.is_ok() && shutdown_result.is_ok());

    accept_result?;
    shutdown_result
}

fn accept_loop(
    listener: &TcpListener,
    queue: &WorkQueue,
    request_timeout: Duration,
) -> Result<(), ServerError> {
    while !SHUTDOWN.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let accepted_at = Instant::now();
                if let Err(error) = configure_stream(&stream) {
                    log_accept_error(&error);
                    continue;
                }
                if !queue.push(
                    WorkItem {
                        stream,
                        accepted_at,
                        deadline: accepted_at + request_timeout,
                    },
                    &SHUTDOWN,
                ) {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let deadline = Instant::now() + ACCEPT_POLL_INTERVAL;
                if let Err(error) = net::wait_fd(listener.as_raw_fd(), libc::POLLIN, deadline) {
                    if error.kind() != io::ErrorKind::TimedOut
                        && error.kind() != io::ErrorKind::Interrupted
                    {
                        return Err(error.into());
                    }
                }
            }
            Err(error) if is_transient_accept_error(&error) => {
                sleep_until_shutdown(ACCEPT_TRANSIENT_BACKOFF);
            }
            Err(error) if is_accept_resource_exhaustion(&error) => {
                log_accept_error(&error);
                sleep_until_shutdown(ACCEPT_RESOURCE_BACKOFF);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn configure_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    Ok(())
}

fn is_transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENETDOWN)
            | Some(libc::EPROTO)
            | Some(libc::ENOPROTOOPT)
            | Some(libc::EHOSTDOWN)
            | Some(libc::ENONET)
            | Some(libc::EHOSTUNREACH)
            | Some(libc::EOPNOTSUPP)
            | Some(libc::ENETUNREACH)
            | Some(libc::ECONNABORTED)
    )
}

fn is_accept_resource_exhaustion(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE) | Some(libc::ENFILE)
    )
}

fn sleep_until_shutdown(duration: Duration) {
    let deadline = Instant::now() + duration;
    while !SHUTDOWN.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(ACCEPT_POLL_INTERVAL));
    }
}

fn worker_loop(queue: Arc<WorkQueue>, root: Arc<RootDir>, config: Arc<Config>) {
    while let Some(mut work) = queue.pop() {
        let peer = work.stream.peer_addr().ok();
        let outcome = handle_connection(&mut work.stream, &root, &config, work.deadline);
        let _ = work.stream.shutdown(Shutdown::Both);
        log_request(peer, &outcome, work.accepted_at.elapsed());
    }
}

struct RequestOutcome {
    method: String,
    path: String,
    status: Status,
    bytes: u64,
    error: Option<&'static str>,
}

impl RequestOutcome {
    fn new(method: impl Into<String>, path: impl Into<String>, status: Status, bytes: u64) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            status,
            bytes,
            error: None,
        }
    }

    fn error(
        method: impl Into<String>,
        path: impl Into<String>,
        status: Status,
        error: &'static str,
    ) -> Self {
        Self::error_with_bytes(method, path, status, 0, error)
    }

    fn error_with_bytes(
        method: impl Into<String>,
        path: impl Into<String>,
        status: Status,
        bytes: u64,
        error: &'static str,
    ) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            status,
            bytes,
            error: Some(error),
        }
    }
}

struct ResponseContext<'a> {
    deadline: Instant,
    method: Method,
    path: &'a str,
}

fn handle_connection(
    stream: &mut TcpStream,
    root: &RootDir,
    config: &Config,
    deadline: Instant,
) -> RequestOutcome {
    if Instant::now() >= deadline {
        return simple_error(
            stream,
            deadline,
            "UNKNOWN",
            "",
            Status::RequestTimeout,
            "queue_timeout",
        );
    }

    let request = match read_request(stream, deadline) {
        Ok(request) => request,
        Err(RequestError::BadRequest) => {
            return simple_error(
                stream,
                deadline,
                "UNKNOWN",
                "",
                Status::BadRequest,
                "bad_request",
            )
        }
        Err(RequestError::HeaderTooLarge) => {
            return simple_error(
                stream,
                deadline,
                "UNKNOWN",
                "",
                Status::HeaderTooLarge,
                "header_too_large",
            )
        }
        Err(RequestError::MethodNotAllowed(method)) => {
            return simple_error(
                stream,
                deadline,
                &method,
                "",
                Status::MethodNotAllowed,
                "method_not_allowed",
            )
        }
        Err(RequestError::Timeout) => {
            return simple_error(
                stream,
                deadline,
                "UNKNOWN",
                "",
                Status::RequestTimeout,
                "timeout",
            )
        }
        Err(RequestError::Io) => {
            return RequestOutcome::error("UNKNOWN", "", Status::InternalServerError, "read_io");
        }
    };

    let path_for_log = request_path_for_log(&request.target);
    let decoded = match decode_target(&request.target) {
        Ok(decoded) => decoded,
        Err(_) => {
            return simple_error(
                stream,
                deadline,
                request.method.as_str(),
                &path_for_log,
                Status::BadRequest,
                "bad_target",
            )
        }
    };

    if is_health_path(&decoded.components) {
        let response = ResponseContext {
            deadline,
            method: request.method,
            path: &path_for_log,
        };
        return simple_response(
            stream,
            &response,
            Status::Ok,
            b"ok\n",
            "text/plain; charset=utf-8",
            &[("Cache-Control", "no-store")],
        );
    }

    let response = ResponseContext {
        deadline,
        method: request.method,
        path: &path_for_log,
    };
    serve_static(stream, root, config, request, decoded, &response)
}

fn serve_static(
    stream: &mut TcpStream,
    root: &RootDir,
    config: &Config,
    request: Request,
    decoded: DecodedPath,
    response: &ResponseContext<'_>,
) -> RequestOutcome {
    let mut components = decoded.components;
    let requested_asset = starts_with_components(&components, config.assets_prefix());
    if components.is_empty() || decoded.trailing_slash {
        components.extend_from_slice(config.index());
    }

    let (file, served_components, compressed, fallback) = match open_representation(
        root,
        &components,
        request.accept_gzip && config.gzip_enabled(),
    ) {
        Ok(opened) => (opened.0, components.clone(), opened.1, false),
        Err(error) if is_not_found(&error) && !requested_asset => {
            match open_representation(
                root,
                config.index(),
                request.accept_gzip && config.gzip_enabled(),
            ) {
                Ok(opened) => (opened.0, config.index().to_vec(), opened.1, true),
                Err(index_error) if is_not_found(&index_error) => {
                    return simple_error(
                        stream,
                        response.deadline,
                        request.method.as_str(),
                        response.path,
                        Status::NotFound,
                        "not_found",
                    )
                }
                Err(_) => {
                    return simple_error(
                        stream,
                        response.deadline,
                        request.method.as_str(),
                        response.path,
                        Status::InternalServerError,
                        "open_index",
                    )
                }
            }
        }
        Err(error) if is_not_found(&error) => {
            return simple_error(
                stream,
                response.deadline,
                request.method.as_str(),
                response.path,
                Status::NotFound,
                "not_found",
            )
        }
        Err(_) => {
            return simple_error(
                stream,
                response.deadline,
                request.method.as_str(),
                response.path,
                Status::InternalServerError,
                "open_file",
            )
        }
    };

    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_) => {
            return simple_error(
                stream,
                response.deadline,
                request.method.as_str(),
                response.path,
                Status::InternalServerError,
                "metadata",
            )
        }
    };
    let etag = etag(&metadata);
    let cache_control = cache_control(&served_components, fallback, config);
    let mime = content_type(&served_components);
    let mut headers = vec![("Cache-Control", cache_control), ("ETag", etag.as_str())];
    if config.gzip_enabled() {
        headers.push(("Vary", "Accept-Encoding"));
    }
    if compressed {
        headers.push(("Content-Encoding", "gzip"));
    }

    if if_none_match_matches(request.if_none_match.as_deref(), &etag) {
        let head = response_head(Status::NotModified, metadata.len(), None, &headers);
        return match net::write_all(stream, &head, response.deadline) {
            Ok(()) => RequestOutcome::new(
                request.method.as_str(),
                response.path,
                Status::NotModified,
                0,
            ),
            Err(_) => RequestOutcome::error(
                request.method.as_str(),
                response.path,
                Status::NotModified,
                "write_header",
            ),
        };
    }

    let head = response_head(Status::Ok, metadata.len(), Some(mime), &headers);
    if net::write_all(stream, &head, response.deadline).is_err() {
        return RequestOutcome::error(
            request.method.as_str(),
            response.path,
            Status::Ok,
            "write_header",
        );
    }
    if request.method == Method::Head {
        return RequestOutcome::new(request.method.as_str(), response.path, Status::Ok, 0);
    }

    match net::send_file(stream, &file, metadata.len(), response.deadline) {
        Ok(bytes) => RequestOutcome::new(request.method.as_str(), response.path, Status::Ok, bytes),
        Err(_) => RequestOutcome::error(
            request.method.as_str(),
            response.path,
            Status::Ok,
            "send_file",
        ),
    }
}

fn open_representation(
    root: &RootDir,
    components: &[Vec<u8>],
    gzip: bool,
) -> io::Result<(File, bool)> {
    if gzip {
        let compressed = gzip_path(components);
        match root.open_file(&compressed) {
            Ok(file) => return Ok((file, true)),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error),
        }
    }
    root.open_file(components).map(|file| (file, false))
}

fn request_path_for_log(target: &[u8]) -> String {
    let path = target.split(|byte| *byte == b'?').next().unwrap_or(target);
    String::from_utf8_lossy(path).into_owned()
}

fn cache_control(components: &[Vec<u8>], fallback: bool, config: &Config) -> &'static str {
    if fallback || components == config.index() {
        "no-cache"
    } else if starts_with_components(components, config.assets_prefix())
        && has_fingerprint(components)
    {
        "public, max-age=31536000, immutable"
    } else if starts_with_components(components, config.assets_prefix()) {
        "public, max-age=3600"
    } else {
        "no-cache"
    }
}

fn is_health_path(components: &[Vec<u8>]) -> bool {
    components.len() == 1
        && (components[0].as_slice() == b"health" || components[0].as_slice() == b"healthz")
}

fn starts_with_components(path: &[Vec<u8>], prefix: &[Vec<u8>]) -> bool {
    path.starts_with(prefix)
}

fn etag(metadata: &std::fs::Metadata) -> String {
    format!(
        "\"{:x}-{:x}-{:x}-{:x}\"",
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec()
    )
}

fn simple_error(
    stream: &mut TcpStream,
    deadline: Instant,
    method: &str,
    path: &str,
    status: Status,
    error: &'static str,
) -> RequestOutcome {
    let error_deadline = deadline.max(Instant::now() + ERROR_RESPONSE_TIMEOUT);
    let body: &[u8] = match status {
        Status::BadRequest => b"bad request\n",
        Status::NotFound => b"not found\n",
        Status::MethodNotAllowed => b"method not allowed\n",
        Status::RequestTimeout => b"request timeout\n",
        Status::HeaderTooLarge => b"request headers too large\n",
        _ => b"internal server error\n",
    };
    let mut headers = vec![("Cache-Control", "no-store")];
    if status == Status::MethodNotAllowed {
        headers.push(("Allow", "GET, HEAD"));
    }
    let head = response_head(
        status,
        body.len() as u64,
        Some("text/plain; charset=utf-8"),
        &headers,
    );
    if net::write_all(stream, &head, error_deadline).is_err() {
        return RequestOutcome::error(method, path, status, "write_header");
    }
    if method == "HEAD" {
        return RequestOutcome::error(method, path, status, error);
    }
    match net::write_all(stream, body, error_deadline) {
        Ok(()) => RequestOutcome::error_with_bytes(method, path, status, body.len() as u64, error),
        Err(_) => RequestOutcome::error(method, path, status, "write_body"),
    }
}

fn simple_response(
    stream: &mut TcpStream,
    response: &ResponseContext<'_>,
    status: Status,
    body: &[u8],
    content_type: &str,
    headers: &[(&str, &str)],
) -> RequestOutcome {
    let head = response_head(status, body.len() as u64, Some(content_type), headers);
    if net::write_all(stream, &head, response.deadline).is_err() {
        return RequestOutcome::error(
            response.method.as_str(),
            response.path,
            status,
            "write_header",
        );
    }
    if response.method == Method::Head {
        return RequestOutcome::new(response.method.as_str(), response.path, status, 0);
    }
    match net::write_all(stream, body, response.deadline) {
        Ok(()) => RequestOutcome::new(
            response.method.as_str(),
            response.path,
            status,
            body.len() as u64,
        ),
        Err(_) => RequestOutcome::error(
            response.method.as_str(),
            response.path,
            status,
            "write_body",
        ),
    }
}

struct WorkQueue {
    capacity: usize,
    state: Mutex<QueueState>,
    not_empty: Condvar,
    not_full: Condvar,
}

struct WorkItem {
    stream: TcpStream,
    accepted_at: Instant,
    deadline: Instant,
}

struct QueueState {
    streams: VecDeque<WorkItem>,
    closed: bool,
}

impl WorkQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(QueueState {
                streams: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
        }
    }

    fn push(&self, work: WorkItem, shutdown: &AtomicBool) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if discard_expired(&mut state) {
                self.not_full.notify_all();
            }
            if state.streams.len() < self.capacity
                || state.closed
                || shutdown.load(Ordering::Acquire)
            {
                break;
            }
            let (next, _) = self
                .not_full
                .wait_timeout(state, ACCEPT_POLL_INTERVAL)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        }
        if state.closed || shutdown.load(Ordering::Acquire) {
            return false;
        }
        if Instant::now() >= work.deadline {
            return true;
        }
        state.streams.push_back(work);
        self.not_empty.notify_one();
        true
    }

    fn pop(&self) -> Option<WorkItem> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if discard_expired(&mut state) {
                self.not_full.notify_all();
            }
            if let Some(work) = state.streams.pop_front() {
                self.not_full.notify_one();
                return Some(work);
            }
            if state.closed {
                return None;
            }
            state = self
                .not_empty
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        state.streams.clear();
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }
}

fn discard_expired(state: &mut QueueState) -> bool {
    // Accepted connections enter the FIFO with monotonically increasing deadlines.
    let initial_len = state.streams.len();
    let now = Instant::now();
    while state
        .streams
        .front()
        .is_some_and(|work| now >= work.deadline)
    {
        state.streams.pop_front();
    }
    state.streams.len() != initial_len
}

fn join_workers(
    mut workers: Vec<JoinHandle<()>>,
    finished: mpsc::Receiver<usize>,
    timeout: Duration,
) -> Result<(), ServerError> {
    let deadline = Instant::now() + timeout;
    let mut completed = vec![false; workers.len()];
    let mut remaining = workers.len();
    while remaining > 0 {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err(ServerError::ShutdownTimeout);
        }
        match finished.recv_timeout(wait) {
            Ok(worker_id) if !completed[worker_id] => {
                completed[worker_id] = true;
                remaining -= 1;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => return Err(ServerError::ShutdownTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for worker in workers.drain(..) {
        if worker.join().is_err() {
            return Err(ServerError::WorkerPanic);
        }
    }
    Ok(())
}

unsafe extern "C" fn shutdown_signal(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Release);
}

fn install_signal_handlers() -> io::Result<()> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = shutdown_signal as usize;
    action.sa_flags = 0;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn log_started(config: &Config) {
    eprintln!(
        concat!(
            "{{\"event\":\"started\",\"listen\":\"{}\",",
            "\"root\":\"{}\",\"threads\":{},\"queue_size\":{}}}"
        ),
        escape_json(&config.listen_addr().to_string()),
        escape_json(&config.root().to_string_lossy()),
        config.threads(),
        config.queue_size(),
    );
}

fn log_stopped(clean: bool) {
    eprintln!("{{\"event\":\"stopped\",\"clean\":{clean}}}");
}

fn log_accept_error(error: &io::Error) {
    eprintln!(
        "{{\"event\":\"accept_error\",\"error\":\"{}\"}}",
        escape_json(&error.to_string()),
    );
}

fn log_request(peer: Option<SocketAddr>, outcome: &RequestOutcome, elapsed: Duration) {
    let peer = peer.map(|peer| peer.to_string()).unwrap_or_default();
    let error = outcome
        .error
        .map(|error| format!(",\"error\":\"{}\"", escape_json(error)))
        .unwrap_or_default();
    eprintln!(
        concat!(
            "{{\"event\":\"request\",\"remote\":\"{}\",",
            "\"method\":\"{}\",\"path\":\"{}\",\"status\":{},",
            "\"bytes\":{},\"duration_ms\":{}{}}}"
        ),
        escape_json(&peer),
        escape_json(&outcome.method),
        escape_json(&outcome.path),
        outcome.status.code(),
        outcome.bytes,
        elapsed.as_millis(),
        error,
    );
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{
        discard_expired, is_accept_resource_exhaustion, request_path_for_log, QueueState, WorkItem,
    };
    use std::collections::VecDeque;
    use std::io;
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    #[test]
    fn log_path_omits_query_string() {
        assert_eq!(
            request_path_for_log(b"/assets/app.js?token=secret"),
            "/assets/app.js"
        );
    }

    #[test]
    fn descriptor_exhaustion_is_recoverable() {
        assert!(is_accept_resource_exhaustion(
            &io::Error::from_raw_os_error(libc::EMFILE,)
        ));
        assert!(is_accept_resource_exhaustion(
            &io::Error::from_raw_os_error(libc::ENFILE,)
        ));
    }

    #[test]
    fn queue_discards_expired_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let _client = TcpStream::connect(address).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let now = Instant::now();
        let mut streams = VecDeque::new();
        streams.push_back(WorkItem {
            stream,
            accepted_at: now - Duration::from_secs(2),
            deadline: now - Duration::from_secs(1),
        });
        let mut state = QueueState {
            streams,
            closed: false,
        };

        assert!(discard_expired(&mut state));
        assert!(state.streams.is_empty());
    }
}
