use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub fn hash(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}
pub fn plain_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "symlink forbidden: {}",
        path.display()
    );
    ensure!(meta.is_file() || meta.is_dir(), "special file forbidden");
    ensure!(
        !meta.is_file() || meta.nlink() == 1,
        "hardlinked file forbidden"
    );
    if meta.is_dir() {
        for e in fs::read_dir(path)? {
            plain_tree(&e?.path())?;
        }
    }
    Ok(())
}
pub fn plain_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if ancestor.exists() {
            let m = fs::symlink_metadata(ancestor)?;
            ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "non-directory/symlink ancestor"
            );
        }
    }
    Ok(())
}
pub fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
pub fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    sync_dir(path.parent().context("directory has no parent")?)
}
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    #[cfg(test)]
    if FAIL_NEXT
        .with(|target| target.borrow().as_deref() == path.file_name().and_then(|s| s.to_str()))
    {
        FAIL_NEXT.with(|target| *target.borrow_mut() = None);
        file.write_all(&bytes[..bytes.len() / 2])?;
        anyhow::bail!("injected partial write failure");
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    sync_dir(path.parent().context("file has no parent")?)
}
pub fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("next");
    if temp.exists() {
        plain_tree(&temp)?;
        fs::remove_file(&temp)?;
    }
    write_new(&temp, bytes)?;
    fs::rename(&temp, path)?;
    sync_dir(path.parent().context("file has no parent")?)
}
pub fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(value)?)
}
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    plain_tree(source)?;
    mkdir(dest)?;
    for e in fs::read_dir(source)? {
        let e = e?;
        let to = dest.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            write_new(&to, &fs::read(e.path())?)?;
        }
    }
    sync_dir(dest)
}
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub sha256: String,
    pub bytes: u64,
}
pub type Inventory = BTreeMap<String, Entry>;
pub fn inventory(path: &Path) -> Result<Inventory> {
    fn walk(root: &Path, at: &Path, out: &mut Inventory) -> Result<()> {
        for e in fs::read_dir(at)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                walk(root, &e.path(), out)?;
            } else {
                let rel = e
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .context("non-UTF8 output path")?
                    .to_owned();
                let bytes = fs::read(e.path())?;
                out.insert(
                    rel,
                    Entry {
                        sha256: hash(&bytes),
                        bytes: bytes.len() as u64,
                    },
                );
            }
        }
        Ok(())
    }
    plain_tree(path)?;
    let mut out = BTreeMap::new();
    walk(path, path, &mut out)?;
    Ok(out)
}
pub fn verify_inventory(path: &Path, expected: &Inventory) -> Result<()> {
    ensure!(
        &inventory(path)? == expected,
        "checkpoint/output inventory mismatch"
    );
    Ok(())
}
pub fn sibling(output: &Path, suffix: &str) -> Result<PathBuf> {
    let name = output
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid output name")?;
    Ok(output.with_file_name(format!(".{name}.{suffix}")))
}

#[cfg(test)]
thread_local! { static FAIL_NEXT:std::cell::RefCell<Option<String>>=const {std::cell::RefCell::new(None)}; }
#[cfg(test)]
pub(crate) fn fail_next_write(name: &str) {
    FAIL_NEXT.with(|target| *target.borrow_mut() = Some(name.to_owned()));
}
