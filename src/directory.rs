//! Directory store (P2a): one database as a directory holding an optional
//! checkpoint image plus write-ahead log segments, listed in order by an
//! atomically replaced `MANIFEST`.
//!
//! The listed files, each later file minus its own header, concatenate into
//! exactly the byte stream of a single-file database. Replay, the change
//! feed, replication and the maintenance tools therefore read a directory
//! through the same code paths as a file. Commits append to the last log
//! segment only; earlier segments are immutable.
//!
//! A checkpoint seals the active log (sync, then publish a manifest with a
//! fresh empty log), replays the sealed prefix on a background thread into a
//! `FLAG_HISTORY` image whose floor is the last sealed commit, and installs
//! it with one more manifest publication. Every crash point leaves either
//! the old or the new manifest, and both describe a complete database.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use tempfile::NamedTempFile;

use crate::crypto::{Cipher, DESCRIPTOR_SIZE};
use crate::format::{self, corrupt};
use crate::{Error, Result};

pub(crate) const MANIFEST: &str = "MANIFEST";
pub(crate) const LOCK: &str = "LOCK";
const MANIFEST_MAGIC: &str = "vlite-manifest 1";
/// Default size of the active log at which a commit starts a checkpoint.
pub const DEFAULT_CHECKPOINT_BYTES: u64 = 64 << 20;
/// Header flags a log segment may carry (it never holds history or stamps).
const WAL_FLAGS: u16 = format::FLAG_GRAPH | format::FLAG_ENCRYPTED;

/// How a directory is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Exclusive lock, writable active log; removes unlisted files.
    Write,
    /// Exclusive lock, read-only handles (maintenance tools).
    ReadExclusive,
    /// Shared lock, read-only handles (`open_read_only`).
    ReadShared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Image,
    Wal,
}

fn parse_name(name: &str) -> Option<(Kind, u64)> {
    let (kind, digits) = if let Some(rest) = name
        .strip_prefix("image-")
        .and_then(|rest| rest.strip_suffix(".vlite"))
    {
        (Kind::Image, rest)
    } else {
        (
            Kind::Wal,
            name.strip_prefix("wal-")
                .and_then(|rest| rest.strip_suffix(".log"))?,
        )
    };
    if digits.is_empty()
        || digits.len() > 19
        || digits.starts_with('0')
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((kind, digits.parse().ok()?))
}

fn encode_manifest(names: &[String]) -> Vec<u8> {
    let mut text = format!("{MANIFEST_MAGIC}\n");
    for name in names {
        text.push_str("file ");
        text.push_str(name);
        text.push('\n');
    }
    let checksum = format::crc32(text.as_bytes());
    text.push_str(&format!("crc {checksum:08x}\n"));
    text.into_bytes()
}

/// Parses and validates a manifest: at least one file, an image only first,
/// a log last, strictly increasing log numbers and a matching CRC.
fn decode_manifest(bytes: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(bytes).map_err(|_| corrupt(0, "manifest is not UTF-8"))?;
    let body = text
        .strip_suffix('\n')
        .ok_or_else(|| corrupt(0, "truncated manifest"))?;
    let split = body
        .rfind('\n')
        .ok_or_else(|| corrupt(0, "truncated manifest"))?;
    let (content, crc_line) = (&text[..=split], &body[split + 1..]);
    let checksum = crc_line
        .strip_prefix("crc ")
        .filter(|hex| hex.len() == 8)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .ok_or_else(|| corrupt(0, "manifest checksum line missing"))?;
    if format::crc32(content.as_bytes()) != checksum {
        return Err(corrupt(0, "manifest checksum mismatch"));
    }
    let mut lines = content.lines();
    if lines.next() != Some(MANIFEST_MAGIC) {
        return Err(corrupt(0, "unsupported manifest version"));
    }
    let mut names = Vec::new();
    let mut image = None;
    let mut previous = 0u64;
    for line in lines {
        let name = line
            .strip_prefix("file ")
            .ok_or_else(|| corrupt(0, "invalid manifest line"))?;
        let (kind, number) =
            parse_name(name).ok_or_else(|| corrupt(0, "invalid manifest file name"))?;
        if kind == Kind::Image {
            if !names.is_empty() {
                return Err(corrupt(0, "manifest image is not first"));
            }
            image = Some(number);
        } else {
            // An image is numbered when installed, after the logs it follows.
            if number <= previous || image == Some(number) {
                return Err(corrupt(0, "manifest log numbers are not increasing"));
            }
            previous = number;
        }
        names.push(name.to_owned());
    }
    match names.last().and_then(|name| parse_name(name)) {
        Some((Kind::Wal, _)) => Ok(names),
        _ => Err(corrupt(0, "manifest must end with a log segment")),
    }
}

/// Syncs a directory entry list (Unix); Windows offers no directory sync.
fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn lock_file(file: &File, shared: bool) -> Result<()> {
    let outcome = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match outcome {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(Error::Locked),
        Err(TryLockError::Error(error)) => Err(Error::Io(error)),
    }
}

/// One listed file. `skip` header bytes are hidden for every file after the
/// first; the file contributes `len` logical bytes starting at `start`.
struct Segment {
    file: File,
    name: String,
    skip: u64,
    start: u64,
    len: u64,
}

/// The logical byte stream over a list of segments with one cursor. Writes
/// go to the last segment only.
pub(crate) struct Segments {
    list: Vec<Segment>,
    position: u64,
}

impl Segments {
    fn total(&self) -> u64 {
        self.list.last().map_or(0, |last| last.start + last.len)
    }

    fn restart(&mut self) {
        let mut start = 0;
        for segment in &mut self.list {
            segment.start = start;
            start += segment.len;
        }
    }

    fn read_at_cursor(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.position >= self.total() {
            return Ok(0);
        }
        let position = self.position;
        let index = self
            .list
            .partition_point(|segment| segment.start + segment.len <= position);
        let segment = &self.list[index];
        let within = position - segment.start;
        let want = (buffer.len() as u64).min(segment.len - within) as usize;
        let mut file = &segment.file;
        file.seek(SeekFrom::Start(segment.skip + within))?;
        let read = file.read(&mut buffer[..want])?;
        self.position += read as u64;
        Ok(read)
    }

    fn write_at_cursor(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let position = self.position;
        let last = self.list.last_mut().ok_or_else(no_segments)?;
        if position < last.start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write before the active log segment",
            ));
        }
        let within = position - last.start;
        let mut file = &last.file;
        file.seek(SeekFrom::Start(last.skip + within))?;
        let written = file.write(buffer)?;
        last.len = last.len.max(within + written as u64);
        self.position += written as u64;
        Ok(written)
    }

    fn seek_cursor(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.total().checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        self.position = target;
        Ok(target)
    }

    fn truncate(&mut self, length: u64) -> io::Result<()> {
        let last = self.list.last_mut().ok_or_else(no_segments)?;
        if length < last.start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot truncate a sealed log segment",
            ));
        }
        last.file.set_len(last.skip + length - last.start)?;
        last.len = length - last.start;
        Ok(())
    }
}

fn no_segments() -> io::Error {
    io::Error::other("directory store has no segments")
}

impl Read for Segments {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_at_cursor(buffer)
    }
}

impl Seek for Segments {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.seek_cursor(position)
    }
}

/// Header facts shared by every segment of one directory.
#[derive(Clone)]
struct Layout {
    /// Bytes hidden at the start of every later segment (32, or 96 encrypted).
    header_len: u64,
    /// Header written at the start of each new log segment.
    wal_header: Vec<u8>,
}

impl Layout {
    /// Derives the layout from the first segment's header bytes.
    fn from_first(first: &[u8]) -> Result<Self> {
        if first.len() < format::HEADER_SIZE as usize || &first[..8] != format::MAGIC {
            return Err(corrupt(0, "invalid database magic"));
        }
        let version = u32::from_le_bytes(first[8..12].try_into().expect("4 bytes"));
        if version != format::V5 {
            return Err(Error::UnsupportedVersion(version));
        }
        let flags = u16::from_le_bytes(first[26..28].try_into().expect("2 bytes"));
        if flags & format::FLAG_TIMESTAMPS != 0 {
            return Err(Error::UnsupportedFeature(format::FLAG_TIMESTAMPS));
        }
        let header_len = format::HEADER_SIZE
            + if flags & format::FLAG_ENCRYPTED != 0 {
                DESCRIPTOR_SIZE as u64
            } else {
                0
            };
        if (first.len() as u64) < header_len {
            return Err(corrupt(
                format::HEADER_SIZE,
                "truncated encryption descriptor",
            ));
        }
        let mut wal_header = first[..header_len as usize].to_vec();
        wal_header[24..26].copy_from_slice(&[0; 2]);
        wal_header[26..28].copy_from_slice(&(flags & WAL_FLAGS).to_le_bytes());
        let checksum = format::crc32(&wal_header[..28]);
        wal_header[28..32].copy_from_slice(&checksum.to_le_bytes());
        Ok(Self {
            header_len,
            wal_header,
        })
    }

    /// A later segment's header must equal `wal_header` except the
    /// collection floor (bytes 24..26), and carry a valid checksum.
    fn check_later(&self, header: &[u8], offset: u64) -> Result<()> {
        let expected = &self.wal_header;
        if header.len() != expected.len()
            || header[..24] != expected[..24]
            || header[26..28] != expected[26..28]
            || header[32..] != expected[32..]
        {
            return Err(corrupt(
                offset,
                "log segment header does not match the store",
            ));
        }
        if format::crc32(&header[..28]).to_le_bytes() != header[28..32] {
            return Err(corrupt(offset, "log segment header checksum mismatch"));
        }
        Ok(())
    }
}

fn read_header(file: &File, length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; length];
    let mut reader = file;
    reader.seek(SeekFrom::Start(0))?;
    let mut filled = 0;
    while filled < length {
        let read = reader.read(&mut bytes[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// Opens the listed files and validates every later segment header.
fn open_segments(
    directory: &Path,
    names: &[String],
    writable_last: bool,
) -> Result<(Segments, Layout)> {
    let mut list: Vec<Segment> = Vec::with_capacity(names.len());
    let mut layout: Option<Layout> = None;
    let mut start = 0u64;
    for (index, name) in names.iter().enumerate() {
        let path = directory.join(name);
        let file = if writable_last && index + 1 == names.len() {
            OpenOptions::new().read(true).write(true).open(&path)?
        } else {
            File::open(&path)?
        };
        let size = file.metadata()?.len();
        let skip = match &layout {
            None => {
                let first = read_header(&file, format::HEADER_SIZE as usize + DESCRIPTOR_SIZE)?;
                let derived = Layout::from_first(&first)?;
                let flags = u16::from_le_bytes(first[26..28].try_into().expect("2 bytes"));
                if parse_name(name).map(|(kind, _)| kind) == Some(Kind::Wal)
                    && flags & !WAL_FLAGS != 0
                {
                    return Err(corrupt(0, "log segment header carries image flags"));
                }
                layout = Some(derived);
                0
            }
            Some(layout) => {
                let header = read_header(&file, layout.header_len as usize)?;
                layout.check_later(&header, start)?;
                layout.header_len
            }
        };
        let len = size - skip;
        list.push(Segment {
            file,
            name: name.clone(),
            skip,
            start,
            len,
        });
        start += len;
    }
    let layout = layout.ok_or_else(|| corrupt(0, "manifest lists no files"))?;
    Ok((Segments { list, position: 0 }, layout))
}

/// A finished background image build.
pub(crate) struct Built {
    image: NamedTempFile,
    len: u64,
    pub(crate) floor: u64,
    flags: u16,
}

struct Worker {
    handle: JoinHandle<Result<Built>>,
    /// Number of leading segments the image replaces.
    sealed: usize,
}

/// An open directory store; the storage behind `Backing::Chain`.
pub(crate) struct Chain {
    lock: File,
    directory: PathBuf,
    access: Access,
    segments: Mutex<Segments>,
    layout: Layout,
    next_number: u64,
    /// Active-log size that triggers an automatic checkpoint.
    pub(crate) threshold: Option<u64>,
    worker: Option<Worker>,
}

impl Chain {
    fn state(&self) -> MutexGuard<'_, Segments> {
        self.segments.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        self.state().read_at_cursor(buffer)
    }

    pub(crate) fn write(&self, buffer: &[u8]) -> io::Result<usize> {
        if self.access != Access::Write {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory store opened read-only",
            ));
        }
        self.state().write_at_cursor(buffer)
    }

    pub(crate) fn seek(&self, position: SeekFrom) -> io::Result<u64> {
        self.state().seek_cursor(position)
    }

    pub(crate) fn set_len(&self, length: u64) -> io::Result<()> {
        if self.access != Access::Write {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory store opened read-only",
            ));
        }
        self.state().truncate(length)
    }

    pub(crate) fn len(&self) -> u64 {
        self.state().total()
    }

    pub(crate) fn sync_all(&self) -> io::Result<()> {
        match self.state().list.last() {
            Some(last) => last.file.sync_all(),
            None => Err(no_segments()),
        }
    }

    /// Handle on the active log segment (the periodic flusher syncs it).
    pub(crate) fn try_clone(&self) -> io::Result<File> {
        match self.state().list.last() {
            Some(last) => last.file.try_clone(),
            None => Err(no_segments()),
        }
    }

    pub(crate) fn unlock(&self) -> io::Result<()> {
        self.lock.unlock()
    }

    /// Bytes of commit frames in the active log segment.
    fn active_bytes(&mut self) -> u64 {
        let header_len = self.layout.header_len;
        let state = self
            .segments
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        state.list.last().map_or(0, |last| {
            let header = if last.skip == 0 { header_len } else { 0 };
            last.len.saturating_sub(header)
        })
    }

    pub(crate) fn wants_checkpoint(&mut self) -> bool {
        self.worker.is_none()
            && self
                .threshold
                .is_some_and(|threshold| self.active_bytes() >= threshold.max(1))
    }

    pub(crate) fn worker_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| worker.handle.is_finished())
    }

    fn names(&mut self) -> Vec<String> {
        let state = self
            .segments
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        state
            .list
            .iter()
            .map(|segment| segment.name.clone())
            .collect()
    }

    fn allocate(&mut self) -> u64 {
        let number = self.next_number;
        self.next_number += 1;
        number
    }

    /// Atomically replaces `MANIFEST`. `Ok(false)` means the new manifest is
    /// in place but the directory sync failed, so it may not survive power
    /// loss; files it no longer lists must then be kept.
    fn publish(&self, names: &[String]) -> Result<bool> {
        publish_manifest(&self.directory, names)
    }

    /// Seals the active log behind a fresh empty one. Returns the number of
    /// sealed segments, or `None` when the active log holds no commits. A
    /// failed sync of the sealed log is `CommitIndeterminate`.
    pub(crate) fn rotate(&mut self) -> Result<Option<usize>> {
        if self.access != Access::Write || self.worker.is_some() || self.active_bytes() == 0 {
            return Ok(None);
        }
        {
            let state = self
                .segments
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner);
            let last = state.list.last().ok_or_else(no_segments)?;
            last.file.sync_all().map_err(Error::CommitIndeterminate)?;
        }
        let name = format!("wal-{}.log", self.allocate());
        let path = self.directory.join(&name);
        let created = (|| -> Result<File> {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(&self.layout.wal_header)?;
            file.sync_all()?;
            sync_directory(&self.directory)?;
            Ok(file)
        })();
        let file = match created {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        let mut names = self.names();
        names.push(name.clone());
        let durable = match self.publish(&names) {
            Ok(durable) => durable,
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        let _ = durable;
        let header_len = self.layout.header_len;
        let state = self
            .segments
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let start = state.total();
        state.list.push(Segment {
            file,
            name,
            skip: header_len,
            start,
            len: 0,
        });
        state.position = start;
        Ok(Some(state.list.len() - 1))
    }

    /// Replays the first `sealed` segments on a background thread into a
    /// history image at `floor` (the last sealed commit).
    pub(crate) fn spawn(
        &mut self,
        sealed: usize,
        floor: u64,
        cipher: Option<Cipher>,
    ) -> Result<()> {
        let names: Vec<String> = self.names().into_iter().take(sealed).collect();
        let directory = self.directory.clone();
        let handle = thread::Builder::new()
            .name("vlite-checkpoint".into())
            .spawn(move || {
                let (mut segments, _) = open_segments(&directory, &names, false)?;
                let loaded = format::load_known(&mut segments, cipher.as_ref())?;
                if loaded.incomplete_bytes != 0 || loaded.sequence != floor {
                    return Err(corrupt(loaded.end, "sealed log segments changed"));
                }
                build_image(&directory, &loaded, cipher.as_ref(), floor)
            })?;
        self.worker = Some(Worker { handle, sealed });
        Ok(())
    }

    /// Takes the background build when it has finished (or, with `wait`,
    /// after joining it).
    pub(crate) fn take_build(&mut self, wait: bool) -> Option<(Result<Built>, usize)> {
        if !wait && !self.worker_finished() {
            return None;
        }
        let worker = self.worker.take()?;
        let result = worker
            .handle
            .join()
            .unwrap_or_else(|_| Err(Error::Io(io::Error::other("checkpoint worker panicked"))));
        Some((result, worker.sealed))
    }

    /// Publishes a built image in place of the first `sealed` segments.
    /// Returns the new logical length and the image's header flags.
    pub(crate) fn install(&mut self, built: Built, sealed: usize) -> Result<(u64, u16)> {
        let name = format!("image-{}.vlite", self.allocate());
        let path = self.directory.join(&name);
        let file = built
            .image
            .persist_noclobber(&path)
            .map_err(|error| Error::Io(error.error))?;
        let mut names = vec![name.clone()];
        names.extend(self.names().into_iter().skip(sealed));
        let durable = match self.publish(&names) {
            Ok(durable) => durable,
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        };
        let state = self
            .segments
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let replaced: Vec<Segment> = state.list.drain(..sealed).collect();
        state.list.insert(
            0,
            Segment {
                file,
                name,
                skip: 0,
                start: 0,
                len: built.len,
            },
        );
        state.restart();
        state.position = state.total();
        let total = state.total();
        let obsolete: Vec<String> = replaced.into_iter().map(|segment| segment.name).collect();
        if durable {
            for name in obsolete {
                let _ = fs::remove_file(self.directory.join(name));
            }
        }
        Ok((total, built.flags))
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        // The worker reads files this handle may delete; never leave it
        // running past the lock. Its staged image removes itself.
        if let Some(worker) = self.worker.take() {
            let _ = worker.handle.join();
        }
    }
}

/// Atomically replaces `directory/MANIFEST` (staged, synced, renamed).
fn publish_manifest(directory: &Path, names: &[String]) -> Result<bool> {
    let mut staging = NamedTempFile::new_in(directory)?;
    staging.write_all(&encode_manifest(names))?;
    staging.as_file().sync_all()?;
    staging
        .persist(directory.join(MANIFEST))
        .map_err(|error| Error::Io(error.error))?;
    Ok(sync_directory(directory).is_ok())
}

/// Writes and validates a history image at `floor` in a staged file.
fn build_image(
    directory: &Path,
    loaded: &format::Loaded,
    cipher: Option<&Cipher>,
    floor: u64,
) -> Result<Built> {
    let lineage = loaded
        .lineage
        .ok_or_else(|| corrupt(0, "directory store without lineage"))?;
    let mut staging = NamedTempFile::new_in(directory)?;
    crate::maintenance::write_v5_image(
        staging.as_file_mut(),
        loaded,
        lineage,
        crate::current_time_ms(),
        cipher,
        loaded.flags & format::FLAG_GRAPH,
        floor,
    )?;
    let check = format::load_known(staging.as_file_mut(), cipher)?;
    if check.incomplete_bytes != 0 || check.sequence != floor || check.lineage != loaded.lineage {
        return Err(corrupt(0, "checkpoint image failed validation"));
    }
    staging.as_file().sync_all()?;
    let len = staging.as_file().metadata()?.len();
    Ok(Built {
        image: staging,
        len,
        floor,
        flags: check.flags,
    })
}

/// Opens an existing directory store under its `LOCK` file.
pub(crate) fn open(directory: &Path, access: Access) -> Result<Chain> {
    let lock_path = directory.join(LOCK);
    let lock = match access {
        Access::Write => OpenOptions::new().read(true).write(true).open(&lock_path)?,
        Access::ReadExclusive | Access::ReadShared => File::open(&lock_path)?,
    };
    lock_file(&lock, access == Access::ReadShared)?;
    let names = decode_manifest(&fs::read(directory.join(MANIFEST))?)?;
    let (segments, layout) = open_segments(directory, &names, access == Access::Write)?;
    let mut highest = names
        .iter()
        .filter_map(|name| parse_name(name))
        .map(|(_, number)| number)
        .max()
        .unwrap_or(0);
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if names.contains(&name) {
            continue;
        }
        let parsed = parse_name(&name);
        if let Some((_, number)) = parsed {
            highest = highest.max(number);
        }
        // Leftovers of an interrupted rotation, build or publication.
        if access == Access::Write && (parsed.is_some() || name.starts_with(".tmp")) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(Chain {
        lock,
        directory: directory.to_path_buf(),
        access,
        segments: Mutex::new(segments),
        layout,
        next_number: highest + 1,
        threshold: Some(DEFAULT_CHECKPOINT_BYTES),
        worker: None,
    })
}

/// Runs `fill` in a newly created directory that holds a locked `LOCK`
/// file, removing the directory again if `fill` fails.
fn create_locked(directory: &Path, fill: impl FnOnce(&File) -> Result<()>) -> Result<File> {
    fs::create_dir(directory)?;
    let result = (|| -> Result<File> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.join(LOCK))?;
        lock_file(&lock, false)?;
        fill(&lock)?;
        sync_directory(directory)?;
        Ok(lock)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(directory);
    }
    result
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Creates a new directory store whose first log starts with `header`.
pub(crate) fn create(directory: &Path, header: &[u8]) -> Result<Chain> {
    let lock = create_locked(directory, |_| {
        write_new(&directory.join("wal-1.log"), header)?;
        publish_manifest(directory, &["wal-1.log".to_owned()])?;
        Ok(())
    })?;
    let names = vec!["wal-1.log".to_owned()];
    let (segments, layout) = open_segments(directory, &names, true)?;
    Ok(Chain {
        lock,
        directory: directory.to_path_buf(),
        access: Access::Write,
        segments: Mutex::new(segments),
        layout,
        next_number: 2,
        threshold: Some(DEFAULT_CHECKPOINT_BYTES),
        worker: None,
    })
}

/// Writes a new directory store holding `loaded`: a history image at its
/// last commit plus an empty log (or only a log for an empty database).
/// The lineage is kept, so change-feed cursors stay comparable.
pub(crate) fn create_converted(directory: &Path, loaded: &format::Loaded) -> Result<()> {
    let lineage = loaded
        .lineage
        .ok_or(Error::UnsupportedVersion(loaded.version))?;
    let cipher = loaded.cipher.as_ref();
    let flags = loaded.flags & WAL_FLAGS;
    let lock = create_locked(directory, |_| {
        if loaded.sequence == 0 {
            let header = format::header_v5_flags(
                loaded.dimensions,
                lineage,
                loaded.collection_high,
                flags,
                cipher,
            );
            write_new(&directory.join("wal-1.log"), &header)?;
            publish_manifest(directory, &["wal-1.log".to_owned()])?;
        } else {
            let built = build_image(directory, loaded, cipher, loaded.sequence)?;
            built
                .image
                .persist_noclobber(directory.join("image-1.vlite"))
                .map_err(|error| Error::Io(error.error))?;
            let header = format::header_v5_flags(loaded.dimensions, lineage, 0, flags, cipher);
            write_new(&directory.join("wal-2.log"), &header)?;
            publish_manifest(
                directory,
                &["image-1.vlite".to_owned(), "wal-2.log".to_owned()],
            )?;
        }
        Ok(())
    })?;
    lock.unlock()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_and_rejects_damage() {
        let names = vec![
            "image-3.vlite".to_owned(),
            "wal-4.log".to_owned(),
            "wal-7.log".to_owned(),
        ];
        let bytes = encode_manifest(&names);
        assert_eq!(decode_manifest(&bytes).unwrap(), names);
        let mut damaged = bytes.clone();
        damaged[20] ^= 1;
        assert!(matches!(
            decode_manifest(&damaged),
            Err(Error::Corrupt { .. })
        ));
        assert!(decode_manifest(&bytes[..bytes.len() - 1]).is_err());
        for bad in [
            vec![
                "wal-2.log".to_owned(),
                "image-3.vlite".to_owned(),
                "wal-4.log".to_owned(),
            ],
            vec!["wal-4.log".to_owned(), "wal-4.log".to_owned()],
            vec!["image-1.vlite".to_owned()],
            vec!["wal-01.log".to_owned()],
            vec!["../wal-1.log".to_owned()],
            vec![],
        ] {
            assert!(decode_manifest(&encode_manifest(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names_parse_strictly() {
        assert_eq!(parse_name("wal-12.log"), Some((Kind::Wal, 12)));
        assert_eq!(parse_name("image-1.vlite"), Some((Kind::Image, 1)));
        for bad in [
            "wal-.log",
            "wal-0.log",
            "wal-1.log.tmp",
            "image-1.log",
            "wal-1a.log",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }
}
