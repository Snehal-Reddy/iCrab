//! Build script for icrab.
//!
//! Emits `cargo:rustc-cfg` for target so code can use `#[cfg(target_ish)]` etc. if needed.
//!
//! Default target is iSH (i686-unknown-linux-musl); build with `cargo build --release`.

use std::env;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    if target == "i686-unknown-linux-musl" {
        println!("cargo:rustc-cfg=target_ish");
    }
    println!("cargo:rustc-cfg=static_linking");
}
