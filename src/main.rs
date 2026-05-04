use std::process;

use statika::{run, Config};

fn main() {
    match Config::from_env() {
        Ok(cfg) => {
            if let Err(err) = run(cfg) {
                eprintln!("{err}");
                process::exit(1);
            }
        }
        Err(err) => {
            eprintln!("{err}");
            process::exit(1);
        }
    }
}
