// SPDX-License-Identifier: GPL-3.0-only
//! HANDLE-relative local Windows storage for the shared recovery transactions.
use super::{random_token, Store};
use crate::windows::{windows_identity as identity, windows_trust};
use anyhow::{ensure, Context, Result};
use std::{
    ffi::{c_void, OsStr},
    fs::{File, OpenOptions},
    io::{Read, Write},
    mem::{offset_of, size_of},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_LOCK_VIOLATION, ERROR_NO_MORE_FILES, HANDLE},
    Security::{
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SetSecurityInfo, SDDL_REVISION_1,
            SE_FILE_OBJECT,
        },
        GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        FileDispositionInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileType, LockFileEx,
        SetFileInformationByHandle, UnlockFileEx, BY_HANDLE_FILE_INFORMATION, DELETE,
        FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_RENAME_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_TRAVERSE, FILE_TYPE_DISK, FILE_WRITE_DATA, LOCKFILE_EXCLUSIVE_LOCK,
        LOCKFILE_FAIL_IMMEDIATELY, READ_CONTROL, SYNCHRONIZE, WRITE_DAC,
    },
    System::IO::OVERLAPPED,
};

const NT_OPEN: u32 = 1;
const NT_CREATE: u32 = 2;
const NT_OPEN_IF: u32 = 3;
const NT_FILE_RENAME_INFORMATION: u32 = 10;
const NT_DIRECTORY_FILE: u32 = 0x0000_0001;
const NT_WRITE_THROUGH: u32 = 0x0000_0002;
const NT_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const NT_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const NT_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const BASE_ACCESS: u32 = READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const DIRECTORY_ACCESS: u32 = BASE_ACCESS | FILE_LIST_DIRECTORY | FILE_TRAVERSE;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *mut UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}
#[repr(C)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        attributes: *mut ObjectAttributes,
        status: *mut IoStatusBlock,
        allocation_size: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea_buffer: *mut c_void,
        ea_length: u32,
    ) -> i32;
    fn NtSetInformationFile(
        handle: HANDLE,
        status: *mut IoStatusBlock,
        information: *const c_void,
        length: u32,
        information_class: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

struct Descriptor(*mut c_void);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn private_descriptor() -> Result<Descriptor> {
    let sid = identity::current_principal()?.sid;
    let sddl = identity::wide(OsStr::new(&format!(
        "O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    )))?;
    let mut descriptor = ptr::null_mut();
    ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } != 0,
        "cannot create private recovery security descriptor: {}",
        std::io::Error::last_os_error()
    );
    Ok(Descriptor(descriptor))
}

pub(super) fn validate_component(name: &OsStr) -> Result<()> {
    let name = name.to_string_lossy();
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.ends_with(['.', ' '])
            && !name
                .chars()
                .any(|value| value.is_control() || ":\\/*?\"<>|".contains(value)),
        "Windows recovery requires a regular single file name without streams"
    );
    let stem = name.split('.').next().unwrap().to_ascii_uppercase();
    ensure!(
        ![
            "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM¹", "COM²", "COM³", "LPT¹",
            "LPT²", "LPT³"
        ]
        .contains(&stem.as_str())
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9')),
        "Windows device names cannot be used as recovery files"
    );
    Ok(())
}

fn information(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetFileType(file.as_raw_handle()) } == FILE_TYPE_DISK,
        "recovery path is not a disk file"
    );
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "cannot inspect recovery file: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "reparse points are not allowed in recovery storage"
    );
    Ok(info)
}

fn seal_owned(file: &File) -> Result<()> {
    ensure!(
        windows_trust::owner_sid(file.as_raw_handle())? == identity::current_principal()?.sid,
        "recovery object belongs to another Windows user"
    );
    let descriptor = private_descriptor()?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl = ptr::null_mut();
    ensure!(
        unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted) }
            != 0
            && present != 0
            && !acl.is_null(),
        "private recovery DACL is unavailable"
    );
    let error = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null_mut(),
        )
    };
    ensure!(error == 0, "cannot protect recovery object DACL ({error})");
    Ok(())
}

fn open_relative(
    directory: &File,
    name: &OsStr,
    access: u32,
    disposition: u32,
    is_directory: bool,
    share: u32,
) -> Result<Option<File>> {
    validate_component(name)?;
    let mut name = identity::wide(name)?;
    ensure!(
        name.len() <= 32_767,
        "recovery component exceeds NT name bounds"
    );
    let mut unicode = UnicodeString {
        length: ((name.len() - 1) * 2) as u16,
        maximum_length: (name.len() * 2) as u16,
        buffer: name.as_mut_ptr(),
    };
    let descriptor = (disposition != NT_OPEN)
        .then(private_descriptor)
        .transpose()?;
    let mut attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: directory.as_raw_handle(),
        object_name: &mut unicode,
        attributes: 0x40 | 0x1000, // OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE.
        security_descriptor: descriptor.as_ref().map_or(ptr::null_mut(), |value| value.0),
        security_quality_of_service: ptr::null_mut(),
    };
    let mut status: IoStatusBlock = unsafe { std::mem::zeroed() };
    let mut handle = ptr::null_mut();
    let options = NT_OPEN_REPARSE_POINT
        | NT_SYNCHRONOUS_IO_NONALERT
        | if is_directory {
            NT_DIRECTORY_FILE
        } else {
            NT_NON_DIRECTORY_FILE | NT_WRITE_THROUGH
        };
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &mut attributes,
            &mut status,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            options,
            ptr::null_mut(),
            0,
        )
    };
    if result < 0 {
        let error =
            std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(result) } as i32);
        if disposition == NT_OPEN && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("opening HANDLE-relative recovery component");
    }
    ensure!(
        !handle.is_null(),
        "NT recovery open returned an invalid handle"
    );
    let file = unsafe { File::from_raw_handle(handle) };
    let info = information(&file)?;
    ensure!(
        (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == is_directory,
        "recovery component has the wrong object type"
    );
    Ok(Some(file))
}

fn open_directory(root: &Path, create: bool) -> Result<File> {
    ensure!(root.is_absolute(), "managed directory must be absolute");
    let mut components = root.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => anyhow::bail!("Windows recovery currently requires a local drive path"),
        },
        _ => anyhow::bail!("Windows recovery requires an absolute local drive path"),
    };
    ensure!(
        matches!(components.next(), Some(Component::RootDir)),
        "managed path has no drive root"
    );
    let anchor = format!("\\\\?\\{}:\\", char::from(drive));
    let mut directory = OpenOptions::new()
        .read(true)
        .access_mode(DIRECTORY_ACCESS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(anchor)?;
    information(&directory)?;
    let parts: Vec<_> = components.collect();
    ensure!(
        !parts.is_empty(),
        "a drive root cannot be a recovery workspace"
    );
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            anyhow::bail!("managed path contains a non-normal component");
        };
        let access = if create && index + 1 == parts.len() {
            DIRECTORY_ACCESS | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | WRITE_DAC
        } else {
            DIRECTORY_ACCESS
        };
        directory = open_relative(
            &directory,
            name,
            access,
            if create { NT_OPEN_IF } else { NT_OPEN },
            true,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?
        .context("managed directory could not be opened")?;
    }
    ensure!(
        windows_trust::owner_sid(directory.as_raw_handle())? == identity::current_principal()?.sid,
        "private application directory belongs to another Windows user"
    );
    Ok(directory)
}

pub(crate) fn open_existing_directory(root: &Path) -> Result<File> {
    open_directory(root, false)
}

pub(super) fn open_store(root: PathBuf) -> Result<Store> {
    let directory = open_directory(&root, true)?;
    seal_owned(&directory)?;
    let journal = open_relative(
        &directory,
        OsStr::new(".glass-journal"),
        DIRECTORY_ACCESS | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | WRITE_DAC,
        NT_OPEN_IF,
        true,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )?
    .context("recovery journal could not be opened")?;
    seal_owned(&journal)?;
    let transaction_lock = open_relative(
        &journal,
        OsStr::new("transaction.lock"),
        BASE_ACCESS | FILE_READ_DATA | FILE_WRITE_DATA | WRITE_DAC,
        NT_OPEN_IF,
        false,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )?
    .context("workspace transaction lock could not be opened")?;
    regular_file(&transaction_lock, 1_048_576)?;
    seal_owned(&transaction_lock)?;
    Ok(Store {
        root,
        directory,
        journal,
        transaction_lock,
    })
}

fn regular_file(file: &File, limit: usize) -> Result<()> {
    let info = information(file)?;
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            && info.nNumberOfLinks == 1
            && file.metadata()?.len() <= limit as u64,
        "managed path must be a bounded regular file without hard links"
    );
    Ok(())
}

pub(crate) fn open_audit_file(directory: &File, name: &str, limit: usize) -> Result<File> {
    let file = open_relative(
        directory,
        OsStr::new(name),
        BASE_ACCESS | FILE_READ_DATA,
        NT_OPEN,
        false,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )?
    .context("persistent audit file disappeared")?;
    regular_file(&file, limit)?;
    ensure!(
        windows_trust::owner_sid(file.as_raw_handle())? == identity::current_principal()?.sid,
        "persistent audit file belongs to another Windows user"
    );
    Ok(file)
}

pub(super) fn read_at(directory: &File, name: &str, limit: usize) -> Result<Option<Vec<u8>>> {
    let Some(file) = open_relative(
        directory,
        OsStr::new(name),
        BASE_ACCESS | FILE_READ_DATA | WRITE_DAC,
        NT_OPEN,
        false,
        FILE_SHARE_READ,
    )?
    else {
        return Ok(None);
    };
    regular_file(&file, limit)?;
    seal_owned(&file)?;
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "managed file exceeded its byte limit");
    Ok(Some(bytes))
}

fn mark_deleted(file: &File) -> Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    ensure!(
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } != 0,
        "recovery file removal failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn rename_information(directory: HANDLE, name: &str) -> Result<Vec<usize>> {
    let target = identity::wide(OsStr::new(name))?;
    let name_bytes = (target.len() - 1) * size_of::<u16>();
    // FileName starts before the structure's trailing alignment padding. Keep
    // that ABI offset, but supply the full structure plus the variable name
    // bytes required by the file-information API, including aligned storage.
    let byte_length = size_of::<FILE_RENAME_INFO>() + name_bytes;
    let mut buffer = vec![0_usize; byte_length.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = 1;
        (*info).RootDirectory = directory;
        (*info).FileNameLength = name_bytes as u32;
        ptr::copy_nonoverlapping(
            target.as_ptr(),
            ptr::addr_of_mut!((*info).FileName).cast(),
            target.len(),
        );
    }
    Ok(buffer)
}

pub(super) fn atomic_write_at(directory: &File, name: &str, bytes: &[u8]) -> Result<()> {
    validate_component(OsStr::new(name))?;
    let temporary = format!(".glass-tmp-{}", random_token()?);
    let mut file = open_relative(
        directory,
        OsStr::new(&temporary),
        BASE_ACCESS | FILE_WRITE_DATA | DELETE,
        NT_CREATE,
        false,
        FILE_SHARE_READ,
    )?
    .context("temporary recovery file could not be created")?;
    let mut renamed = false;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        let buffer = rename_information(directory.as_raw_handle(), name)?;
        let byte_length = u32::try_from(buffer.len() * size_of::<usize>())
            .context("recovery rename information exceeds native byte bounds")?;
        let mut io_status: IoStatusBlock = unsafe { std::mem::zeroed() };
        // Keep the validated directory HANDLE as the name-resolution root.
        // This is the NT FileRenameInformation class (10), not the Win32
        // FileRenameInfo class (3); the buffer has the shared native layout.
        let status = unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut io_status,
                buffer.as_ptr().cast(),
                byte_length,
                NT_FILE_RENAME_INFORMATION,
            )
        };
        ensure!(
            status >= 0,
            "atomic HANDLE-relative replacement failed: NTSTATUS 0x{:08X}: {}",
            status as u32,
            std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
        );
        renamed = true;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() && !renamed {
        let _ = mark_deleted(&file);
    }
    result
}

pub(super) fn remove_at(directory: &File, name: &str) -> Result<()> {
    let file = open_relative(
        directory,
        OsStr::new(name),
        BASE_ACCESS | FILE_READ_DATA | DELETE,
        NT_OPEN,
        false,
        FILE_SHARE_READ,
    )?
    .context("recovery file disappeared before removal")?;
    regular_file(&file, super::MAX_FILE_BYTES)?;
    ensure!(
        windows_trust::owner_sid(file.as_raw_handle())? == identity::current_principal()?.sid,
        "recovery removal target belongs to another user"
    );
    mark_deleted(&file)
}

pub(super) struct TransactionGuard<'a>(&'a File);
pub(super) fn lock_transaction(file: &File) -> Result<TransactionGuard<'_>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut overlap: OVERLAPPED = unsafe { std::mem::zeroed() };
        if unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlap,
            )
        } != 0
        {
            return Ok(TransactionGuard(file));
        }
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32),
            "workspace lock failed: {error}"
        );
        ensure!(Instant::now() < deadline, "workspace transaction is busy");
        std::thread::sleep(Duration::from_millis(5));
    }
}
impl Drop for TransactionGuard<'_> {
    fn drop(&mut self) {
        let mut overlap: OVERLAPPED = unsafe { std::mem::zeroed() };
        unsafe {
            UnlockFileEx(self.0.as_raw_handle(), 0, 1, 0, &mut overlap);
        }
    }
}

pub(crate) fn directory_names(directory: &File, limit: usize) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut buffer = vec![0_u64; 8192];
    let mut class = FileIdBothDirectoryRestartInfo;
    while result.len() < limit {
        if unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(error).context("enumerating HANDLE-relative recovery directory");
        }
        class = FileIdBothDirectoryInfo;
        let mut offset = 0;
        loop {
            ensure!(
                offset + size_of::<FILE_ID_BOTH_DIR_INFO>() <= buffer.len() * 8,
                "invalid recovery directory enumeration record"
            );
            let entry = unsafe {
                &*buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
            };
            let length = entry.FileNameLength as usize;
            ensure!(
                length % 2 == 0
                    && offset + offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) + length
                        <= buffer.len() * 8,
                "invalid recovery directory file name"
            );
            let name = unsafe {
                std::slice::from_raw_parts(ptr::addr_of!(entry.FileName).cast::<u16>(), length / 2)
            };
            if let Ok(name) = String::from_utf16(name) {
                result.push(name);
            }
            if result.len() == limit || entry.NextEntryOffset == 0 {
                break;
            }
            ensure!(
                entry.NextEntryOffset as usize
                    >= offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) + length
                    && entry.NextEntryOffset % 8 == 0,
                "invalid recovery directory entry boundary"
            );
            offset += entry.NextEntryOffset as usize;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_information_retains_abi_filename_offset_and_full_buffer_size() {
        let directory = 0x1234_usize as HANDLE;
        let filename_offset = offset_of!(FILE_RENAME_INFO, FileName);
        assert!(size_of::<FILE_RENAME_INFO>() >= filename_offset + size_of::<u16>());
        for name in ["a", "note.txt", "空 格🧪.txt"] {
            let expected = identity::wide(OsStr::new(name)).unwrap();
            let buffer = rename_information(directory, name).unwrap();
            let info = unsafe { &*buffer.as_ptr().cast::<FILE_RENAME_INFO>() };
            let name_bytes = (expected.len() - 1) * size_of::<u16>();
            assert_eq!(info.RootDirectory, directory);
            assert_eq!(unsafe { info.Anonymous.ReplaceIfExists }, 1);
            assert_eq!(info.FileNameLength as usize, name_bytes);
            assert!(
                buffer.len() * size_of::<usize>() >= size_of::<FILE_RENAME_INFO>() + name_bytes
            );
            let filename = ptr::addr_of!(info.FileName).cast::<u16>();
            assert_eq!(
                filename as usize - buffer.as_ptr() as usize,
                filename_offset
            );
            assert_eq!(
                unsafe { std::slice::from_raw_parts(filename, expected.len()) },
                expected
            );
        }
    }

    #[test]
    fn streams_devices_and_non_normal_names_are_rejected() {
        for name in [
            "note:secret",
            "NUL.txt",
            "COM1",
            "COM¹",
            "x.",
            "x ",
            "..",
            "a/b",
            "a\\b",
        ] {
            assert!(validate_component(OsStr::new(name)).is_err(), "{name}");
        }
        assert!(validate_component(OsStr::new("中文.txt")).is_ok());
    }

    #[test]
    fn recovery_refuses_hard_links_without_changing_the_original() {
        let directory = tempfile::tempdir().unwrap();
        let store = super::super::RecoveryStore::new(
            directory.path().canonicalize().unwrap().join("managed"),
        )
        .unwrap();
        std::fs::write(store.root().join("original.txt"), b"before").unwrap();
        std::fs::hard_link(
            store.root().join("original.txt"),
            store.root().join("linked.txt"),
        )
        .unwrap();
        assert!(store.read("linked.txt").is_err());
        assert!(store.edit("linked.txt", b"after").is_err());
        assert_eq!(
            std::fs::read(store.root().join("original.txt")).unwrap(),
            b"before"
        );
    }
}

// FVPROV1 23740 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNGE2OTcxMDQ2ODViNTU1NWVhNjVkZGQzNWExYmY5MDYzZjFiMTU0MzczMDM4MDU4NzAzNmNlYjYzNTQ0NDk1YyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZDU1NWI0YjZhZjQyZDkwNzM5NDliOTY1MDg1NjFiMjEzMWUyOWFlNTk5NTk4ZDE4ZmIwNTU3YzZhNTEyZTQ4ODMwMTM2MjJlY2Y0MDJiOWU3YzJhN2NhOTliNDczNTZlNmVhZWJmZTQ2NzFlZWVmNzdlNGViMThmZTM5NDhmMDQifV19XQ== FVPROV1-END
