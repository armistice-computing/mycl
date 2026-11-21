//! Disposable handle-owned snapshots. No extraction or cleanup paths are exposed.
use crate::vault_preflight::{Admission, Kind, Limits};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
};

const COMPRESSED_LIMIT: usize = 256 * 1024 * 1024;
const CENTRAL_DIRECTORY_LIMIT: usize = 16 * 1024 * 1024;
const EXTRA_LIMIT: usize = 4096;

#[derive(Debug)]
pub(crate) struct Entry {
    pub path: String,
    pub kind: Kind,
    pub bytes: u64,
    pub sha256: String,
    // Offset in the private spool, never a path supplied to a consumer.
    blob: Option<u64>,
}
#[derive(Debug)]
pub(crate) struct Snapshot {
    data: File,
    entries: Vec<Entry>,
}
impl Snapshot {
    fn new() -> Result<Self, String> {
        // tempfile unlinks on Unix and uses exclusive delete-on-close on Windows.
        // The process's OS temporary directory is trusted; imported paths are not.
        Ok(Self {
            data: tempfile::tempfile().map_err(|e| e.to_string())?,
            entries: vec![],
        })
    }
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(crate) fn copy_entry(
        &mut self,
        index: usize,
        output: &mut impl Write,
    ) -> Result<(), String> {
        let e = self.entries.get(index).ok_or("unknown staged entry")?;
        let offset = e.blob.ok_or("entry is a directory")?;
        self.data
            .seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let copied = std::io::copy(&mut (&mut self.data).take(e.bytes), output)
            .map_err(|e| e.to_string())?;
        if copied != e.bytes {
            return Err("truncated staging spool".into());
        }
        Ok(())
    }
}

fn check_cancel(cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("cancelled".into())
    } else {
        Ok(())
    }
}

// Copy first, then parse an immutable buffer. A changing file cannot extend the
// parser's input after admission. This is a capture window, not an atomic snapshot.
fn capture_zip(
    source: &mut File,
    cap: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<u8>, String> {
    check_cancel(cancelled)?;
    let before = source.metadata().map_err(|e| e.to_string())?;
    if !before.is_file() {
        return Err("ZIP source must be a regular file".into());
    }
    if before.len() > cap as u64 {
        return Err("compressed byte limit exceeded".into());
    }
    source.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(before.len() as usize)
        .map_err(|_| "ZIP capture allocation failed")?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancel(cancelled)?;
        // One byte beyond the bound detects growth without a large allocation.
        let want = buffer.len().min(cap - bytes.len() + 1);
        let n = source
            .read(&mut buffer[..want])
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if n > cap - bytes.len() {
            return Err("compressed byte limit exceeded".into());
        }
        bytes
            .try_reserve_exact(n)
            .map_err(|_| "ZIP capture allocation failed")?;
        bytes.extend_from_slice(&buffer[..n]);
    }
    let after = source.metadata().map_err(|e| e.to_string())?;
    if before.len() != after.len()
        || bytes.len() as u64 != after.len()
        || before.modified().map_err(|e| e.to_string())?
            != after.modified().map_err(|e| e.to_string())?
    {
        return Err("ZIP source changed during capture".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.ctime(), before.ctime_nsec()) != (after.ctime(), after.ctime_nsec()) {
            return Err("ZIP source changed during capture".into());
        }
    }
    check_cancel(cancelled)?;
    Ok(bytes)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

// Bound metadata BEFORE ZipArchive allocates its index. This first importer
// accepts ordinary single-disk ZIP32 only: ZIP64 and ambiguous layouts fail closed.
fn preflight_zip(bytes: &[u8], limits: Limits) -> Result<(), String> {
    if bytes.len() < 22 {
        return Err("truncated ZIP".into());
    }
    let start = bytes.len().saturating_sub(22 + u16::MAX as usize);
    let end = (start..=bytes.len() - 22)
        .rev()
        .find(|&i| {
            bytes[i..i + 4] == *b"PK\x05\x06"
                && i + 22 + u16_at(bytes, i + 20) as usize == bytes.len()
        })
        .ok_or("missing or ambiguous ZIP end record")?;
    if bytes[end + 22..].windows(4).any(|w| w == b"PK\x05\x06") {
        return Err("ambiguous ZIP end record in comment".into());
    }
    let count = u16_at(bytes, end + 10) as usize;
    let size = u32_at(bytes, end + 12) as usize;
    let offset = u32_at(bytes, end + 16) as usize;
    if count == u16::MAX as usize
        || size == u32::MAX as usize
        || offset == u32::MAX as usize
        || (end >= 20 && bytes[end - 20..end - 16] == *b"PK\x06\x07")
    {
        return Err("ZIP64 is not supported by bounded vault staging".into());
    }
    if u16_at(bytes, end + 4) != 0
        || u16_at(bytes, end + 6) != 0
        || u16_at(bytes, end + 8) as usize != count
    {
        return Err("multi-disk ZIP refused".into());
    }
    if count > limits.entries {
        return Err("entry limit exceeded".into());
    }
    if size > CENTRAL_DIRECTORY_LIMIT {
        return Err("ZIP metadata byte limit exceeded".into());
    }
    if offset.checked_add(size) != Some(end) {
        return Err("invalid ZIP central directory extent".into());
    }
    let mut pos = offset;
    let mut admission = Admission::new(limits);
    for _ in 0..count {
        if pos.checked_add(46).filter(|&n| n <= end).is_none()
            || bytes[pos..pos + 4] != *b"PK\x01\x02"
        {
            return Err("invalid ZIP central directory entry".into());
        }
        let name = u16_at(bytes, pos + 28) as usize;
        if u32_at(bytes, pos + 20) == u32::MAX || u32_at(bytes, pos + 24) == u32::MAX {
            return Err("ZIP64 entry sizes are unsupported".into());
        }
        // Other codecs can allocate large dictionaries before yielding bytes.
        if !matches!(u16_at(bytes, pos + 10), 0 | 8) {
            return Err("unsupported ZIP compression; use stored or DEFLATE entries".into());
        }
        if u16_at(bytes, pos + 8) & (1 | 64) != 0 {
            return Err("encrypted ZIP entries are unsupported".into());
        }
        let extra = u16_at(bytes, pos + 30) as usize;
        let comment = u16_at(bytes, pos + 32) as usize;
        if name > limits.path_bytes || extra > EXTRA_LIMIT {
            return Err("ZIP entry metadata limit exceeded".into());
        }
        if u16_at(bytes, pos + 34) != 0 || u32_at(bytes, pos + 42) as usize >= offset {
            return Err("invalid ZIP local entry offset or disk".into());
        }
        let next = pos
            .checked_add(46 + name + extra + comment)
            .filter(|&n| n <= end)
            .ok_or("truncated ZIP metadata")?;
        let raw_name = &bytes[pos + 46..pos + 46 + name];
        if !raw_name.is_ascii() && u16_at(bytes, pos + 8) & (1 << 11) == 0 {
            return Err("non-ASCII ZIP names must declare UTF-8".into());
        }
        let name = std::str::from_utf8(raw_name).map_err(|_| "non-UTF8 ZIP filename refused")?;
        // ZipArchive can coalesce exact duplicate names while building its index.
        // Reject them (and aliases) before that information can disappear.
        admission.admit(name, name.ends_with('/'))?;
        let name = raw_name.len();
        let mut e = pos + 46 + name;
        let extra_end = e + extra;
        while e < extra_end {
            if e + 4 > extra_end {
                return Err("truncated ZIP extra field".into());
            }
            if u16_at(bytes, e) == 1 {
                return Err("ZIP64 extra fields are unsupported".into());
            }
            if u16_at(bytes, e) == 0x7075 {
                return Err("ZIP Unicode path overrides are unsupported; use UTF-8 names".into());
            }
            e = e
                .checked_add(4 + u16_at(bytes, e + 2) as usize)
                .filter(|&n| n <= extra_end)
                .ok_or("truncated ZIP extra field")?;
        }
        pos = next;
    }
    if pos != end {
        return Err("ZIP entry count or metadata length mismatch".into());
    }
    Ok(())
}

pub(crate) fn stage_zip(
    source: &mut File,
    limits: Limits,
    cancelled: impl Fn() -> bool,
) -> Result<Snapshot, String> {
    let bytes = capture_zip(source, COMPRESSED_LIMIT, &cancelled)?;
    preflight_zip(&bytes, limits)?;
    check_cancel(&cancelled)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    if archive.len() > limits.entries {
        return Err("entry limit exceeded".into());
    }
    let mut snapshot = Snapshot::new()?;
    let mut admission = Admission::new(limits);
    let mut buffer = [0u8; 64 * 1024];
    for index in 0..archive.len() {
        check_cancel(&cancelled)?;
        let mut file = archive.by_index(index).map_err(|e| e.to_string())?;
        let directory = file.is_dir();
        if let Some(mode) = file.unix_mode() {
            let ty = mode & 0o170000;
            if ty != 0 && ty != if directory { 0o040000 } else { 0o100000 } {
                return Err("symlink or special file refused".into());
            }
        }
        let (path, kind) = admission.admit(file.name(), directory)?;
        let offset = snapshot.data.stream_position().map_err(|e| e.to_string())?;
        let mut bytes = 0;
        let mut digest = Sha256::new();
        loop {
            check_cancel(&cancelled)?;
            let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            admission.account(kind, &mut bytes, n)?;
            digest.update(&buffer[..n]);
            snapshot
                .data
                .write_all(&buffer[..n])
                .map_err(|e| e.to_string())?;
        }
        snapshot.entries.push(Entry {
            path,
            kind,
            bytes,
            sha256: format!("{:x}", digest.finalize()),
            blob: (!directory).then_some(offset),
        });
    }
    check_cancel(&cancelled)?;
    Ok(snapshot)
}

/// Source authority belongs to the caller; descendants are opened without following links.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn stage_folder(
    root: &File,
    limits: Limits,
    cancelled: impl Fn() -> bool,
) -> Result<Snapshot, String> {
    stage_folder_excluding(root, limits, cancelled, None)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn stage_folder_excluding(root: &File, limits: Limits, cancelled: impl Fn() -> bool, excluded: Option<(u64, u64)>) -> Result<Snapshot, String> {
    check_cancel(&cancelled)?;
    if !root.metadata().map_err(|e| e.to_string())?.is_dir() {
        return Err("source is not a directory".into());
    }
    let mut snapshot = Snapshot::new()?;
    let mut admission = Admission::new(limits);
    walk_folder(root, "", &mut snapshot, &mut admission, &cancelled, excluded)?;
    check_cancel(&cancelled)?;
    Ok(snapshot)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn stage_folder(_: &File, _: Limits, _: impl Fn() -> bool) -> Result<Snapshot, String> {
    Err("safe folder capture is not implemented on this platform".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn stage_folder_excluding(_: &File, _: Limits, _: impl Fn() -> bool, _: Option<(u64, u64)>) -> Result<Snapshot, String> {
    Err("safe folder capture is not implemented on this platform".into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn walk_folder(
    dir: &File,
    prefix: &str,
    snapshot: &mut Snapshot,
    admission: &mut Admission,
    cancelled: &impl Fn() -> bool,
    excluded: Option<(u64, u64)>,
) -> Result<(), String> {
    use std::{
        ffi::{CStr, CString},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };
    let identity = dir.metadata().map_err(|e| e.to_string())?;
    if excluded == Some((identity.dev(), identity.ino())) {
        return Err("source contains the owner's private profile".into());
    }
    // fdopendir owns the descriptor. Opening '.' gives an independent cursor,
    // unlike dup(), which would share the caller's directory position.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(std::io::Error::last_os_error().to_string());
    }
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Dir(stream);
    loop {
        if cancelled() {
            return Err("cancelled".into());
        }
        // readdir reports EOF and errors with the same pointer; clear errno.
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error().unwrap_or(0) != 0 {
                return Err(err.to_string());
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let name_str = name.to_str().map_err(|_| "non-UTF8 filename refused")?;
        let path = if prefix.is_empty() {
            name_str.to_string()
        } else {
            format!("{prefix}/{name_str}")
        };
        let name =
            CString::new(std::ffi::OsStr::new(name_str).as_bytes()).map_err(|e| e.to_string())?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let ty = stat.st_mode & libc::S_IFMT;
        if ty != libc::S_IFDIR && ty != libc::S_IFREG {
            return Err("symlink or special file refused".into());
        }
        let (path, kind) = admission.admit(&path, ty == libc::S_IFDIR)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if kind == Kind::Directory {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut source = unsafe { File::from_raw_fd(fd) };
        let metadata = source.metadata().map_err(|e| e.to_string())?;
        if metadata.dev() != stat.st_dev as u64 || metadata.ino() != stat.st_ino as u64 {
            return Err("source changed during open".into());
        }
        if kind == Kind::Directory {
            snapshot.entries.push(Entry {
                path: path.clone(),
                kind,
                bytes: 0,
                sha256: String::new(),
                blob: None,
            });
            walk_folder(&source, &path, snapshot, admission, cancelled, excluded)?;
        } else {
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err("special or hard-linked file refused".into());
            }
            let offset = snapshot.data.stream_position().map_err(|e| e.to_string())?;
            let mut buffer = [0u8; 64 * 1024];
            let mut bytes = 0;
            let mut digest = Sha256::new();
            loop {
                if cancelled() {
                    return Err("cancelled".into());
                }
                let n = source.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                admission.account(kind, &mut bytes, n)?;
                snapshot
                    .data
                    .write_all(&buffer[..n])
                    .map_err(|e| e.to_string())?;
                digest.update(&buffer[..n]);
            }
            let after = source.metadata().map_err(|e| e.to_string())?;
            if after.len() != metadata.len()
                || after.mtime() != metadata.mtime()
                || after.mtime_nsec() != metadata.mtime_nsec()
                || after.ctime() != metadata.ctime()
                || after.ctime_nsec() != metadata.ctime_nsec()
            {
                return Err("source changed during capture".into());
            }
            snapshot.entries.push(Entry {
                path,
                kind,
                bytes,
                sha256: format!("{:x}", digest.finalize()),
                blob: Some(offset),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs, io::Seek, path::Path};
    fn fixture(root: &Path, names: &[(&str, &[u8])]) -> File {
        let path = root.join("input.zip");
        let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
        for (name, bytes) in names {
            z.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(bytes).unwrap();
        }
        z.finish().unwrap();
        File::open(path).unwrap()
    }
    fn read(snapshot: &mut Snapshot, index: usize) -> Vec<u8> {
        let mut out = vec![];
        snapshot.copy_entry(index, &mut out).unwrap();
        out
    }
    #[test]
    fn bytes_metadata_links_and_blob_ranges_survive() {
        let t = tempfile::tempdir().unwrap();
        let body = b"---\ntitle: Example\nunknown: keep-me\n---\n[[Other]]\n";
        let mut f = fixture(
            t.path(),
            &[
                ("Notes/Example.md", body),
                ("attachments/picture.bin", &[0, 255, 1]),
            ],
        );
        let mut s = stage_zip(&mut f, Limits::default(), || false).unwrap();
        assert_eq!(s.entries()[0].path, "Notes/Example.md");
        assert_eq!(s.entries()[0].kind, Kind::Note);
        assert_eq!(s.entries()[0].sha256, format!("{:x}", Sha256::digest(body)));
        assert_eq!(read(&mut s, 1), [0, 255, 1]);
        assert_eq!(read(&mut s, 0), body);
        assert_eq!(read(&mut s, 1), [0, 255, 1]);
        assert!(s.copy_entry(2, &mut vec![]).is_err());
        fs::write(t.path().join("input.zip"), b"changed after capture").unwrap();
        assert_eq!(read(&mut s, 0), body);
    }
    #[test]
    fn unsafe_names_limits_and_cancellation_fail_without_path_cleanup() {
        let t = tempfile::tempdir().unwrap();
        let keep = t.path().join("keep");
        fs::write(&keep, b"owner").unwrap();
        for names in [
            vec![
                ("ok.md", b"hello".as_slice()),
                ("../escape", b"bad".as_slice()),
            ],
            vec![("same.md", b"a".as_slice()), ("SAME.md", b"b".as_slice())],
        ] {
            let mut f = fixture(t.path(), &names);
            assert!(stage_zip(&mut f, Limits::default(), || false).is_err());
        }
        let mut f = fixture(t.path(), &[("a.md", b"hello")]);
        assert!(stage_zip(
            &mut f,
            Limits {
                note_bytes: 2,
                ..Limits::default()
            },
            || false
        )
        .is_err());
        assert!(stage_zip(&mut f, Limits::default(), || true).is_err());
        assert_eq!(fs::read(keep).unwrap(), b"owner");
    }
    #[test]
    fn every_cancellation_boundary_is_observed() {
        let t = tempfile::tempdir().unwrap();
        let mut f = fixture(t.path(), &[("a.md", b"one"), ("b.md", b"two")]);
        let total = Cell::new(0);
        drop(
            stage_zip(&mut f, Limits::default(), || {
                total.set(total.get() + 1);
                false
            })
            .unwrap(),
        );
        for stop in 1..=total.get() {
            let calls = Cell::new(0);
            let error = stage_zip(&mut f, Limits::default(), || {
                calls.set(calls.get() + 1);
                calls.get() >= stop
            })
            .unwrap_err();
            assert_eq!(error, "cancelled", "boundary {stop}");
        }
    }
    #[test]
    fn capture_bounds_actual_bytes_and_rewinds_input() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("raw");
        fs::write(&p, b"1234").unwrap();
        let mut f = File::open(&p).unwrap();
        f.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(capture_zip(&mut f, 4, &|| false).unwrap(), b"1234");
        assert!(capture_zip(&mut f, 3, &|| false)
            .unwrap_err()
            .contains("limit"));
        let calls = Cell::new(0);
        let e = capture_zip(&mut f, 4, &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                fs::write(&p, b"12345").unwrap();
            }
            false
        })
        .unwrap_err();
        assert!(e.contains("limit"));
    }
    #[test]
    fn changing_source_is_rejected_and_captured_buffer_is_independent() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("raw");
        fs::write(&p, vec![7; 100000]).unwrap();
        let mut f = File::open(&p).unwrap();
        let calls = Cell::new(0);
        let e = capture_zip(&mut f, 200000, &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                fs::write(&p, b"short").unwrap();
            }
            false
        })
        .unwrap_err();
        assert!(e.contains("changed"));
        let captured = capture_zip(&mut f, 200000, &|| false).unwrap();
        fs::write(&p, b"other").unwrap();
        assert_eq!(captured, b"short");
    }
    #[test]
    fn preparse_bounds_counts_metadata_and_zip64() {
        let t = tempfile::tempdir().unwrap();
        let mut f = fixture(t.path(), &[("a.md", b"x")]);
        let bytes = capture_zip(&mut f, 10000, &|| false).unwrap();
        assert!(preflight_zip(&bytes, Limits::default()).is_ok());
        assert!(preflight_zip(
            &bytes,
            Limits {
                entries: 0,
                ..Limits::default()
            }
        )
        .unwrap_err()
        .contains("entry limit"));
        let end = bytes.len() - 22;
        let mut bad = bytes.clone();
        bad[end + 12..end + 16]
            .copy_from_slice(&((CENTRAL_DIRECTORY_LIMIT + 1) as u32).to_le_bytes());
        assert!(preflight_zip(&bad, Limits::default())
            .unwrap_err()
            .contains("metadata byte"));
        let mut bad = bytes.clone();
        bad[end + 10..end + 12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(preflight_zip(&bad, Limits::default())
            .unwrap_err()
            .contains("ZIP64"));
        let mut bad = bytes.clone();
        bad[end + 8..end + 12].copy_from_slice(&[0, 0, 0, 0]);
        assert!(preflight_zip(&bad, Limits::default()).is_err());
        for n in 0..bytes.len() {
            assert!(preflight_zip(&bytes[..n], Limits::default()).is_err());
        }
    }
    #[test]
    fn symlink_and_corrupt_zip_refused() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("link.zip");
        let mut z = zip::ZipWriter::new(File::create(&p).unwrap());
        z.add_symlink(
            "outside",
            "/etc/passwd",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.finish().unwrap();
        assert!(stage_zip(&mut File::open(&p).unwrap(), Limits::default(), || false).is_err());
        fs::write(&p, b"broken").unwrap();
        assert!(stage_zip(&mut File::open(&p).unwrap(), Limits::default(), || false).is_err());
    }
    #[test]
    fn duplicate_central_names_and_end_record_decoys_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let mut f = fixture(t.path(), &[("a.md", b"one"), ("b.md", b"two")]);
        let bytes = capture_zip(&mut f, 10000, &|| false).unwrap();
        let end = bytes.len() - 22;
        let first = u32_at(&bytes, end + 16) as usize;
        let second = first
            + 46
            + u16_at(&bytes, first + 28) as usize
            + u16_at(&bytes, first + 30) as usize
            + u16_at(&bytes, first + 32) as usize;
        let mut duplicate = bytes.clone();
        duplicate[second + 46..second + 50].copy_from_slice(b"a.md");
        assert!(preflight_zip(&duplicate, Limits::default()).is_err());
        let mut codec = bytes.clone();
        codec[first + 10..first + 12].copy_from_slice(&93u16.to_le_bytes());
        assert!(preflight_zip(&codec, Limits::default())
            .unwrap_err()
            .contains("compression"));
        let mut encrypted = bytes.clone();
        encrypted[first + 8] |= 1;
        assert!(preflight_zip(&encrypted, Limits::default())
            .unwrap_err()
            .contains("encrypted"));
        let mut comment = bytes.clone();
        comment[end + 20..end + 22].copy_from_slice(&24u16.to_le_bytes());
        comment.extend_from_slice(b"PK\x05\x06");
        comment.extend_from_slice(&[0; 20]);
        assert!(preflight_zip(&comment, Limits::default())
            .unwrap_err()
            .contains("ambiguous"));
        let mut oversized = bytes;
        oversized[first + 30..first + 32]
            .copy_from_slice(&((EXTRA_LIMIT + 1) as u16).to_le_bytes());
        assert!(preflight_zip(&oversized, Limits::default())
            .unwrap_err()
            .contains("metadata limit"));
    }

    #[test]
    fn directories_unicode_and_archive_comments_work() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("tree.zip");
        let mut z = zip::ZipWriter::new(File::create(&path).unwrap());
        z.add_directory("Notes/", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.start_file(
            "Notes/caf\u{e9}.md",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.write_all(b"contents").unwrap();
        z.set_comment("an ordinary archive comment").unwrap();
        z.finish().unwrap();
        let mut snapshot =
            stage_zip(&mut File::open(path).unwrap(), Limits::default(), || false).unwrap();
        assert_eq!(snapshot.entries()[0].kind, Kind::Directory);
        assert!(snapshot.copy_entry(0, &mut vec![]).is_err());
        assert_eq!(snapshot.entries()[1].path, "Notes/caf\u{e9}.md");
        assert_eq!(read(&mut snapshot, 1), b"contents");
    }

    #[test]
    fn zip64_entry_size_sentinels_fail_before_parsing_and_staging() {
        let t = tempfile::tempdir().unwrap();
        let mut source = fixture(t.path(), &[("a.md", b"x")]);
        let bytes = capture_zip(&mut source, 10000, &|| false).unwrap();
        let central = u32_at(&bytes, bytes.len() - 22 + 16) as usize;
        for field in [20, 24] {
            let mut mutated = bytes.clone();
            mutated[central + field..central + field + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(preflight_zip(&mutated, Limits::default())
                .unwrap_err()
                .contains("ZIP64 entry sizes"));
            let path = t.path().join(format!("size-{field}.zip"));
            fs::write(&path, mutated).unwrap();
            assert!(
                stage_zip(&mut File::open(path).unwrap(), Limits::default(), || false)
                    .unwrap_err()
                    .contains("ZIP64 entry sizes")
            );
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn folder_detects_mutation_and_cancellation_at_all_boundaries() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("a.md");
        fs::write(&path, vec![1; 100000]).unwrap();
        let root = File::open(t.path()).unwrap();
        let total = Cell::new(0);
        drop(
            stage_folder(&root, Limits::default(), || {
                total.set(total.get() + 1);
                false
            })
            .unwrap(),
        );
        for stop in 1..=total.get() {
            let calls = Cell::new(0);
            assert_eq!(
                stage_folder(&root, Limits::default(), || {
                    calls.set(calls.get() + 1);
                    calls.get() >= stop
                })
                .unwrap_err(),
                "cancelled"
            );
        }
        let mut attempts = 0;
        // Find a callback inside the read loop rather than relying on readdir's
        // dot-entry behavior, which differs between supported Unix platforms.
        for change_at in 1..=total.get() {
            fs::write(&path, vec![1; 100000]).unwrap();
            let calls = Cell::new(0);
            let result = stage_folder(&root, Limits::default(), || {
                calls.set(calls.get() + 1);
                if calls.get() == change_at {
                    fs::write(&path, b"short").unwrap();
                }
                false
            });
            if matches!(result, Err(ref e) if e.contains("changed during capture")) {
                attempts += 1;
            }
        }
        assert!(attempts > 0);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn snapshot_has_no_cleanup_name_and_survives_source_path_replacement() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("source");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.md"), b"original").unwrap();
        let f = File::open(&root).unwrap();
        let moved = t.path().join("moved");
        let fired = Cell::new(false);
        let mut s = stage_folder(&f, Limits::default(), || {
            if !fired.replace(true) {
                fs::rename(&root, &moved).unwrap();
                fs::create_dir(&root).unwrap();
                fs::write(root.join("keep"), b"replacement").unwrap();
            }
            false
        })
        .unwrap();
        assert_eq!(s.data.metadata().unwrap().nlink(), 0);
        assert_eq!(read(&mut s, 0), b"original");
        drop(s);
        assert_eq!(fs::read(root.join("keep")).unwrap(), b"replacement");
        assert_eq!(fs::read(moved.join("a.md")).unwrap(), b"original");
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn folder_links_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = File::open(t.path()).unwrap();
        fs::write(t.path().join("a.md"), b"hello").unwrap();
        let mut s = stage_folder(&root, Limits::default(), || false).unwrap();
        assert_eq!(read(&mut s, 0), b"hello");
        std::os::unix::fs::symlink("/etc/passwd", t.path().join("escape")).unwrap();
        assert!(stage_folder(&root, Limits::default(), || false).is_err());
        fs::remove_file(t.path().join("escape")).unwrap();
        fs::hard_link(t.path().join("a.md"), t.path().join("alias.md")).unwrap();
        assert!(stage_folder(&root, Limits::default(), || false).is_err());
    }
}
