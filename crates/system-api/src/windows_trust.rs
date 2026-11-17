// SPDX-License-Identifier: GPL-3.0-only
use super::{
    windows_identity::{self, PinnedImage},
    windows_services,
};
use anyhow::{ensure, Result};
use std::{
    fs::{File, OpenOptions},
    os::windows::{
        ffi::OsStringExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::PathBuf,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{LocalFree, GENERIC_ALL, GENERIC_WRITE, HANDLE},
    Security::{
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
        GetAce, ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE,
        OWNER_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, READ_CONTROL,
        WRITE_DAC, WRITE_OWNER,
    },
    System::{
        Com::CoTaskMemFree,
        SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE},
    },
    UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath},
};

pub struct InstalledPackage {
    pub broker: PinnedImage,
    pub ui: PinnedImage,
    _directories: Vec<File>,
}

struct Descriptor(*mut std::ffi::c_void);

impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

pub fn owner_sid(handle: HANDLE) -> Result<String> {
    unsafe {
        let mut owner = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let error = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        );
        ensure!(error == 0, "cannot query Windows file owner ({error})");
        let _descriptor = Descriptor(descriptor);
        windows_identity::sid_string(owner)
    }
}

fn verify_acl(handle: HANDLE, trusted: &[String], leaf: bool) -> Result<()> {
    unsafe {
        let mut owner = ptr::null_mut();
        let mut acl = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let error = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut descriptor,
        );
        ensure!(error == 0, "cannot verify installed broker ACL ({error})");
        let _descriptor = Descriptor(descriptor);
        ensure!(
            trusted.contains(&windows_identity::sid_string(owner)?),
            "installed broker path has an untrusted owner SID"
        );
        ensure!(
            !acl.is_null() && (*acl).AceCount <= 1024,
            "installed broker path has an unrestricted or oversized DACL"
        );
        let mut dangerous = GENERIC_ALL
            | GENERIC_WRITE
            | DELETE
            | WRITE_DAC
            | WRITE_OWNER
            | FILE_WRITE_DATA
            | FILE_WRITE_EA
            | FILE_WRITE_ATTRIBUTES
            | FILE_DELETE_CHILD;
        if leaf {
            dangerous |= FILE_APPEND_DATA;
        }
        for index in 0..u32::from((*acl).AceCount) {
            let mut entry = ptr::null_mut();
            ensure!(
                GetAce(acl, index, &mut entry) != 0,
                "cannot inspect installed broker ACL"
            );
            let ace = &*entry.cast::<ACCESS_ALLOWED_ACE>();
            if u32::from(ace.Header.AceFlags) & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            ensure!(
                u32::from(ace.Header.AceType) == ACCESS_ALLOWED_ACE_TYPE
                    || u32::from(ace.Header.AceType) == ACCESS_DENIED_ACE_TYPE,
                "installed broker path uses an unsupported access-control entry"
            );
            if u32::from(ace.Header.AceType) == ACCESS_ALLOWED_ACE_TYPE && ace.Mask & dangerous != 0
            {
                let sid =
                    windows_identity::sid_string(ptr::addr_of!(ace.SidStart).cast_mut().cast())?;
                ensure!(
                    trusted.contains(&sid),
                    "installed broker path grants modification to unprivileged SID {sid}"
                );
            }
        }
        Ok(())
    }
}

pub fn installed_package() -> Result<InstalledPackage> {
    let program_files = unsafe {
        let mut pointer = ptr::null_mut();
        let result = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, ptr::null_mut(), &mut pointer);
        ensure!(
            result >= 0 && !pointer.is_null(),
            "Windows Program Files location is unavailable ({result:#x})"
        );
        let mut length = 0;
        while *pointer.add(length) != 0 {
            length += 1;
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
            pointer, length,
        )));
        CoTaskMemFree(pointer.cast());
        path
    };
    ensure!(
        program_files.is_absolute(),
        "Windows Program Files location is not absolute"
    );
    let directory = program_files.join("GlassTerminal");
    let trusted = vec![
        "S-1-5-18".into(),
        "S-1-5-32-544".into(),
        windows_services::account_sid("NT SERVICE\\TrustedInstaller")?,
    ];
    let mut ancestors = Vec::new();
    let mut path = PathBuf::new();
    for component in directory.components() {
        path.push(component.as_os_str());
        if !path.is_absolute() {
            continue;
        }
        let file = OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "installed broker ancestor is not a non-reparse directory"
        );
        // Creating unrelated directories at a volume root cannot replace this pinned path.
        verify_acl(file.as_raw_handle(), &trusted, path == directory)?;
        ancestors.push(file);
    }
    let broker = PinnedImage::open(&directory.join("plyra-privilege-broker.exe"))?;
    let ui = PinnedImage::open(&directory.join("glass-terminal.exe"))?;
    verify_acl(broker.raw(), &trusted, true)?;
    verify_acl(ui.raw(), &trusted, true)?;
    Ok(InstalledPackage {
        broker,
        ui,
        _directories: ancestors,
    })
}

// FVPROV1 6748 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNzBmYzQyNTJjYmM2MmRlYjg3MWU0YjdjYzE1ZjQ3ZDhmZjIyMGM5N2VhODg5OGQ5Mzk3M2MzYTRlOTgzNDhjYyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTE0NGRmYzMyM2M0OTg3NjRlYjNlYzkzOTU5NjljOGY0ZWZlYzJiZmI4M2MyOWQ5NTJhNzI3OWIwM2UxNTAzY2U0YzUyN2JhZWE4OWRjMGZmNDFhNjlmNzczYzk5ZDViNzNkNzQzNWMwY2E5ZWM0Y2E5OTA2OWY2ZDUxOGVjMDAifV19XQ== FVPROV1-END
