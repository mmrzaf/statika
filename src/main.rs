#[cfg(not(target_os = "linux"))]
compile_error!("statika currently supports Linux only");

mod config;
mod fs;
mod http;
mod net;
mod server;

use config::Config;

fn main() {
    if is_version_request() {
        println!("statika {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let config = Config::from_env().unwrap_or_else(|error| {
        eprintln!("configuration error: {error}");
        std::process::exit(2);
    });

    if let Err(error) = server::run(config) {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}

fn is_version_request() -> bool {
    let mut args = std::env::args_os();
    let _program = args.next();
    let first = args.next().and_then(|arg| arg.into_string().ok());
    matches!(first.as_deref(), Some("--version") | Some("-V")) && args.next().is_none()
}
