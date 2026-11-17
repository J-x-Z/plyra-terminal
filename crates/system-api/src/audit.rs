// SPDX-License-Identifier: GPL-3.0-only
#[cfg(unix)]
use crate::current_uid;
use crate::AuditRecord;
use anyhow::{ensure, Context, Result};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::{collections::VecDeque, io::Read, path::Path};

pub(crate) fn read_history(directory: &Path, limit: usize) -> Result<Vec<AuditRecord>> {
    ensure!(
        (1..=500).contains(&limit),
        "audit history limit must be 1..500"
    );
    if !directory.exists() {
        return Ok(Vec::new());
    }
    #[cfg(any(unix, windows))]
    {
        #[cfg(unix)]
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        };
        #[cfg(unix)]
        let directory_fd = {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(directory)?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.uid() == current_uid() && metadata.mode() & 0o077 == 0,
                "audit history directory is not private to the current user"
            );
            file
        };
        #[cfg(windows)]
        let directory_fd = crate::recovery::windows::open_existing_directory(directory)?;
        #[cfg(windows)]
        let caller_sid = crate::windows::current_principal()?.sid;
        #[cfg(unix)]
        let mut names: Vec<_> = std::fs::read_dir(directory)?
            .take(4096)
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?;
        #[cfg(windows)]
        let mut names = crate::recovery::windows::directory_names(&directory_fd, 4096)?;
        names.retain(|name| valid_name(name));
        names.sort();
        let mut records = VecDeque::new();
        for name in names
            .into_iter()
            .rev()
            .take(32)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            #[cfg(unix)]
            let file = {
                let name = std::ffi::CString::new(name)?;
                let fd = unsafe {
                    libc::openat(
                        directory_fd.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                ensure!(
                    fd >= 0,
                    "cannot read persistent broker audit: {}",
                    std::io::Error::last_os_error()
                );
                let file = unsafe { std::fs::File::from_raw_fd(fd) };
                let metadata = file.metadata()?;
                ensure!(
                    metadata.is_file()
                        && metadata.nlink() == 1
                        && metadata.len() <= 1024 * 1024
                        && (metadata.uid() == current_uid() || metadata.uid() == 0),
                    "persistent audit file identity or size is invalid"
                );
                file
            };
            #[cfg(windows)]
            let file =
                crate::recovery::windows::open_audit_file(&directory_fd, &name, 1024 * 1024)?;
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            append_records(&bytes, &mut records, limit, |record| {
                #[cfg(unix)]
                {
                    record.caller_uid == current_uid()
                }
                #[cfg(windows)]
                {
                    record.caller_sid.as_deref() == Some(caller_sid.as_str())
                }
            })?;
        }
        let mut records: Vec<_> = records.into_iter().collect();
        records.sort_by_key(|record| record.timestamp_ms);
        Ok(records)
    }
    #[cfg(not(any(unix, windows)))]
    {
        anyhow::bail!("native persistent audit reader is unavailable on this platform")
    }
}

fn append_records(
    bytes: &[u8],
    records: &mut VecDeque<AuditRecord>,
    limit: usize,
    authorized: impl Fn(&AuditRecord) -> bool,
) -> Result<()> {
    ensure!(
        bytes.len() <= 1024 * 1024,
        "persistent audit grew beyond its byte limit"
    );
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        ensure!(
            line.len() <= 16_384,
            "persistent audit record exceeds its limit"
        );
        let record: AuditRecord = serde_json::from_slice(line)
            .context("persistent audit contains an incomplete or invalid record")?;
        ensure!(
            authorized(&record)
                && record.operation.len() <= 128
                && record.target.len() <= 4096
                && record.result.len() <= 4096,
            "persistent audit record is outside this user's scope"
        );
        records.push_back(record);
        if records.len() > limit {
            records.pop_front();
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    let Some(base) = name.strip_suffix(".jsonl") else {
        return false;
    };
    let Some((timestamp, suffix)) = base.split_once('-') else {
        return false;
    };
    !timestamp.is_empty()
        && timestamp.len() <= 20
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
        && suffix.len() == 12
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_persistent_history_is_readable_after_session_and_rejects_links() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let record = AuditRecord {
            timestamp_ms: 1,
            caller_uid: crate::current_uid(),
            caller_pid: std::process::id(),
            caller_sid: {
                #[cfg(windows)]
                {
                    Some(crate::windows::current_principal().unwrap().sid)
                }
                #[cfg(not(windows))]
                {
                    None
                }
            },
            operation: "identity".into(),
            target: "fixture".into(),
            result: "read".into(),
        };
        let mut bytes = serde_json::to_vec(&record).unwrap();
        bytes.push(b'\n');
        let path = directory.path().join("1-abcdef012345.jsonl");
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_history(directory.path(), 20).unwrap().len(), 1);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&path, directory.path().join("2-abcdef012345.jsonl"))
                .unwrap();
            assert!(read_history(directory.path(), 20).is_err());
        }
        #[cfg(windows)]
        {
            std::fs::hard_link(&path, directory.path().join("2-abcdef012345.jsonl")).unwrap();
            assert!(read_history(directory.path(), 20).is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_audit_shares_with_live_writer_and_requires_the_full_caller_sid() {
        use std::{io::Write, os::windows::fs::OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("1-abcdef012345.jsonl");
        let mut record = AuditRecord {
            timestamp_ms: 1,
            caller_uid: u32::MAX,
            caller_pid: std::process::id(),
            caller_sid: Some(crate::windows::current_principal().unwrap().sid),
            operation: "identity".into(),
            target: "owned audit fixture".into(),
            result: "read".into(),
        };
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        let mut bytes = serde_json::to_vec(&record).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).unwrap();
        assert_eq!(read_history(directory.path(), 20).unwrap().len(), 1);
        drop(writer);
        record.caller_sid = Some("S-1-0-0".into());
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(read_history(directory.path(), 20).is_err());
        record.caller_sid = None;
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(read_history(directory.path(), 20).is_err());
    }
}

// FVPROV1 8709 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiOGI0MTc5MDNkMzgzMTEzY2Y1MDMwYThjZTY5NTEzZDM1MjdiM2QzMmJiOWNhOTVmNjU1NTVlYjllZDg2NTk1ZiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNWRjZjRkZTBiNWY3MDhiOTFkYjNiMjBlYjllMWVlZWYyYzkwODg1ZDhhNjMyYzZmYWIzZWQ1ZjgzY2JlMjk1NjkxMzU3MGMxYzMxMzlhOWMwNDg0ZWQwODc2OTI5YTM1ZjZkZTVjNTAwYjU4ZDkwNTZlMDYyZWQ0YzIyNDhjMDAifV19XQ== FVPROV1-END
