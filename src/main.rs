#[cfg(not(target_os = "linux"))]
compile_error!("statika currently supports Linux only");

mod config;
mod fs;
mod http;
mod net;
mod server;

use config::Config;

fn main() {
    let config = Config::from_env().unwrap_or_else(|error| {
        eprintln!("configuration error: {error}");
        std::process::exit(2);
    });

    if let Err(error) = server::run(config) {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}
