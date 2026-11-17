//! Streaming import of a user-selected Arch Linux ARM root filesystem.
use anyhow::{bail, ensure, Context, Result};
use flate2::read::MultiGzDecoder;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CString, OsString},
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    net::IpAddr,
    os::unix::{
        ffi::OsStrExt, ffi::OsStringExt, fs::MetadataExt, fs::OpenOptionsExt, fs::PermissionsExt,
    },
    path::{Component, Path, PathBuf},
};

const MAX_COMPRESSED: u64 = 2 * 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 8 * 1024 * 1024 * 1024;
const MAX_FILE: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 300_000;
const MAX_METADATA: u64 = 64 * 1024;
const RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const STAGE_MARKER: &str = "glass-arch-import-stage-v1\n";
const INSTALL_MARKER: &str = ".glass-terminal-import.json";

type Attributes = HashMap<String, Vec<u8>>;

#[derive(Default)]
struct ExtractStats {
    entries: usize,
    regular_bytes: u64,
    materialized_hardlink_count: usize,
    materialized_hardlink_bytes: u64,
}

struct Bounded<R> {
    inner: R,
    limit: u64,
    bytes: u64,
    hash: Option<Sha256>,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit - self.bytes;
        if remaining == 0 {
            let mut probe = [0];
            if self.inner.read(&mut probe)? == 0 {
                return Ok(0);
            }
            return Err(io::Error::other(
                "root filesystem archive exceeds its size limit",
            ));
        }
        let size = buffer.len().min(remaining.min(usize::MAX as u64) as usize);
        let read = self.inner.read(&mut buffer[..size])?;
        self.bytes += read as u64;
        if let Some(hash) = &mut self.hash {
            hash.update(&buffer[..read]);
        }
        Ok(read)
    }
}

/// Publishes one complete rootfs. Existing installations and guest edits are preserved.
/// Empty DNS input supports offline import without selecting third-party resolvers.
pub fn install(input: impl Read, files_dir: &Path, dns_servers: &[IpAddr]) -> Result<()> {
    ensure!(
        dns_servers.len() <= 8,
        "At most eight active-network DNS servers are supported"
    );
    check_directory(files_dir)?;
    let directory = files_dir.join("arch-linux");
    create_directory(&directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(".install.lock"))?;
    let lock_metadata = lock.metadata()?;
    ensure!(
        lock_metadata.is_file()
            && lock_metadata.nlink() == 1
            && lock_metadata.uid() == unsafe { libc::geteuid() },
        "Invalid rootfs import lock"
    );
    ensure!(
        unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } == 0,
        "Another Arch Linux import is already running"
    );
    let destination = directory.join("rootfs");
    ensure!(
        !destination.try_exists()? && fs::symlink_metadata(&destination).is_err(),
        "Arch Linux is already installed"
    );
    clean_stages(&directory)?;
    let stage = tempfile::Builder::new()
        .prefix(".import-")
        .tempdir_in(&directory)?;
    write_new(&stage.path().join("owner"), STAGE_MARKER.as_bytes(), 0o600)?;
    let root = stage.path().join("rootfs");
    create_directory(&root)?;
    let budget = available_bytes(&directory)?
        .saturating_sub(RESERVE_BYTES)
        .min(MAX_EXPANDED);
    ensure!(
        budget > 0,
        "Insufficient storage for root filesystem import"
    );
    let compressed = Bounded {
        inner: input,
        limit: MAX_COMPRESSED,
        bytes: 0,
        hash: Some(Sha256::new()),
    };
    let decoded = MultiGzDecoder::new(compressed);
    let counted = Bounded {
        inner: decoded,
        limit: MAX_EXPANDED,
        bytes: 0,
        hash: None,
    };
    let mut reader = BufReader::with_capacity(64 * 1024, counted);
    let stats = extract(&mut reader, &root, budget)?;
    let mut trailing = [0; 64 * 1024];
    loop {
        let read = reader
            .read(&mut trailing)
            .context("Checking gzip trailer")?;
        if read == 0 {
            break;
        }
        ensure!(
            trailing[..read].iter().all(|byte| *byte == 0),
            "Unexpected data after TAR end marker"
        );
    }
    let compressed = reader.into_inner().inner.into_inner();
    let archive_hash = format!("{:x}", compressed.hash.unwrap().finalize());
    validate_arch(&root)?;
    for path in ["root", "tmp", "dev", "dev/pts", "proc", "sys", "etc"] {
        ensure_directories(&root, Path::new(path))?;
    }
    for path in [
        "dev/null",
        "dev/zero",
        "dev/random",
        "dev/urandom",
        "dev/tty",
    ] {
        replace_guest_file(&root, path, b"")?;
    }
    let mut resolver = String::from("# Seeded from Android active-network DNS during import.\n");
    if dns_servers.is_empty() {
        resolver.push_str("# Offline import: no active-network DNS server was available.\n");
    }
    for server in dns_servers {
        resolver.push_str(&format!("nameserver {server}\n"));
    }
    replace_guest_file(&root, "etc/resolv.conf", resolver.as_bytes())?;
    replace_guest_file(&root, "etc/hosts", b"127.0.0.1 localhost\n::1 localhost\n")?;
    let marker = serde_json::json!({
        "schema": "glass-arch-rootfs.v1", "architecture": "aarch64", "distribution": "arch",
        "archive_sha256": archive_hash, "compressed_bytes": compressed.bytes,
        "extracted_regular_bytes": stats.regular_bytes, "archive_entries": stats.entries,
        "materialized_hardlink_count": stats.materialized_hardlink_count,
        "materialized_hardlink_bytes": stats.materialized_hardlink_bytes,
        "directory_mode": "0700", "file_mode_policy": "0600 | (archive_mode & 0755)",
        "resolv_conf_sha256": format!("{:x}", Sha256::digest(resolver.as_bytes())),
    });
    write_new(
        &root.join(INSTALL_MARKER),
        &serde_json::to_vec_pretty(&marker)?,
        0o600,
    )?;
    File::open(&root)?.sync_all()?;
    File::open(&directory)?.sync_all()?;
    publish(&root, &destination).context("Publishing the complete Arch Linux root filesystem")
}

fn extract(reader: &mut impl Read, root: &Path, budget: u64) -> Result<ExtractStats> {
    let mut global = Attributes::new();
    let mut local = Attributes::new();
    let mut long_path = None;
    let mut long_link = None;
    let mut symlinks = Vec::new();
    let mut hardlinks = HashMap::new();
    let mut entries = 0;
    let mut extracted = 0_u64;
    loop {
        let mut block = [0_u8; 512];
        reader
            .read_exact(&mut block)
            .context("Truncated TAR header")?;
        if block.iter().all(|byte| *byte == 0) {
            reader
                .read_exact(&mut block)
                .context("Missing second TAR end marker")?;
            ensure!(
                block.iter().all(|byte| *byte == 0),
                "Invalid TAR end marker"
            );
            break;
        }
        entries += 1;
        ensure!(entries <= MAX_ENTRIES, "Too many archive entries");
        let header = tar::Header::from_byte_slice(&block);
        let checksum = header.cksum()? as u64;
        let unsigned: u64 = block
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if (148..156).contains(&index) {
                    32
                } else {
                    u64::from(*value)
                }
            })
            .sum();
        let signed: i64 = block
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if (148..156).contains(&index) {
                    32
                } else {
                    i64::from(*value as i8)
                }
            })
            .sum();
        ensure!(
            checksum == unsigned || (signed >= 0 && checksum == signed as u64),
            "Invalid TAR checksum"
        );
        let kind = header.entry_type().as_byte();
        let header_size = header.size()?;
        if matches!(kind, b'x' | b'g' | b'L' | b'K') {
            ensure!(header_size <= MAX_METADATA, "TAR extension exceeds 64 KiB");
            let mut bytes = vec![0; header_size as usize];
            reader.read_exact(&mut bytes)?;
            padding(reader, header_size)?;
            match kind {
                b'x' => local.extend(pax(&bytes)?),
                b'g' => global.extend(pax(&bytes)?),
                b'L' => long_path = Some(long_name(bytes)?),
                b'K' => long_link = Some(long_name(bytes)?),
                _ => unreachable!(),
            }
            continue;
        }
        let attribute = |key: &str| {
            local
                .get(key)
                .or_else(|| global.get(key))
                .filter(|value| !value.is_empty())
        };
        let size = match attribute("size") {
            Some(value) => std::str::from_utf8(value)?.parse::<u64>()?,
            None => header_size,
        };
        ensure!(size <= MAX_FILE, "Archive member exceeds 1 GiB");
        let path = match attribute("path") {
            Some(value) => PathBuf::from(OsString::from_vec(value.clone())),
            None => long_path.take().unwrap_or(header.path()?.into_owned()),
        };
        let relative = relative_path(&path)?;
        let link = match attribute("linkpath") {
            Some(value) => Some(PathBuf::from(OsString::from_vec(value.clone()))),
            None => long_link
                .take()
                .or(header.link_name()?.map(|path| path.into_owned())),
        };
        long_path = None;
        long_link = None;
        local.clear();
        if relative.as_os_str().is_empty() {
            ensure!(kind == b'5', "Invalid root archive member");
        } else {
            ensure_directories(root, relative.parent().unwrap_or(Path::new("")))?;
            let target = root.join(&relative);
            match kind {
                0 | b'0' | b'7' => {
                    extracted = extracted
                        .checked_add(size)
                        .context("Archive byte count overflow")?;
                    ensure!(
                        extracted <= budget,
                        "Root filesystem exceeds available storage or import limit"
                    );
                    let mut output = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(&target)?;
                    let copied = io::copy(&mut reader.take(size), &mut output)?;
                    ensure!(copied == size, "Truncated archive file");
                    output.set_permissions(fs::Permissions::from_mode(
                        0o600 | (header.mode()? & 0o755),
                    ))?;
                    padding(reader, size)?;
                    continue;
                }
                b'5' => create_directory(&target)?,
                b'2' => {
                    let link = link.context("Missing symlink target")?;
                    check_symlink_target(&relative, &link)?;
                    symlinks.push((relative, link));
                }
                b'1' => {
                    let link = relative_path(&link.context("Missing hardlink target")?)?;
                    ensure!(!link.as_os_str().is_empty(), "Invalid hardlink target");
                    ensure!(
                        hardlinks.insert(relative, link).is_none(),
                        "Duplicate hardlink member"
                    );
                }
                b'3' | b'4' | b'6' => {} // Devices and FIFOs are replaced by runtime binds.
                _ => bail!("Unsupported TAR entry type {kind}"),
            }
        }
        ensure!(
            io::copy(&mut reader.take(size), &mut io::sink())? == size,
            "Truncated TAR member"
        );
        padding(reader, size)?;
    }
    ensure!(
        local.is_empty() && long_path.is_none() && long_link.is_none(),
        "Dangling TAR extension"
    );
    let mut stats = ExtractStats {
        entries,
        regular_bytes: extracted,
        ..Default::default()
    };
    for (path, target) in &hardlinks {
        let source = hardlink_source(root, target, &hardlinks)?;
        ensure_directories(root, path.parent().unwrap())?;
        let remaining = budget
            .checked_sub(extracted)
            .and_then(|value| value.checked_sub(stats.materialized_hardlink_bytes))
            .context("Root filesystem storage budget exceeded")?;
        if let Some(bytes) =
            hardlink_or_copy(&source, &root.join(path), remaining, |source, target| {
                fs::hard_link(source, target)
            })?
        {
            stats.materialized_hardlink_count += 1;
            stats.materialized_hardlink_bytes += bytes;
        }
    }
    for (path, target) in symlinks {
        ensure_directories(root, path.parent().unwrap())?;
        std::os::unix::fs::symlink(target, root.join(path))?;
    }
    Ok(stats)
}

// Imported symlinks are still deferred when this helper is called.
fn hardlink_or_copy(
    source: &Path,
    target: &Path,
    remaining: u64,
    try_link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<Option<u64>> {
    match try_link(source, target) {
        Ok(()) => return Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
            ) || error.raw_os_error().is_some_and(|code| {
                [
                    libc::EPERM,
                    libc::EACCES,
                    libc::ENOSYS,
                    libc::EOPNOTSUPP,
                    libc::ENOTSUP,
                ]
                .contains(&code)
            }) => {}
        Err(error) => return Err(error).context("Creating archive hardlink"),
    }
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(source)?;
    let metadata = input.metadata()?;
    ensure!(
        metadata.is_file(),
        "Hardlink copy source is not a regular file"
    );
    let size = metadata.len();
    ensure!(
        size <= MAX_FILE && size <= remaining,
        "Hardlink copy exceeds available storage or import limit"
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(target)?;
    let copied = io::copy(&mut (&mut input).take(size), &mut output)?;
    let mut extra = [0];
    ensure!(
        copied == size && input.read(&mut extra)? == 0 && input.metadata()?.len() == size,
        "Hardlink copy source changed during import"
    );
    output.set_permissions(fs::Permissions::from_mode(
        0o600 | (metadata.mode() & 0o755),
    ))?;
    Ok(Some(copied))
}

fn padding(reader: &mut impl Read, size: u64) -> Result<()> {
    let remaining = (512 - size % 512) % 512;
    ensure!(
        io::copy(&mut reader.take(remaining), &mut io::sink())? == remaining,
        "Truncated TAR padding"
    );
    Ok(())
}

fn pax(bytes: &[u8]) -> Result<Attributes> {
    let mut attributes = Attributes::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let space = bytes[offset..]
            .iter()
            .position(|byte| *byte == b' ')
            .context("Invalid PAX record")?
            + offset;
        let size: usize = std::str::from_utf8(&bytes[offset..space])?.parse()?;
        let end = offset
            .checked_add(size)
            .context("PAX record size overflow")?;
        ensure!(
            end <= bytes.len() && end > space + 1 && bytes[end - 1] == b'\n',
            "Invalid PAX record length"
        );
        let record = &bytes[space + 1..end - 1];
        let separator = record
            .iter()
            .position(|byte| *byte == b'=')
            .context("Invalid PAX attribute")?;
        let name = std::str::from_utf8(&record[..separator])?;
        ensure!(
            !name.starts_with("GNU.sparse."),
            "Sparse TAR members are unsupported"
        );
        if matches!(name, "path" | "linkpath" | "size") {
            attributes.insert(name.into(), record[separator + 1..].to_vec());
        }
        offset = end;
    }
    Ok(attributes)
}

fn long_name(mut bytes: Vec<u8>) -> Result<PathBuf> {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    ensure!(!bytes.contains(&0), "Invalid GNU long name");
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn relative_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.as_os_str().as_bytes().len() <= 4096,
        "Archive path is too long"
    );
    let mut result = PathBuf::new();
    let mut depth = 0;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => {
                result.push(value);
                depth += 1;
            }
            _ => bail!("Archive path escapes the root filesystem"),
        }
    }
    ensure!(
        depth <= 128 && !path.as_os_str().as_bytes().contains(&0),
        "Invalid archive path"
    );
    Ok(result)
}

fn check_symlink_target(path: &Path, target: &Path) -> Result<()> {
    ensure!(
        !target.as_os_str().is_empty()
            && target.as_os_str().as_bytes().len() <= 4096
            && !target.as_os_str().as_bytes().contains(&0),
        "Invalid symlink target"
    );
    let mut depth = path.parent().unwrap().components().count();
    for component in target.components() {
        match component {
            Component::RootDir => depth = 0,
            Component::CurDir => {}
            Component::Normal(_) => {
                depth += 1;
                ensure!(depth <= 128, "Symlink target is too deep");
            }
            Component::ParentDir => {
                ensure!(depth > 0, "Symlink escapes the guest root");
                depth -= 1;
            }
            Component::Prefix(_) => bail!("Invalid symlink prefix"),
        }
    }
    Ok(())
}

fn hardlink_source(
    root: &Path,
    target: &Path,
    links: &HashMap<PathBuf, PathBuf>,
) -> Result<PathBuf> {
    let mut current = target;
    for _ in 0..64 {
        if let Some(next) = links.get(current) {
            current = next;
            continue;
        }
        ensure_directories(root, current.parent().unwrap())?;
        let source = root.join(current);
        ensure!(
            fs::symlink_metadata(&source)?.is_file(),
            "Hardlink target is not a regular file"
        );
        return Ok(source);
    }
    bail!("Cyclic or excessive hardlink chain")
}

fn check_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
        "Rootfs directory is not an owned regular directory"
    );
    Ok(())
}

fn create_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => check_directory(path)?,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_directories(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "Invalid directory component"
        );
        path.push(component);
        create_directory(&path)?;
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn replace_guest_file(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let relative = relative_path(Path::new(name))?;
    ensure_directories(root, relative.parent().unwrap())?;
    let path = root.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                !metadata.is_dir(),
                "Guest configuration path is a directory"
            );
            fs::remove_file(&path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    write_new(&path, bytes, 0o600)
}

fn guest_file(root: &Path, name: &Path) -> Result<PathBuf> {
    let mut pending: VecDeque<OsString> = name
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            _ => None,
        })
        .collect();
    let mut path = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            ensure!(path.pop(), "Guest path escapes root");
            continue;
        }
        let candidate = root.join(&path).join(&part);
        let metadata = fs::symlink_metadata(&candidate)?;
        if metadata.file_type().is_symlink() {
            links += 1;
            ensure!(links <= 40, "Guest symlink cycle");
            let target = fs::read_link(candidate)?;
            if target.is_absolute() {
                path.clear();
            }
            let parts: Vec<_> = target
                .components()
                .filter_map(|component| match component {
                    Component::Normal(value) => Some(value.to_owned()),
                    Component::ParentDir => Some(OsString::from("..")),
                    _ => None,
                })
                .collect();
            for part in parts.into_iter().rev() {
                pending.push_front(part);
            }
        } else {
            ensure!(
                pending.is_empty() || metadata.is_dir(),
                "Guest path traverses a non-directory"
            );
            path.push(part);
        }
    }
    let result = root.join(path);
    ensure!(
        fs::symlink_metadata(&result)?.is_file(),
        "Required guest path is not a regular file"
    );
    Ok(result)
}

fn elf_interpreter(path: &Path) -> Result<Option<PathBuf>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut header = [0; 64];
    file.read_exact(&mut header)?;
    ensure!(
        &header[..6] == b"\x7fELF\x02\x01"
            && u16::from_le_bytes(header[18..20].try_into().unwrap()) == 183,
        "Guest executable must be little-endian AArch64 ELF64"
    );
    ensure!(
        matches!(
            u16::from_le_bytes(header[16..18].try_into().unwrap()),
            2 | 3
        ),
        "Invalid guest ELF type"
    );
    let start = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let size = u16::from_le_bytes(header[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into().unwrap()) as u64;
    ensure!(
        size >= 56
            && size <= 256
            && count <= 128
            && start
                .checked_add(size * count)
                .is_some_and(|end| end <= length),
        "Invalid guest ELF program table"
    );
    let mut interpreter = None;
    for index in 0..count {
        file.seek(SeekFrom::Start(start + size * index))?;
        let mut program = [0; 56];
        file.read_exact(&mut program)?;
        if u32::from_le_bytes(program[..4].try_into().unwrap()) != 3 {
            continue;
        }
        ensure!(interpreter.is_none(), "Duplicate guest ELF interpreter");
        let offset = u64::from_le_bytes(program[8..16].try_into().unwrap());
        let bytes = u64::from_le_bytes(program[32..40].try_into().unwrap());
        ensure!(
            (2..=4096).contains(&bytes)
                && offset.checked_add(bytes).is_some_and(|end| end <= length),
            "Invalid guest ELF interpreter"
        );
        let mut value = vec![0; bytes as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut value)?;
        ensure!(
            value.pop() == Some(0) && !value.contains(&0),
            "Invalid guest ELF interpreter string"
        );
        interpreter = Some(PathBuf::from(OsString::from_vec(value)));
    }
    Ok(interpreter)
}

fn validate_arch(root: &Path) -> Result<()> {
    for name in ["usr/bin/bash", "usr/bin/pacman"] {
        let path = guest_file(root, Path::new(name))
            .with_context(|| format!("Required Arch program {name} is missing"))?;
        ensure!(
            fs::metadata(&path)?.permissions().mode() & 0o100 != 0,
            "Guest program is not executable"
        );
        let interpreter =
            elf_interpreter(&path)?.context("Guest program must use the glibc dynamic loader")?;
        ensure!(
            interpreter.is_absolute()
                && interpreter
                    .file_name()
                    .is_some_and(|name| name == "ld-linux-aarch64.so.1"),
            "Guest program does not use AArch64 glibc"
        );
        let loader = guest_file(root, &interpreter)?;
        elf_interpreter(&loader)?;
    }
    let libc = guest_file(root, Path::new("usr/lib/libc.so.6"))?;
    elf_interpreter(&libc)?;
    guest_file(root, Path::new("etc/pacman.conf"))?;
    Ok(())
}

fn clean_stages(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_name().as_bytes().starts_with(b".import-") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let owner = path.join("owner");
        let owned = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&owner)
            .and_then(|file| {
                let mut value = String::new();
                file.take(128).read_to_string(&mut value)?;
                Ok(value == STAGE_MARKER)
            })
            .unwrap_or(false);
        if owned {
            fs::remove_dir_all(path)?;
        } else {
            let _ = fs::remove_dir(path);
        } // Only an empty pre-marker stage can be removed.
    }
    Ok(())
}

fn available_bytes(path: &Path) -> Result<u64> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "Cannot inspect available rootfs storage"
    );
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

fn publish(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            1_u32,
        )
    };
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    ensure!(
        result == 0,
        "Cannot publish rootfs without replacing an existing installation: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(test)]
mod hardlink_copy_tests {
    use super::*;

    #[test]
    fn permission_and_unsupported_link_errors_copy_bytes_and_mode() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        write_new(&source, b"payload", 0o755).unwrap();
        for code in [libc::EACCES, libc::EOPNOTSUPP] {
            let target = directory.path().join(format!("copy-{code}"));
            let copied = hardlink_or_copy(&source, &target, 7, |_, _| {
                Err(io::Error::from_raw_os_error(code))
            })
            .unwrap();
            assert_eq!(copied, Some(7));
            assert_eq!(fs::read(&target).unwrap(), b"payload");
            assert_ne!(
                fs::metadata(&source).unwrap().ino(),
                fs::metadata(&target).unwrap().ino()
            );
            assert_eq!(fs::metadata(&target).unwrap().mode() & 0o7777, 0o755);
        }
    }

    #[test]
    fn copy_storage_limit_is_checked_before_creating_output() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        write_new(&source, b"payload", 0o600).unwrap();
        assert!(hardlink_or_copy(&source, &target, 6, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EPERM))
        })
        .is_err());
        assert!(!target.exists());
    }

    #[test]
    fn unrelated_link_error_preserves_existing_output() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        write_new(&source, b"payload", 0o600).unwrap();
        write_new(&target, b"existing", 0o600).unwrap();
        assert!(hardlink_or_copy(&source, &target, 7, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EEXIST))
        })
        .is_err());
        assert_eq!(fs::read(target).unwrap(), b"existing");
    }

    #[test]
    fn copy_does_not_follow_source_or_destination_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let alias = directory.path().join("alias");
        let target = directory.path().join("target");
        write_new(&source, b"payload", 0o600).unwrap();
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        assert!(hardlink_or_copy(&alias, &target, 7, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .is_err());
        assert!(!target.exists());
        std::os::unix::fs::symlink(&source, &target).unwrap();
        assert!(hardlink_or_copy(&source, &target, 7, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .is_err());
        assert_eq!(fs::read(source).unwrap(), b"payload");
    }
}

// FVPROV1 31381 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNzRlZjBlZGZhOGI4NDliODRiOWFlNGZhMjVjYWViNjI3NDkwOWIxY2JjZTNhZjg0ZjE2N2ZlOTM3MjU4YzYwNiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNTA0OThjNGY4MzMyOTg0ZmExYzNmMDE4ODY1NzQ4YzE4MGJlYjEyY2YyOWZkZjk3ODZhYjZjZDdhYTVlNGRkYWZkZTRlNTFhYTliMDMxMDZkNzg1NTJlMGZjMmIyNWRiMDMwZDc4NjE5NjE2NDE5MWJmMmE0YzIxZWRlNTkyMDEifV19XQ== FVPROV1-END
