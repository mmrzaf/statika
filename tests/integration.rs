use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fixture() -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("statika-it-{}-{suffix}", std::process::id()));
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::write(root.join("index.html"), b"INDEX").unwrap();
    fs::write(root.join("assets/app.js"), b"plain-app").unwrap();
    fs::write(root.join("assets/app.js.gz"), b"gzip-app").unwrap();
    fs::write(root.join("assets/app.0123abcd.js"), b"hashed-app").unwrap();
    root
}

fn spawn_server(root: &Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_statika"))
        .env("STATIKA_ROOT", root)
        .env("STATIKA_LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .env("STATIKA_THREADS", "2")
        .env("STATIKA_QUEUE_SIZE", "4")
        .env("STATIKA_GZIP", "1")
        .env("STATIKA_REQUEST_TIMEOUT_SECS", "1")
        .env("STATIKA_SHUTDOWN_TIMEOUT_SECS", "3")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_ready(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "server did not become ready");
        thread::sleep(Duration::from_millis(20));
    }
}

fn request(addr: SocketAddr, request: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    (
        String::from_utf8_lossy(&response[..split]).into_owned(),
        response[split + 4..].to_vec(),
    )
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}: ");
    headers
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
}

fn stop(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "server exited unsuccessfully: {status}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "server did not stop after SIGTERM"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serves_health_spa_assets_head_gzip_and_etag() {
    let root = fixture();
    let port = free_port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut child = spawn_server(&root, port);
    wait_ready(addr);

    let (head, body) = request(
        addr,
        "GET /health?probe=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(body, b"ok\n");

    let (head, body) = request(
        addr,
        "GET /assets/app.js HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip;q=1.0\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert!(head.contains("Content-Encoding: gzip"));
    assert!(head.contains("Cache-Control: public, max-age=3600"));
    assert_eq!(body, b"gzip-app");
    let etag = header(&head, "ETag").unwrap().to_owned();

    let (head, body) = request(
        addr,
        &format!(
            concat!(
                "GET /assets/app.js HTTP/1.1\r\n",
                "Host: localhost\r\n",
                "Accept-Encoding: gzip;q=1.0\r\n",
                "If-None-Match: {}\r\n",
                "\r\n"
            ),
            etag
        ),
    );
    assert!(head.starts_with("HTTP/1.1 304 Not Modified"));
    assert_eq!(header(&head, "Content-Length"), Some("8"));
    assert!(body.is_empty());

    let (head, body) = request(
        addr,
        "HEAD /assets/app.js HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(header(&head, "Content-Length"), Some("9"));
    assert!(body.is_empty());

    let (head, body) = request(
        addr,
        "GET /assets/app.0123abcd.js HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(head.contains("Cache-Control: public, max-age=31536000, immutable"));
    assert_eq!(body, b"hashed-app");

    let (head, body) = request(
        addr,
        "GET /application/route HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert!(head.contains("Cache-Control: no-cache"));
    assert_eq!(body, b"INDEX");

    stop(&mut child);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_asset_fallback_symlinks_and_unsupported_methods() {
    let root = fixture();
    let outside = root.parent().unwrap().join(format!(
        "{}-outside.txt",
        root.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&outside, b"secret").unwrap();
    symlink(&outside, root.join("assets/escape.txt")).unwrap();

    let port = free_port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut child = spawn_server(&root, port);
    wait_ready(addr);

    let (head, body) = request(
        addr,
        "GET /assets/missing.js HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 404 Not Found"));
    assert_eq!(body, b"not found\n");

    let (head, body) = request(
        addr,
        "GET /assets/escape.txt HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 404 Not Found"));
    assert_eq!(body, b"not found\n");

    let (head, _) = request(addr, "POST /health HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 405 Method Not Allowed"));
    assert!(head.contains("Allow: GET, HEAD"));

    stop(&mut child);
    fs::remove_dir_all(root).unwrap();
    fs::remove_file(outside).unwrap();
}

#[test]
fn sigterm_stops_idle_server_cleanly() {
    let root = fixture();
    let port = free_port();
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut child = spawn_server(&root, port);
    wait_ready(addr);
    stop(&mut child);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_invalid_configuration() {
    let status = Command::new(env!("CARGO_BIN_EXE_statika"))
        .env("STATIKA_ROOT", std::env::temp_dir())
        .env("STATIKA_GZIP", "flase")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}
