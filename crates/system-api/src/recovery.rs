// SPDX-License-Identifier: GPL-3.0-only
use crate::{now_ms, random_token, ManagedFile, RecoveryRecord};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    io::{Read, Write},
};

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TRANSACTIONS: usize = 128;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
use windows::{atomic_write_at, lock_transaction, read_at, remove_at};

#[derive(Clone)]
pub struct RecoveryStore {
    root: Arc<PathBuf>,
    inner: Arc<Mutex<Store>>,
}
struct Store {
    root: PathBuf,
    directory: File,
    journal: File,
    transaction_lock: File,
}

impl RecoveryStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
            ensure!(root.is_absolute(), "managed directory must be absolute");
            let mut component = PathBuf::new();
            for part in root.components() {
                component.push(part);
                match std::fs::symlink_metadata(&component) {
                    Ok(metadata) => ensure!(
                        !metadata.file_type().is_symlink(),
                        "managed directory contains a symbolic link"
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error.into()),
                }
            }
            std::fs::create_dir_all(&root)?;
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&root)?;
            ensure!(
                directory.metadata()?.uid() == unsafe { libc::geteuid() },
                "managed directory belongs to another user"
            );
            directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            use std::os::fd::AsRawFd;
            let journal_name = std::ffi::CString::new(".glass-journal")?;
            let created =
                unsafe { libc::mkdirat(directory.as_raw_fd(), journal_name.as_ptr(), 0o700) };
            if created != 0 {
                ensure!(
                    std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists,
                    "cannot create recovery journal"
                );
            }
            let journal = open_at(
                &directory,
                ".glass-journal",
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )?
            .context("recovery journal unavailable")?;
            ensure!(
                journal.metadata()?.uid() == unsafe { libc::geteuid() },
                "recovery journal belongs to another user"
            );
            journal.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            let transaction_lock = open_at(
                &journal,
                "transaction.lock",
                libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
                0o600,
            )?
            .context("workspace transaction lock unavailable")?;
            let metadata = transaction_lock.metadata()?;
            ensure!(
                metadata.is_file()
                    && metadata.nlink() == 1
                    && metadata.uid() == unsafe { libc::geteuid() },
                "workspace transaction lock has an invalid identity"
            );
            Ok(Self {
                root: Arc::new(root.clone()),
                inner: Arc::new(Mutex::new(Store {
                    root,
                    directory,
                    journal,
                    transaction_lock,
                })),
            })
        }
        #[cfg(windows)]
        {
            let store = windows::open_store(root.clone())?;
            Ok(Self {
                root: Arc::new(root),
                inner: Arc::new(Mutex::new(store)),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = root;
            anyhow::bail!("controlled file recovery requires the native Unix backend")
        }
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub fn files(&self) -> Result<Vec<ManagedFile>> {
        let store = self.inner.lock().unwrap();
        let mut files = Vec::new();
        for name in directory_names(&store.directory, &store.root, 1024)? {
            if validate_name(&name).is_err() {
                continue;
            }
            if let Ok(Some(bytes)) = read_at(&store.directory, &name, MAX_FILE_BYTES) {
                files.push(ManagedFile {
                    path: name,
                    bytes: bytes.len() as u64,
                });
            }
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    pub fn read(&self, path: &str) -> Result<String> {
        validate_name(path)?;
        let store = self.inner.lock().unwrap();
        let bytes = read_at(&store.directory, path, MAX_FILE_BYTES)?
            .context("managed file does not exist")?;
        String::from_utf8(bytes).context("managed editor accepts UTF-8 text")
    }

    pub fn edit(&self, path: &str, contents: &[u8]) -> Result<RecoveryRecord> {
        validate_name(path)?;
        ensure!(
            contents.len() <= MAX_FILE_BYTES,
            "managed edit exceeds 1 MiB"
        );
        let store = self.inner.lock().unwrap();
        let _transaction = lock_transaction(&store.transaction_lock)?;
        ensure!(
            records_locked(&store)?.len() < MAX_TRANSACTIONS,
            "managed snapshot budget reached 128 transactions"
        );
        let before = read_at(&store.directory, path, MAX_FILE_BYTES)?;
        let id = random_token()?;
        let mut record = RecoveryRecord {
            id: id.clone(),
            path: path.into(),
            before_sha256: before.as_deref().map(hash),
            after_sha256: hash(contents),
            status: "prepared".into(),
            created_at_ms: now_ms(),
        };
        if let Some(before) = before.as_deref() {
            atomic_write_at(&store.journal, &format!("{id}.before"), before)?;
        }
        write_record(&store, &record)?;
        let current = read_at(&store.directory, path, MAX_FILE_BYTES)?;
        ensure!(
            current.as_deref().map(hash) == record.before_sha256,
            "managed file changed while its snapshot was being prepared"
        );
        atomic_write_at(&store.directory, path, contents)?;
        record.status = "applied".into();
        write_record(&store, &record)?;
        Ok(record)
    }

    pub fn records(&self) -> Result<Vec<RecoveryRecord>> {
        let store = self.inner.lock().unwrap();
        let _transaction = lock_transaction(&store.transaction_lock)?;
        records_locked(&store)
    }

    pub fn rollback(&self, id: &str) -> Result<RecoveryRecord> {
        validate_id(id)?;
        let store = self.inner.lock().unwrap();
        let _transaction = lock_transaction(&store.transaction_lock)?;
        let bytes = read_at(&store.journal, &format!("{id}.json"), 16_384)?
            .context("recovery transaction does not exist")?;
        let mut record: RecoveryRecord = serde_json::from_slice(&bytes)?;
        validate_name(&record.path)?;
        ensure!(
            record.id == id
                && ["applied", "prepared", "rollback_prepared", "rolled_back"]
                    .contains(&record.status.as_str()),
            "transaction cannot be rolled back"
        );
        let current = read_at(&store.directory, &record.path, MAX_FILE_BYTES)?;
        let current_hash = current.as_deref().map(hash);
        if ["rollback_prepared", "rolled_back"].contains(&record.status.as_str())
            && current_hash == record.before_sha256
        {
            record.status = "rolled_back".into();
            write_record(&store, &record)?;
            return Ok(record);
        }
        ensure!(
            record.status != "rolled_back",
            "rollback conflict: file changed after the completed restore"
        );
        ensure!(
            current_hash.as_deref() == Some(&record.after_sha256),
            "rollback conflict: managed file changed after the recorded edit"
        );
        record.status = "rollback_prepared".into();
        write_record(&store, &record)?;
        if let Some(expected) = &record.before_sha256 {
            let before = read_at(&store.journal, &format!("{id}.before"), MAX_FILE_BYTES)?
                .context("recovery snapshot is missing")?;
            ensure!(
                hash(&before) == *expected,
                "recovery snapshot integrity check failed"
            );
            atomic_write_at(&store.directory, &record.path, &before)?;
        } else {
            remove_at(&store.directory, &record.path)?;
        }
        record.status = "rolled_back".into();
        write_record(&store, &record)?;
        Ok(record)
    }
}

fn records_locked(store: &Store) -> Result<Vec<RecoveryRecord>> {
    let mut records = Vec::new();
    for name in directory_names(
        &store.journal,
        &store.root.join(".glass-journal"),
        MAX_TRANSACTIONS * 3,
    )? {
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if validate_id(id).is_err() {
            continue;
        }
        let bytes =
            read_at(&store.journal, &name, 16_384)?.context("recovery metadata disappeared")?;
        let mut record: RecoveryRecord = serde_json::from_slice(&bytes)?;
        ensure!(
            record.id == id,
            "recovery journal identifier does not match its file"
        );
        validate_name(&record.path)?;
        if record.status == "prepared" || record.status == "rollback_prepared" {
            let current = read_at(&store.directory, &record.path, MAX_FILE_BYTES)?;
            let current_hash = current.as_deref().map(hash);
            record.status =
                if record.status == "rollback_prepared" && current_hash == record.before_sha256 {
                    "rolled_back"
                } else if current_hash.as_deref() == Some(&record.after_sha256) {
                    "applied"
                } else if current_hash == record.before_sha256 {
                    "not_applied"
                } else {
                    "conflict"
                }
                .into();
        }
        records.push(record);
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
    Ok(records)
}

#[cfg(unix)]
struct TransactionGuard<'a>(&'a File);

#[cfg(unix)]
fn lock_transaction(file: &File) -> Result<TransactionGuard<'_>> {
    use std::os::fd::AsRawFd;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(TransactionGuard(file));
        }
        let error = std::io::Error::last_os_error();
        ensure!(
            error.kind() == std::io::ErrorKind::WouldBlock,
            "workspace lock failed: {error}"
        );
        ensure!(
            std::time::Instant::now() < deadline,
            "workspace transaction is busy"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(unix)]
impl Drop for TransactionGuard<'_> {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn lock_transaction(_: &File) -> Result<()> {
    anyhow::bail!("native workspace lock unavailable")
}

fn write_record(store: &Store, record: &RecoveryRecord) -> Result<()> {
    atomic_write_at(
        &store.journal,
        &format!("{}.json", record.id),
        &serde_json::to_vec(record)?,
    )
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid recovery transaction ID"
    );
    Ok(())
}
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && !name.starts_with(".glass-")
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control),
        "managed editor requires a single file name inside its workspace"
    );
    #[cfg(windows)]
    windows::validate_component(std::ffi::OsStr::new(name))?;
    Ok(())
}

fn directory_names(directory: &File, path: &Path, limit: usize) -> Result<Vec<String>> {
    #[cfg(windows)]
    {
        let _ = path;
        windows::directory_names(directory, limit)
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        std::fs::read_dir(path)?
            .take(limit)
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect()
    }
}

#[cfg(unix)]
fn open_at(directory: &File, name: &str, flags: i32, mode: u32) -> Result<Option<File>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

#[cfg(unix)]
fn read_at(directory: &File, name: &str, limit: usize) -> Result<Option<Vec<u8>>> {
    use std::os::unix::fs::MetadataExt;
    let Some(file) = open_at(directory, name, libc::O_RDONLY | libc::O_NONBLOCK, 0)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.len() <= limit as u64,
        "managed path must be a bounded regular file without hard links"
    );
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "managed file exceeded its byte limit");
    Ok(Some(bytes))
}

#[cfg(unix)]
fn atomic_write_at(directory: &File, name: &str, bytes: &[u8]) -> Result<()> {
    use std::os::fd::AsRawFd;
    let temporary = format!(".glass-tmp-{}", random_token()?);
    let mut file = open_at(
        directory,
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?
    .context("temporary file could not be created")?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        let source = std::ffi::CString::new(temporary.as_str())?;
        let target = std::ffi::CString::new(name)?;
        ensure!(
            unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    source.as_ptr(),
                    directory.as_raw_fd(),
                    target.as_ptr(),
                )
            } == 0,
            "atomic managed-file replacement failed: {}",
            std::io::Error::last_os_error()
        );
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_at(directory, &temporary);
    }
    result
}

#[cfg(unix)]
fn remove_at(directory: &File, name: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(name)?;
    ensure!(
        unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0,
        "managed-file removal failed: {}",
        std::io::Error::last_os_error()
    );
    directory.sync_all()?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_at(_: &File, _: &str, _: usize) -> Result<Option<Vec<u8>>> {
    anyhow::bail!("native secure-file backend unavailable")
}
#[cfg(not(any(unix, windows)))]
fn atomic_write_at(_: &File, _: &str, _: &[u8]) -> Result<()> {
    anyhow::bail!("native secure-file backend unavailable")
}
#[cfg(not(any(unix, windows)))]
fn remove_at(_: &File, _: &str) -> Result<()> {
    anyhow::bail!("native secure-file backend unavailable")
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn completion_cross_store_transactions_preserve_the_undo_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("managed");
        let first = RecoveryStore::new(root.clone()).unwrap();
        let second = RecoveryStore::new(root).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for (store, text) in [(first.clone(), "first"), (second, "second")] {
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store.edit("concurrent.txt", text.as_bytes()).unwrap()
            }));
        }
        barrier.wait();
        let records: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.before_sha256.is_none())
                .count(),
            1
        );
        let newest = records
            .iter()
            .find(|record| record.before_sha256.is_some())
            .unwrap();
        first.rollback(&newest.id).unwrap();
        assert!(first.rollback(&newest.id).is_ok());
        let created = records
            .iter()
            .find(|record| record.before_sha256.is_none())
            .unwrap();
        first.rollback(&created.id).unwrap();
        assert!(first.rollback(&created.id).is_ok());
        assert!(!first.root().join("concurrent.txt").exists());
    }

    #[test]
    fn completion_interrupted_restore_reconciles_and_retries() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            RecoveryStore::new(directory.path().canonicalize().unwrap().join("managed")).unwrap();
        store.edit("note.txt", b"before").unwrap();
        let mut record = store.edit("note.txt", b"after").unwrap();
        {
            let state = store.inner.lock().unwrap();
            record.status = "rollback_prepared".into();
            write_record(&state, &record).unwrap();
            atomic_write_at(&state.directory, "note.txt", b"before").unwrap();
        }
        assert_eq!(
            store
                .records()
                .unwrap()
                .iter()
                .find(|entry| entry.id == record.id)
                .unwrap()
                .status,
            "rolled_back"
        );
        assert_eq!(store.rollback(&record.id).unwrap().status, "rolled_back");
        assert_eq!(store.read("note.txt").unwrap(), "before");
    }
}

// FVPROV1 19265 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZjkwYWExZTBkMWM2ZWM1ZmIyMjg4YWRjOWI0MmRiYTMzMTE2NDVjYmQ5NjkyNDM3OWE0YTc2MGU4NWUzZTQwMCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiM2MwYzA0YWU1NTdmMWZmYWU2ODk1NGMxMTY1ZjgxNTI4MjhkOWQ1YzU0ZTMzMGM1MGU4ZWM0OTRhZGMxZjQ3NmEyZTlmOTIxNDQzOGMzMDhhNjEzZTk1OTMzZGVhM2JjODZhY2RkMzhlZDUwMjNhOTEyMWMzMWMxNWU4OTc1MDMifV19XQ== FVPROV1-END
