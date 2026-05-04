use std::fs::{self};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_server(root: &PathBuf, port: u16) -> Child {
    let test_bin = std::env::current_exe().expect("test exe");
    let profile_dir = test_bin
        .parent()
        .and_then(|p| p.parent())
        .expect("profile dir");
    let bin = profile_dir.join("statika");

    Command::new(bin)
        .env("STATIKA_ROOT", root)
        .env("STATIKA_LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .env("STATIKA_THREADS", "2")
        .env("STATIKA_QUEUE_SIZE", "4")
        .env("STATIKA_GZIP", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server")
}

fn wait_ready(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("server did not become ready");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn request(addr: SocketAddr, req: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    stream.write_all(req.as_bytes()).expect("write req");
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read resp");
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("headers");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let body = buf[split + 4..].to_vec();
    (head, body)
}

#[test]
fn end_to_end_serving_health_assets_and_fallback() {
    let mut root = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("statika-it-{}-{}", std::process::id(), nanos));
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::write(root.join("index.html"), b"INDEX").unwrap();
    fs::write(root.join("assets/app.js"), b"console.log('app');").unwrap();
    fs::write(root.join("assets/app.js.gz"), b"gz-app-bytes").unwrap();

    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut child = spawn_server(&root, port);
    wait_ready(addr);

    let (health_head, health_body) =
        request(addr, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(health_head.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(health_body, b"ok\n");

    let (asset_head, asset_body) = request(
        addr,
        "GET /assets/app.js HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip, deflate\r\n\r\n",
    );
    assert!(asset_head.starts_with("HTTP/1.1 200 OK"));
    assert!(asset_head.contains("Content-Encoding: gzip"));
    assert!(asset_head.contains("Cache-Control: public, max-age=31536000, immutable"));
    assert_eq!(asset_body, b"gz-app-bytes");

    let (fallback_head, fallback_body) = request(
        addr,
        "GET /missing/path HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(fallback_head.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(fallback_body, b"INDEX");

    let (method_head, _) = request(addr, "POST /health HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(method_head.starts_with("HTTP/1.1 405 Method Not Allowed"));
    assert!(method_head.contains("Allow: GET"));

    let _ = child.kill();
    let _ = child.wait();
}
