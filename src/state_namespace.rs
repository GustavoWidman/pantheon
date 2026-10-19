//! Native state admission. This is schema validation, never format conversion.
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

const MARKER: &str = "pantheon-native-state-v1\n";
const NAME: &str = "state-format";

/// Read-only preflight, before opening locks, SQLite, gateways, or workers.
/// A lone daemon.lock is an interrupted fresh bootstrap, not durable state.
pub fn validate(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir(),
        "state namespace must be a real directory"
    );
    let marker = path.join(NAME);
    if marker.try_exists()? {
        ensure!(
            fs::symlink_metadata(&marker)?.file_type().is_file()
                && fs::read_to_string(&marker)? == MARKER,
            "unsupported state namespace schema; use a fresh native state directory"
        );
    } else {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            ensure!(
                entry.file_name() == "daemon.lock" && entry.file_type()?.is_file(),
                "state namespace is nonempty without a native schema marker; use a fresh native state directory (external conversion is separate)"
            );
        }
    }
    // Validate idle banks too: startup must not run queued effects while any
    // bank would fail its lazy channel/subagent open. Never repair indexes here.
    for name in ["chats", "subagents"] {
        let directory = path.join(name);
        if directory.try_exists()? {
            ensure!(
                fs::symlink_metadata(&directory)?.is_dir(),
                "native bank namespace must be a real directory"
            );
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                crate::memory::Memory::validate_namespace(entry.path())
                    .with_context(|| format!("invalid native bank in {name}"))?;
            }
        }
    }
    Ok(())
}

/// Admit, acquire the daemon's lifetime lock, and revalidate before any state
/// initialization. The caller keeps the returned file alive until shutdown.
pub fn acquire(path: &Path) -> Result<File> {
    validate(path)?;
    durable_directory(path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.join("daemon.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context(
        "another Pantheon process owns this state directory; stop it before offline export/import",
    )?;
    initialize(path)?;
    Ok(lock)
}

/// Initialize only after read-only admission. The daemon calls this under its
/// lifetime writer lock; embedded Harness users retain their existing lock duty.
pub fn initialize(path: &Path) -> Result<()> {
    validate(path)?;
    durable_directory(path)?;
    if !path.join(NAME).try_exists()? {
        let mut marker = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path.join(NAME))?;
        marker.write_all(MARKER.as_bytes())?;
        marker.sync_all()?;
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

fn durable_directory(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        durable_directory(parent)?;
    }
    fs::create_dir(path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(parent)?.sync_all()?;
    }
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_and_native_restart_are_admitted() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("nested/state");
        validate(&state).unwrap();
        assert!(!state.exists());
        initialize(&state).unwrap();
        crate::memory::Memory::open(state.join("chats/1"), 128000).unwrap();
        fs::write(state.join("runtime.sqlite"), b"synthetic operational state").unwrap();
        validate(&state).unwrap();
        initialize(&state).unwrap();
        assert_eq!(fs::read_to_string(state.join(NAME)).unwrap(), MARKER);
    }

    #[test]
    fn nonempty_unmarked_namespace_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path();
        fs::write(state.join("runtime.sqlite"), b"opaque incompatible state").unwrap();
        assert!(initialize(state).is_err());
        assert_eq!(fs::read_dir(state).unwrap().count(), 1);
        assert_eq!(
            fs::read(state.join("runtime.sqlite")).unwrap(),
            b"opaque incompatible state"
        );
    }

    #[test]
    fn wrong_marker_and_idle_bank_fail_without_repairs() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path();
        fs::write(state.join(NAME), "another schema\n").unwrap();
        assert!(initialize(state).is_err());
        assert_eq!(
            fs::read_to_string(state.join(NAME)).unwrap(),
            "another schema\n"
        );
        fs::write(state.join(NAME), MARKER).unwrap();
        for collection in ["chats", "subagents"] {
            let bank = state.join(collection).join("idle");
            fs::create_dir_all(&bank).unwrap();
            fs::write(bank.join("opaque"), "not native").unwrap();
            assert!(initialize(state).is_err());
            assert_eq!(fs::read_dir(&bank).unwrap().count(), 1);
            fs::remove_dir_all(state.join(collection)).unwrap();
        }
    }

    #[test]
    fn interrupted_empty_bootstrap_is_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("daemon.lock"), "").unwrap();
        initialize(temp.path()).unwrap();
        validate(temp.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn namespace_marker_and_banks_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        initialize(&state).unwrap();
        let target = temp.path().join("target");
        fs::write(&target, MARKER).unwrap();
        fs::remove_file(state.join(NAME)).unwrap();
        symlink(&target, state.join(NAME)).unwrap();
        assert!(validate(&state).is_err());
        fs::remove_file(state.join(NAME)).unwrap();
        initialize(&state).unwrap();
        fs::create_dir(state.join("chats")).unwrap();
        let bank = temp.path().join("bank");
        crate::memory::Memory::open(&bank, 128000).unwrap();
        symlink(bank, state.join("chats/1")).unwrap();
        assert!(validate(&state).is_err());
    }
}
