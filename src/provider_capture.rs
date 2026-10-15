//! Explicitly sensitive, default-off selected provider-error capture. Never a raw event dump.
use serde_json::{Value, json};
use std::{path::Path, sync::OnceLock};

pub(crate) const MAX_FIELD_INPUT: usize = 16 * 1024;
pub(crate) const MAX_FILE_BYTES: usize = 8 * 1024;
pub(crate) const MAX_FILES: usize = 16;
pub(crate) const MAX_TOTAL_BYTES: u64 = (MAX_FILES * MAX_FILE_BYTES) as u64;

/// Do not expose selected text through a derived Debug/Display or routine logging.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SelectedFields(Value);
impl std::fmt::Debug for SelectedFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[private provider-error fields]")
    }
}
impl SelectedFields {
    pub(crate) fn new(error: Option<&Value>) -> Self {
        let mut fields = serde_json::Map::new();
        for (name, limit) in [("code", 256), ("type", 256), ("message", 4096)] {
            let Some(field) = error.and_then(|e| e.get(name)) else {
                continue;
            };
            // Z.ai variants may use a numeric error code; do not reinterpret it for retries.
            if name == "code" && field.is_number() {
                fields.insert(name.into(), field.clone());
                continue;
            }
            let Some(value) = field.as_str() else {
                continue;
            };
            // Never copy arbitrary JSON objects, keys, headers, or the surrounding response.
            // Reject oversize text *before* truncating so a split token cannot evade redaction.
            let value = if value.len() > MAX_FIELD_INPUT {
                "[omitted: oversized field]".to_owned()
            } else {
                let redacted = redact(value);
                if redacted.len() > limit {
                    "[omitted: field exceeds capture limit]".to_owned()
                } else {
                    redacted
                }
            };
            fields.insert(name.into(), json!(value));
        }
        Self(Value::Object(fields))
    }
}

fn redact(text: &str) -> String {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            // URLs may contain credentials, signed query strings, or echoed request data.
            r#"(?i)(?:https?|wss?)://[^\s<>\"']+"#,
            r#"(?i)\b(?:authorization|proxy-authorization|cookie|set-cookie)\s*[:=][^\r\n]*"#,
            r#"(?i)\b(?:bearer|basic)\s+[a-z0-9+/_.=~:-]+"#,
            r#"(?i)\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|token|password|passwd|secret|client[_-]?secret)\b[\"']?\s*[:=]\s*(?:\"[^\"]*\"|'[^']*'|[^\s,;}]+)"#,
            r#"\b(?:sk-[A-Za-z0-9_-]+|gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+|AKIA[A-Z0-9]{16})\b"#,
            r#"\beyJ[A-Za-z0-9_-]*\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b"#,
            r#"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*"#,
            // Opaque high-entropy credentials often lack a recognizable prefix.
            r#"[A-Za-z0-9_+/=-]{32,}"#,
        ]
        .into_iter()
        .map(|pattern| regex::Regex::new(pattern).expect("static diagnostic redaction regex"))
        .collect()
    });
    let mut output: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    for pattern in patterns {
        output = pattern.replace_all(&output, "[redacted]").into_owned();
    }
    output
}

/// Fixed-schema summaries: never arbitrary property names or scalar values.
pub(crate) fn structure(event: &Value, error: Option<&Value>) -> Value {
    fn field(value: Option<&Value>, fingerprint: bool) -> Value {
        let (kind, length) = match value {
            None => ("missing", None),
            Some(Value::Null) => ("null", None),
            Some(Value::Bool(_)) => ("boolean", None),
            Some(Value::Number(_)) => ("number", None),
            Some(Value::String(s)) => ("string", Some(s.len())),
            Some(Value::Array(a)) => ("array", Some(a.len())),
            Some(Value::Object(o)) => ("object", Some(o.len())),
        };
        // Process-local secret salt prevents offline guessing/cross-restart tracking.
        // Hash only short code/type strings, never arbitrary message/prompt text.
        let digest = value
            .and_then(Value::as_str)
            .filter(|s| fingerprint && s.len() <= 256)
            .map(|s| {
                use sha2::{Digest, Sha256};
                static SALT: OnceLock<[u8; 32]> = OnceLock::new();
                let salt = SALT.get_or_init(|| {
                    let mut bytes = [0; 32];
                    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                    bytes
                });
                let mut hash = Sha256::new();
                hash.update(salt);
                hash.update(b"pantheon-error-field-v1\0");
                hash.update((s.len() as u64).to_be_bytes());
                hash.update(s.as_bytes());
                hex::encode(hash.finalize())
            });
        json!({"kind":kind,"length":length,"fingerprint":digest})
    }
    let mut fields = serde_json::Map::new();
    for name in ["code", "type", "message", "param", "status"] {
        fields.insert(
            name.into(),
            field(
                error.and_then(|e| e.get(name)),
                matches!(name, "code" | "type"),
            ),
        );
    }
    json!({"event":field(Some(event),false),"top_error":field(event.get("error"),false),
        "response_error":field(event.get("response").and_then(|r|r.get("error")),false),
        "selected_error":field(error,false),"fields":fields})
}

/// Return only a fixed outcome, never paths, OS error strings or captured text.
/// Capture failures cannot replace the original provider failure or alter retry policy.
pub(crate) fn save(
    directory: &Path,
    id: &str,
    metadata: Value,
    fields: &SelectedFields,
) -> &'static str {
    let value = json!({"schema_version":1,"diagnostic_id":id,"recorded_at":chrono::Utc::now().to_rfc3339(),
        "metadata":metadata,"selected_error":fields.0});
    let Ok(bytes) = serde_json::to_vec(&value) else {
        return "rejected";
    };
    if bytes.len() > MAX_FILE_BYTES {
        return "size_limit";
    }
    #[cfg(unix)]
    {
        storage::save(directory, id, &bytes).unwrap_or("unavailable")
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        "unsupported_platform"
    }
}

#[cfg(unix)]
mod storage {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        fs::{DirBuilder, File},
        io::{self, Write},
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        },
    };
    fn rejected() -> io::Error {
        io::Error::from(io::ErrorKind::PermissionDenied)
    }
    fn check(file: &File, directory: bool) -> io::Result<()> {
        let meta = file.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o077 != 0
            || (if directory {
                !meta.is_dir()
            } else {
                !meta.is_file() || meta.nlink() != 1
            })
        {
            return Err(rejected());
        }
        Ok(())
    }
    fn open_at(dir: &File, name: &CStr, flags: i32) -> io::Result<File> {
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    struct Entries(*mut libc::DIR);
    impl Drop for Entries {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    fn usage(dir: &File) -> io::Result<(usize, u64)> {
        // Use a descriptor-relative fresh directory description (not dup, whose offset is shared).
        let scan = open_at(dir, c".", libc::O_RDONLY | libc::O_DIRECTORY)?;
        let raw: RawFd = std::os::fd::IntoRawFd::into_raw_fd(scan);
        let ptr = unsafe { libc::fdopendir(raw) };
        if ptr.is_null() {
            unsafe {
                libc::close(raw);
            }
            return Err(io::Error::last_os_error());
        }
        let entries = Entries(ptr);
        usage_entries(dir, || next_name(&entries))
    }
    fn errno_pointer() -> io::Result<*mut libc::c_int> {
        #[cfg(any(target_os = "linux", target_os = "dragonfly"))]
        {
            Ok(unsafe { libc::__errno_location() })
        }
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            Ok(unsafe { libc::__error() })
        }
        #[cfg(any(target_os = "android", target_os = "openbsd", target_os = "netbsd"))]
        {
            Ok(unsafe { libc::__errno() })
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
        )))]
        {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
    fn next_name(entries: &Entries) -> io::Result<Option<CString>> {
        let errno = errno_pointer()?;
        // POSIX readdir returns NULL for both EOF and read failure. Clear errno
        // before each call, then fail closed rather than undercount on EIO/etc.
        unsafe {
            *errno = 0;
        }
        let entry = unsafe { libc::readdir(entries.0) };
        if entry.is_null() {
            let error = unsafe { *errno };
            return if error == 0 {
                Ok(None)
            } else {
                Err(io::Error::from_raw_os_error(error))
            };
        }
        Ok(Some(
            unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned(),
        ))
    }
    fn usage_entries(
        dir: &File,
        mut next: impl FnMut() -> io::Result<Option<CString>>,
    ) -> io::Result<(usize, u64)> {
        let mut count = 0;
        let mut total = 0u64;
        // Bound directory work even if someone inserts unexpected contents.
        while let Some(name) = next()? {
            if matches!(name.to_bytes(), b"." | b".." | b".lock") {
                continue;
            }
            count += 1;
            if count > MAX_FILES {
                return Ok((count, MAX_TOTAL_BYTES));
            }
            let name_text = name.to_str().map_err(|_| rejected())?;
            let id = name_text.strip_suffix(".json").ok_or_else(rejected)?;
            if uuid::Uuid::parse_str(id).is_err() {
                return Err(rejected());
            }
            let file = open_at(dir, &name, libc::O_RDONLY)?;
            check(&file, false)?;
            let size = file.metadata()?.len();
            if size > MAX_FILE_BYTES as u64 {
                return Err(rejected());
            }
            total = total.saturating_add(size);
        }
        Ok((count, total))
    }
    pub(super) fn save(directory: &Path, id: &str, bytes: &[u8]) -> io::Result<&'static str> {
        save_with_usage(directory, id, bytes, usage)
    }
    fn save_with_usage(
        directory: &Path,
        id: &str,
        bytes: &[u8],
        scan: impl FnOnce(&File) -> io::Result<(usize, u64)>,
    ) -> io::Result<&'static str> {
        // No caller-controlled filenames; the exchange owns this UUID.
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(rejected());
        }
        match DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
        let dir = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory)?;
        check(&dir, true)?;
        let lock = open_at(&dir, c".lock", libc::O_RDWR | libc::O_CREAT)?;
        check(&lock, false)?;
        if lock.metadata()?.len() != 0 {
            return Err(rejected());
        }
        // Never wait behind another writer in the provider failure path.
        if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
            return Ok("busy");
        }
        let (count, total) = scan(&dir)?;
        if count >= MAX_FILES || total.saturating_add(bytes.len() as u64) > MAX_TOTAL_BYTES {
            return Ok("full");
        }
        let name = CString::new(format!("{id}.json")).map_err(|_| rejected())?;
        let mut file = open_at(&dir, &name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
        check(&file, false)?;
        if file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .is_err()
        {
            unsafe {
                libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0);
            }
            return Err(io::Error::other("capture write failed"));
        }
        Ok("saved")
    }

    #[cfg(test)]
    mod scan_tests {
        use super::*;
        #[test]
        fn injected_directory_scan_failure_is_not_eof_and_admits_no_snapshot() {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("capture");
            let id = uuid::Uuid::new_v4().to_string();
            let error = save_with_usage(&dir, &id, b"{}", |dir| {
                let mut calls = 0;
                usage_entries(dir, || {
                    calls += 1;
                    if calls == 1 {
                        Ok(Some(CString::new(".").unwrap()))
                    } else {
                        Err(io::Error::from_raw_os_error(libc::EIO))
                    }
                })
            })
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            assert!(!dir.join(format!("{id}.json")).exists());
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1); // empty lock only
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_never_copies_values_or_arbitrary_keys() {
        let event = json!({"type":"error","private-key":"private-prompt","error":{
            "code":"private-token","type":"private-type","message":"private-message",
            "param":{"private-field":"private-param"},"private-secret-key":"private-value"},
            "response":{"output":[{"content":"private-response"}]}});
        let record = structure(&event, event.get("error"));
        assert_eq!(record["fields"]["code"]["kind"], "string");
        assert_eq!(record["fields"]["message"]["length"], 15);
        assert_eq!(
            record["fields"]["code"]["fingerprint"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(record, structure(&event, event.get("error")));
        assert!(!record.to_string().contains("private-"));
        assert!(!format!("{:?}", SelectedFields::new(event.get("error"))).contains("private-"));
    }
    #[test]
    fn selected_fields_recover_novel_explanation_but_redact_common_credentials() {
        let fields = SelectedFields::new(Some(
            &json!({"code":"new_error_code","type":"upstream_rejection",
            "message":"invalid item reference; bearer sensitive-token; api_key=private-api-value; password=private-password; https://u:pw@test.invalid/?token=secret; sk-privateApiKey123; eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature; cookie: private-cookie",
            "headers":{"Authorization":"private-header"}, "output":"private-prompt"}),
        ));
        let text = fields.0.to_string();
        assert!(text.contains("new_error_code"));
        assert!(text.contains("invalid item reference"));
        for secret in [
            "sensitive-token",
            "private-",
            "test.invalid",
            "eyJhbGci",
            "signature",
            "https://",
        ] {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
        assert_eq!(fields.0.as_object().unwrap().len(), 3);
    }
    #[test]
    fn unicode_oversize_and_nonstring_fields_are_bounded_without_slicing_tokens() {
        let fields = SelectedFields::new(Some(&json!({"code":"é".repeat(MAX_FIELD_INPUT),
            "type":{"secret":"never-copy"},"message":"🦀".repeat(2000)})));
        assert_eq!(fields.0["code"], "[omitted: oversized field]");
        assert!(fields.0.get("type").is_none());
        assert_eq!(
            fields.0["message"],
            "[omitted: field exceeds capture limit]"
        );
        for value in [json!(null), json!(42), json!(["secret"]), json!("secret")] {
            assert_eq!(SelectedFields::new(Some(&value)).0, json!({}));
        }
    }
    #[cfg(unix)]
    fn capture(directory: &Path) -> &'static str {
        save(
            directory,
            &uuid::Uuid::new_v4().to_string(),
            json!({"event":"error"}),
            &SelectedFields::new(Some(
                &json!({"code":"novel_error","message":"recover this reason"}),
            )),
        )
    }
    #[test]
    #[cfg(unix)]
    fn storage_has_private_permissions_and_stops_at_count_bound() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("capture");
        for _ in 0..MAX_FILES {
            assert_eq!(capture(&dir), "saved");
        }
        assert_eq!(capture(&dir), "full");
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let mut total = 0;
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap();
            let meta = file.metadata().unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            if file.file_name() != ".lock" {
                assert!(meta.len() <= MAX_FILE_BYTES as u64);
                total += meta.len();
                let record: Value =
                    serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
                assert_eq!(record["selected_error"]["message"], "recover this reason");
            }
        }
        assert!(total <= MAX_TOTAL_BYTES);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), MAX_FILES + 1);
    }
    #[test]
    #[cfg(unix)]
    fn storage_rejects_symlinks_insecure_permissions_unexpected_files_and_busy_lock() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("capture");
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &dir).unwrap();
        assert_eq!(capture(&dir), "unavailable");
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        std::fs::remove_file(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(capture(&dir), "unavailable");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let secret = temp.path().join("secret");
        std::fs::write(&secret, "unchanged").unwrap();
        symlink(&secret, dir.join(".lock")).unwrap();
        assert_eq!(capture(&dir), "unavailable");
        std::fs::remove_file(dir.join(".lock")).unwrap();
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "unchanged");
        assert_eq!(capture(&dir), "saved");
        let lock = std::fs::File::options()
            .read(true)
            .write(true)
            .open(dir.join(".lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        assert_eq!(capture(&dir), "busy");
        fs2::FileExt::unlock(&lock).unwrap();
        std::fs::write(dir.join("unexpected"), "do not delete").unwrap();
        assert_eq!(capture(&dir), "unavailable");
        std::fs::remove_file(dir.join("unexpected")).unwrap();
        symlink(&secret, dir.join(format!("{}.json", uuid::Uuid::new_v4()))).unwrap();
        assert_eq!(capture(&dir), "unavailable");
        assert_eq!(
            capture(&temp.path().join("missing-parent/child")),
            "unavailable"
        );
    }
    #[test]
    #[cfg(unix)]
    fn storage_size_limit_and_parallel_writers_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("capture");
        assert_eq!(
            save(
                &dir,
                &uuid::Uuid::new_v4().to_string(),
                json!({"x":"x".repeat(MAX_FILE_BYTES)}),
                &SelectedFields(json!({}))
            ),
            "size_limit"
        );
        assert!(!dir.exists());
        std::thread::scope(|scope| {
            for _ in 0..32 {
                let dir = &dir;
                scope.spawn(move || {
                    assert!(["saved", "busy", "full"].contains(&capture(dir)));
                });
            }
        });
        assert!(std::fs::read_dir(&dir).unwrap().count() <= MAX_FILES + 1);
    }
    #[test]
    #[cfg(unix)]
    fn unsafe_existing_snapshots_and_duplicate_ids_cannot_overwrite_evidence() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("capture");
        let id = uuid::Uuid::new_v4().to_string();
        let fields = SelectedFields::new(Some(&json!({"message":"original reason"})));
        assert_eq!(save(&dir, &id, json!({}), &fields), "saved");
        let path = dir.join(format!("{id}.json"));
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            save(
                &dir,
                &id,
                json!({}),
                &SelectedFields(json!({"message":"replacement"}))
            ),
            "unavailable"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(capture(&dir), "unavailable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let hardlink = temp.path().join("hardlink");
        std::fs::hard_link(&path, &hardlink).unwrap();
        assert_eq!(capture(&dir), "unavailable");
        std::fs::remove_file(&hardlink).unwrap();
        std::fs::write(&path, vec![b'x'; MAX_FILE_BYTES + 1]).unwrap();
        assert_eq!(capture(&dir), "unavailable");
    }
}
