//! §3.5 Config and startup: missing/invalid config, env overrides.

use std::path::PathBuf;

use icrab::config::{self, ConfigError};

/// Missing config path → default config then validation fails with clear message.
#[test]
fn test_config_missing_file_fails_validation() {
    let missing = PathBuf::from("/nonexistent/icrab/config.toml");
    let result = config::load(&missing);

    let err = result.expect_err("load with missing path should fail");
    match &err {
        ConfigError::Validation(msg) => {
            assert!(
                msg.contains("workspace") || msg.contains("config"),
                "validation message should mention workspace or config: {}",
                msg
            );
        }
        _ => panic!("expected Validation error, got {:?}", err),
    }
}

/// Invalid TOML in config file → Parse error.
#[test]
fn test_config_invalid_toml_fails_parse() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(&path, "not valid toml {{{").unwrap();

    let result = config::load(&path);

    let err = result.expect_err("load with invalid TOML should fail");
    match &err {
        ConfigError::Parse(msg) => {
            assert!(!msg.is_empty());
        }
        _ => panic!("expected Parse error, got {:?}", err),
    }
}

/// TELEGRAM_BOT_TOKEN env override is applied when config has telegram section but empty token.
#[test]
fn test_config_env_override_telegram_token() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let ws = tmp.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let ws_str = ws.to_string_lossy();
    let config_content = format!(
        r#"
workspace = "{ws_str}"
[telegram]
bot-token = ""
allowed-user-ids = [1]
[llm]
provider = "openai"
api-key = "file_key"
model = "gpt-4"
"#
    );
    std::fs::write(&config_path, config_content).unwrap();

    // SAFETY: test only; we restore the var in RestoreEnv::drop.
    unsafe { std::env::set_var("TELEGRAM_BOT_TOKEN", "env_token_value") };
    let restore = RestoreEnv::new("TELEGRAM_BOT_TOKEN");

    let result = config::load(&config_path);
    drop(restore);

    let cfg = result.expect("load should succeed with env override");
    assert_eq!(
        cfg.telegram.as_ref().and_then(|t| t.bot_token.as_deref()),
        Some("env_token_value")
    );
}

/// Restore an env var to its previous value (or remove if was unset).
struct RestoreEnv {
    key: String,
    previous: Option<String>,
}

impl RestoreEnv {
    fn new(key: &str) -> Self {
        let previous = std::env::var(key).ok();
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        // SAFETY: restoring env to state before test.
        unsafe {
            match &self.previous {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}
