//! In-memory byte-buffer storage backend. It lets the engine run where no
//! filesystem or OS file locks exist (WebAssembly) and makes it possible to
//! create, ship and restore databases as plain byte vectors.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Mutex, MutexGuard, PoisonError};

#[derive(Default)]
struct MemState {
    data: Vec<u8>,
    position: u64,
}

/// A growable, seekable buffer with interior mutability so `&MemFile` can be
/// read like `&File`. There is no locking: an in-memory database has one owner.
#[derive(Default)]
pub(crate) struct MemFile(Mutex<MemState>);

impl MemFile {
    pub(crate) fn from_bytes(data: Vec<u8>) -> Self {
        Self(Mutex::new(MemState { data, position: 0 }))
    }

    fn state(&self) -> MutexGuard<'_, MemState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> u64 {
        self.state().data.len() as u64
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        self.state().data.clone()
    }

    pub(crate) fn set_len(&self, length: u64) -> io::Result<()> {
        let length = usize::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length too large"))?;
        self.state().data.resize(length, 0);
        Ok(())
    }

    pub(crate) fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut state = self.state();
        let start = usize::try_from(state.position)
            .unwrap_or(usize::MAX)
            .min(state.data.len());
        let count = buffer.len().min(state.data.len() - start);
        buffer[..count].copy_from_slice(&state.data[start..start + count]);
        state.position += count as u64;
        Ok(count)
    }

    pub(crate) fn write(&self, buffer: &[u8]) -> io::Result<usize> {
        let mut state = self.state();
        let start = usize::try_from(state.position)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "position too large"))?;
        let end = start
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write too large"))?;
        if end > state.data.len() {
            state.data.resize(end, 0);
        }
        state.data[start..end].copy_from_slice(buffer);
        state.position = end as u64;
        Ok(buffer.len())
    }

    pub(crate) fn seek(&self, position: SeekFrom) -> io::Result<u64> {
        let mut state = self.state();
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => (state.data.len() as u64).checked_add_signed(delta),
            SeekFrom::Current(delta) => state.position.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        state.position = target;
        Ok(target)
    }
}

impl Read for &MemFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        MemFile::read(self, buffer)
    }
}

impl Seek for &MemFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        MemFile::seek(self, position)
    }
}

impl Write for &MemFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        MemFile::write(self, buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_writes_seeks_and_truncates() {
        let file = MemFile::default();
        let mut handle = &file;
        handle.write_all(b"hello world").unwrap();
        handle.seek(SeekFrom::Start(6)).unwrap();
        let mut word = [0u8; 5];
        handle.read_exact(&mut word).unwrap();
        assert_eq!(&word, b"world");
        assert_eq!(handle.read(&mut word).unwrap(), 0);
        file.set_len(5).unwrap();
        assert_eq!(file.to_bytes(), b"hello");
        handle.seek(SeekFrom::Start(7)).unwrap();
        handle.write_all(b"!").unwrap();
        assert_eq!(file.to_bytes(), b"hello\0\0!");
        assert!(handle.seek(SeekFrom::Current(-100)).is_err());
    }
}
