//! iCrab— minimal personal AI assistant for iSH; Telegram-only.
//!
//! Single binary: runs Telegram poller + agent loop. Config: `~/.moltbot/config.toml` or env.

fn main() {
    eprintln!("icrab {}", env!("CARGO_PKG_VERSION"));
    // TODO: load config, start poller + agent
}
