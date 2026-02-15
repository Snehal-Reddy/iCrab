# iCrab

Minimal openClaw style personal AI assistant that runs on an old iPhone as a server. Telegram-only; workspace-backed agent with tools.

- **Target:** `i686-unknown-linux-musl` for iSH (static binary).

## Building for iSH

The supported way to build the i686 binary is [cross](https://github.com/cross-rs/cross) (Docker + Rust cross-compilation):

1. Install cross: `cargo install cross`
2. Ensure Docker is running.
3. Build:
   - Debug: `cross build` or `./build.sh`
   - Release: `cross build --release` or `./build.sh --release`

Output: `target/i686-unknown-linux-musl/debug/icrab` or `.../release/icrab`.

