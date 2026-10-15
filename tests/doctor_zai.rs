//! Offline diagnostics use only synthetic environment and a temporary config.
use std::process::Command;

#[test]
fn doctor_checks_explicit_curator_credentials_only_when_enabled() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    for (enabled, key_present) in [(true, false), (true, true), (false, false)] {
        std::fs::write(&path, format!(
            "workspace={:?}\nstate_dir={:?}\n[discord]\napplication_id=1\nallowed_users=[2]\n[agent]\nmodel='openai/test'\ncompactor_model='openai/test'\n[curator]\nenabled={enabled}\nmodel='zai/glm-5.3'\n[zai]\nenabled=true\n",
            root.path(), root.path().join("state"),
        )).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_pantheon"));
        command
            .env_clear()
            .env("PATH", "")
            .env("OPENAI_API_KEY", "synthetic-openai-do-not-print")
            .env("DISCORD_TOKEN", "synthetic-discord-do-not-print")
            .arg("--config")
            .arg(&path)
            .arg("doctor");
        if key_present {
            command.env("ZAI_API_KEY", "synthetic-zai-do-not-print");
        }
        let output = command.output().unwrap();
        // Browser executables intentionally absent, so doctor is still a failure.
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            error.contains("ZAI_API_KEY"),
            enabled && !key_present,
            "{error}"
        );
        assert!(!error.contains("do-not-print"));
        assert!(!error.contains("ANTHROPIC_API_KEY"));
    }
}
