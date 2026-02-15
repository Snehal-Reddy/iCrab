//! iCrab— minimal personal AI assistant for iSH; Telegram-only.
//!
//! Single binary: runs Telegram poller + agent loop. Config: `~/.icrab/config.toml` or env.

use icrab::config;

fn main() {
    eprintln!("icrab {}", env!("CARGO_PKG_VERSION"));
    let path = config::default_config_path();
    let cfg = match config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!("workspace: {}", cfg.workspace_path());
    // TODO: start Telegram poller + agent loop
}
