// SPDX-License-Identifier: GPL-3.0-only
//! Installed Linux administrator helper and kernel process identity checks.
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub const BROKER_PATH: &str = "/usr/libexec/glass-terminal/plyra-privilege-broker";
pub const POLICY_PATH: &str = "/usr/share/polkit-1/actions/org.plyra.glass-terminal.policy";
pub const PKEXEC_PATH: &str = "/usr/bin/pkexec";

fn root_owned_path(path: &Path, executable: bool) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "administrator installation path must be absolute"
    );
    for (index, component) in path.ancestors().enumerate() {
        let metadata = std::fs::symlink_metadata(component).with_context(|| {
            format!(
                "administrator component is unavailable: {}",
                component.display()
            )
        })?;
        ensure!(
            !metadata.file_type().is_symlink()
                && metadata.uid() == 0
                && metadata.mode() & 0o022 == 0,
            "administrator component must be root-owned and protected: {}",
            component.display()
        );
        if index == 0 {
            ensure!(
                metadata.is_file(),
                "administrator component is not a regular file"
            );
            ensure!(
                !executable || metadata.mode() & 0o111 != 0,
                "administrator component is not executable"
            );
        } else {
            ensure!(metadata.is_dir(), "administrator parent is not a directory");
        }
    }
    Ok(())
}

pub fn verify_installation() -> Result<PathBuf> {
    root_owned_path(Path::new(BROKER_PATH), true)?;
    root_owned_path(Path::new(POLICY_PATH), false)?;
    root_owned_path(Path::new(PKEXEC_PATH), true)?;
    Ok(PathBuf::from(BROKER_PATH))
}

pub(crate) fn file_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("cannot read executable identity: {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "executable identity is not a regular file"
    );
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn process_start_time(pid: u32) -> Result<u64> {
    ensure!(pid > 1, "invalid authenticated process PID");
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_process_start_time(&text)
}

fn parse_process_start_time(text: &str) -> Result<u64> {
    // comm may contain spaces and ')'. Field 3 follows its final closing ')'.
    let fields = text
        .rsplit_once(')')
        .context("invalid process stat identity")?
        .1;
    let start = fields
        .split_whitespace()
        .nth(19)
        .context("process start time is unavailable")?
        .parse::<u64>()?;
    ensure!(start > 0, "invalid process start time");
    Ok(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_start_identity_uses_field_22_after_parenthesized_names() {
        let stat =
            "123 (name with ) spaces) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 99";
        assert_eq!(parse_process_start_time(stat).unwrap(), 4242);
        assert!(parse_process_start_time("123 missing fields").is_err());
    }

    #[test]
    fn writable_or_symlinked_helpers_cannot_be_trusted() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("broker");
        std::fs::write(&helper, b"owned test bytes").unwrap();
        assert!(root_owned_path(&helper, true).is_err());
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&helper, &link).unwrap();
        assert!(root_owned_path(&link, true).is_err());
    }

    #[test]
    fn executable_hash_uses_the_fixed_sha256_vector() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("identity");
        std::fs::write(&executable, b"abc").unwrap();
        assert_eq!(
            file_hash(&executable).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

// FVPROV1 4446 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZThjZjkxYmY1ZjcyNDViZjJiZjAzZjY5ZDIxNDQyZTYxMjQzYmZiMDMyZDgyMTM4NjUyMDQ2ZDU0YTNhM2RkOSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZjMzYWE1MmIxMDhmODdjZWQ5M2FmNjA4YjlmMDU5NjIyODcwYzQyYjViNjk5M2Q1NzdkNTgxYTAwNDczZDQyYmM2NzExZTE4ZDI0NWI1MjkyZWMzNjJmY2VkOTRkMzRmMWE5NWI5MWNmMDczMDA2Y2I5MGYwMTExMzI0YzY2MGUifV19XQ== FVPROV1-END
