//! Admission must be read-only on incompatible state, before gateway/DB/jobs.
use pantheon::{config::Config, discord::Discord, runtime::Harness, state_namespace};
use std::{collections::BTreeMap, fs, path::Path, process::Command, sync::Arc};
use tokio_util::sync::CancellationToken;

fn snapshot(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, out: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.insert(path.strip_prefix(root).unwrap().into(), vec![]);
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().into(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[tokio::test]
async fn cli_and_direct_harness_refuse_incompatible_namespaces_before_writes() {
    for case in 0..5 {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        fs::create_dir(&state).unwrap();
        match case {
            0 => {
                fs::write(state.join("runtime.sqlite"), b"opaque DB").unwrap();
            }
            1 => {
                fs::write(state.join("state-format"), b"unsupported schema\n").unwrap();
            }
            2 | 3 => {
                state_namespace::initialize(&state).unwrap();
                let collection = if case == 2 { "chats" } else { "subagents" };
                // A valid earlier bank must not hide a later incompatible idle bank.
                pantheon::memory::Memory::open(state.join(collection).join("1"), 128000).unwrap();
                let bad = state.join(collection).join("2");
                fs::create_dir(&bad).unwrap();
                fs::write(bad.join("opaque"), "incompatible bank").unwrap();
                fs::write(state.join("runtime.sqlite"), b"do not open this DB").unwrap();
            }
            4 => {
                let bad = state.join("chats/1");
                fs::create_dir_all(&bad).unwrap();
                fs::write(bad.join("opaque"), "unmarked bank").unwrap();
            }
            _ => unreachable!(),
        }
        let before = snapshot(&state);
        let workspace = temp.path().join("workspace-must-not-be-created");
        let config = temp.path().join("config.toml");
        fs::write(&config, format!(
            "state_dir={state:?}\nworkspace={workspace:?}\n[discord]\napplication_id=1\nallowed_users=[2]\n"
        )).unwrap();
        for action in ["run", "export", "import", "models"] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_pantheon"));
            command
                .env_clear()
                .env("PATH", "")
                .arg("--config")
                .arg(&config)
                .arg(action);
            if action == "export" {
                command.arg("--output").arg(temp.path().join("out.html"));
            }
            if action == "import" {
                command.arg("--file").arg(temp.path().join("absent.txt"));
            }
            let output = command.output().unwrap();
            assert!(!output.status.success(), "case {case}/{action}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains("schema") || error.contains("native"),
                "case {case}/{action}: {error}"
            );
            assert_eq!(before, snapshot(&state), "case {case}/{action} wrote state");
            assert!(!workspace.exists());
        }
        let config = Config {
            state_dir: state.clone(),
            workspace: workspace.clone(),
            ..Default::default()
        };
        let discord = Arc::new(Discord::new("synthetic".into(), 1, vec![2]).unwrap());
        assert!(Harness::new(config, discord, CancellationToken::new()).is_err());
        assert_eq!(
            before,
            snapshot(&state),
            "direct Harness case {case} wrote state"
        );
        assert!(!workspace.exists());
    }
}

#[test]
fn daemon_lock_and_native_restart_remain_single_writer() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let lock = state_namespace::acquire(&state).unwrap();
    assert!(state_namespace::acquire(&state).is_err());
    drop(lock);
    let reopened = state_namespace::acquire(&state).unwrap();
    state_namespace::validate(&state).unwrap();
    drop(reopened);
}

#[test]
fn offline_models_first_command_initializes_a_reopenable_native_namespace() {
    for existing_empty in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        if existing_empty {
            fs::create_dir(&state).unwrap();
        }
        let config = temp.path().join("config.toml");
        fs::write(&config, format!(
            "state_dir={state:?}\nworkspace={:?}\n[discord]\napplication_id=1\nallowed_users=[2]\n",
            temp.path().join("workspace")
        )).unwrap();
        for _ in 0..2 {
            // No credentials and empty environment: no provider calls are made.
            let output = Command::new(env!("CARGO_BIN_EXE_pantheon"))
                .env_clear()
                .env("PATH", "")
                .args(["--config"])
                .arg(&config)
                .arg("models")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let lock = state_namespace::acquire(&state).unwrap();
            assert!(state.join("state-format").is_file());
            assert!(state.join("model-catalog.json").is_file());
            drop(lock);
        }
    }
}
