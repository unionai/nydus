// Copyright 2021 Ant Group. All rights reserved.
// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::Result;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use nix::NixPath;
use nydus_utils::div_round_up;
use nydus_utils::filemap::FileMapState;

use crate::utils::readahead;

#[cfg(target_os = "macos")]
use vmm_sys_util::tempfile::TempFile;

pub(crate) const VERSION: u32 = 2;
pub(crate) const MAGIC1: u32 = 0x424D_4150;
pub(crate) const MAGIC2: u32 = 0x434D_4150;
pub(crate) const HEADER_SIZE: usize = 4096;
pub(crate) const HEADER_RESERVED_SIZE: usize = HEADER_SIZE - 20 - size_of::<AtomicU32>();

/// The blob chunk map file header, 4096 bytes.
#[repr(C)]
pub(crate) struct Header {
    /// PersistMap magic number
    pub magic: u32,
    pub version: u32,
    pub magic2: u32,
    pub _all_ready: u32,
    pub count: u32,
    pub not_ready_count: AtomicU32,
    pub reserved: [u8; HEADER_RESERVED_SIZE],
}

impl Header {
    #[cfg(test)]
    pub fn as_slice(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Header as *const u8,
                std::mem::size_of::<Header>(),
            )
        }
    }
}

pub(crate) struct PersistMap {
    count: u32,
    filemap: FileMapState,
}

impl PersistMap {
    /// Creates a new file or opens existing. An existing file chunk count must
    /// match the specified.
    pub fn create<P: AsRef<Path>>(filename: P, chunk_count: u32) -> Result<Self> {
        if chunk_count == 0 {
            return Err(einval!("chunk count should be greater than 0"));
        }

        let filename = filename.as_ref();
        let dir = filename
            .parent()
            .map(|d| {
                if d.is_empty() {
                    PathBuf::from(".")
                } else {
                    d.into()
                }
            })
            .ok_or_else(|| einval!("missing filename"))?;

        // Create an annonymous file
        let file = AnonymousFile::open(&dir)?;

        let file_size = Self::calc_file_size(chunk_count);
        file.inner().set_len(file_size)?;

        let mut filemap =
            FileMapState::new(file.inner().try_clone()?, 0, file_size as usize, true)?;

        // Write the header and zeroed out bitmap
        Self::write_header(&mut filemap, chunk_count)?;

        // Flush before publishing to ensure that the file is in a valid state on disk
        file.inner().sync_all()?;

        // Publish as filename
        match file.publish(&filename) {
            Ok(()) => Ok(Self {
                count: chunk_count,
                filemap,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process beat us to it
                Self::existing(filename, chunk_count)
            }
            Err(e) => Err(e),
        }
    }

    /// Akin to `create` but does not store it on disk.
    pub fn transient(chunk_count: u32) -> Result<Self> {
        if chunk_count == 0 {
            return Err(einval!("chunk count should be greater than 0"));
        }

        let file_size = Self::calc_file_size(chunk_count);

        let mut filemap = FileMapState::anonymous(0, file_size as usize)?;

        // Write the header and zeroed out bitmap
        Self::write_header(&mut filemap, chunk_count)?;

        Ok(Self {
            count: chunk_count,
            filemap,
        })
    }

    /// Opens an existing file. Fails if the file does not exist.
    /// An existing file chunk count must match the specified.
    pub fn existing<P: AsRef<Path>>(filename: P, chunk_count: u32) -> Result<Self> {
        if chunk_count == 0 {
            return Err(einval!("chunk count should be greater than 0"));
        }

        let filename = filename.as_ref();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(filename)
            .map_err(|err| {
                einval!(format!(
                    "failed to open blob chunk_map file {}: {err}",
                    filename.display()
                ))
            })?;

        let file_size = file.metadata()?.len();
        let expected_size = Self::calc_file_size(chunk_count);
        if file_size != expected_size {
            return Err(einval!(format!(
                "chunk_map file {} has unexpected size: {file_size}, expected: {expected_size}",
                filename.display()
            )));
        }

        readahead(file.as_raw_fd(), 0, expected_size);

        let mut filemap = FileMapState::new(file, 0, expected_size as usize, true)?;
        let header = filemap.get_mut::<Header>(0)?;
        if header.magic != MAGIC1 {
            return Err(einval!(format!(
                "chunk_map file {filename:?} has bad magic"
            )));
        }
        if header.version != VERSION {
            return Err(einval!(format!(
                "chunk_map file {filename:?} has bad version"
            )));
        }
        if header.count != chunk_count {
            return Err(einval!(format!(
                "chunk_map file {filename:?} has wrong count"
            )));
        }

        Ok(Self {
            count: chunk_count,
            filemap,
        })
    }

    fn calc_file_size(chunk_count: u32) -> u64 {
        let bitmap_size = div_round_up(chunk_count as u64, 8u64);
        HEADER_SIZE as u64 + bitmap_size
    }

    fn write_header(filemap: &mut FileMapState, chunk_count: u32) -> Result<()> {
        let header = Header {
            magic: MAGIC1,
            version: VERSION,
            magic2: MAGIC2,
            _all_ready: 0,
            count: chunk_count,
            not_ready_count: AtomicU32::new(chunk_count),
            reserved: [0x0u8; HEADER_RESERVED_SIZE],
        };

        *filemap.get_mut(0)? = header;

        Ok(())
    }

    #[cfg(test)]
    pub fn size(&self) -> usize {
        self.filemap.size()
    }

    #[inline]
    fn validate_index(&self, idx: u32) -> Result<()> {
        if idx < self.count {
            Ok(())
        } else {
            Err(einval!(format!(
                "chunk index {} exceeds chunk count {}",
                idx, self.count
            )))
        }
    }

    #[inline]
    fn read_u8(&self, idx: u32) -> Result<u8> {
        let start = HEADER_SIZE + (idx as usize >> 3);
        let current = self.filemap.get_ref::<AtomicU8>(start)?;

        Ok(current.load(Ordering::Acquire))
    }

    #[inline]
    fn write_u8(&self, idx: u32, current: u8) -> Result<bool> {
        let mask = Self::index_to_mask(idx);
        let expected = current | mask;
        let start = HEADER_SIZE + (idx as usize >> 3);
        let atomic_value = self.filemap.get_ref::<AtomicU8>(start)?;

        Ok(atomic_value
            .compare_exchange(current, expected, Ordering::Acquire, Ordering::Relaxed)
            .is_ok())
    }

    #[inline]
    fn index_to_mask(index: u32) -> u8 {
        let pos = 8 - ((index & 0b111) + 1);
        1 << pos
    }

    #[inline]
    fn header(&self) -> &Header {
        self.filemap.get_ref(0).unwrap()
    }

    #[inline]
    pub fn is_chunk_ready(&self, index: u32) -> Result<bool> {
        self.validate_index(index)?;

        let mask = Self::index_to_mask(index);
        let current = self.read_u8(index)?;
        Ok(current & mask == mask)
    }

    pub fn set_chunk_ready(&self, index: u32) -> Result<()> {
        self.validate_index(index)?;
        let mask = Self::index_to_mask(index);

        // Loop to atomically update the state bit corresponding to the chunk index.
        loop {
            let current = self.read_u8(index)?;
            let ready = current & mask == mask;
            if ready {
                break;
            }

            if self.write_u8(index, current)? {
                self.header().not_ready_count.fetch_sub(1, Ordering::AcqRel);
                break;
            }
        }

        Ok(())
    }

    #[inline]
    pub fn is_range_all_ready(&self) -> bool {
        self.header().not_ready_count.load(Ordering::Acquire) == 0
    }

    #[inline]
    #[cfg(test)]
    pub fn count(&self) -> u32 {
        self.count
    }
}

#[cfg(target_os = "macos")]
struct AnonymousFile(TempFile);

#[cfg(target_os = "macos")]
impl AnonymousFile {
    fn open(dir: &Path) -> Result<Self> {
        Ok(Self(TempFile::new_in(dir)?))
    }

    fn inner(&self) -> &File {
        &self.0.as_file()
    }

    fn publish(&self, filename: &Path) -> Result<()> {
        fs::hard_link(self.0.as_path(), filename)
    }
}

#[cfg(target_os = "linux")]
struct AnonymousFile(File);

#[cfg(target_os = "linux")]
impl AnonymousFile {
    fn open(dir: &Path) -> Result<Self> {
        // Create an annonymous file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_TMPFILE)
            .open(dir)
            .map_err(|err| einval!(format!("failed to open/create blob chunk_map file: {err}",)))?;

        Ok(Self(file))
    }

    fn inner(&self) -> &File {
        &self.0
    }

    fn publish(&self, filename: &Path) -> Result<()> {
        let ret = unsafe {
            let oldpath = std::ffi::CString::default();
            let newpath = std::ffi::CString::new(filename.as_os_str().as_encoded_bytes())?;

            libc::linkat(
                self.0.as_raw_fd(),
                oldpath.as_ptr(),
                libc::AT_FDCWD,
                newpath.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        };

        if ret < 0 {
            Err(last_error!())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmm_sys_util::tempdir::TempDir;

    const NUM_CHUNKS: u32 = 1000;

    #[test]
    fn test_persist_map_create() {
        let dir = TempDir::new().unwrap();
        let filename = dir.as_path().join("persist-map");

        let mut maps: Vec<PersistMap> = Vec::new();

        for i in 0..10 {
            let map = PersistMap::create(&filename, NUM_CHUNKS).unwrap();
            assert!(map.count() == NUM_CHUNKS);

            // set some ready with overlap from the previous iteration
            let start_idx = std::cmp::max((i as i32) * 100 - 20, 0) as u32;
            let end_idx = (i + 1) * 100;
            for idx in start_idx..end_idx {
                map.set_chunk_ready(idx as u32).unwrap();
                assert!(map.is_chunk_ready(idx).unwrap());

                let is_last = idx == NUM_CHUNKS - 1;
                assert!(map.is_range_all_ready() == is_last);
            }

            maps.push(map);
        }

        // close all
        drop(maps);

        // make sure the file is still there
        let map = PersistMap::create(&filename, NUM_CHUNKS).unwrap();
        assert!(map.is_range_all_ready());
        drop(map);

        let map = PersistMap::existing(&filename, NUM_CHUNKS).unwrap();
        assert!(map.is_range_all_ready());
    }

    #[test]
    fn test_persist_map_transient() {
        let map = PersistMap::transient(NUM_CHUNKS).unwrap();
        assert!(map.count() == NUM_CHUNKS);

        // set some ready with overlap from the previous iteration
        for idx in 0..NUM_CHUNKS {
            map.set_chunk_ready(idx as u32).unwrap();
            assert!(map.is_chunk_ready(idx).unwrap());

            let is_last = idx == NUM_CHUNKS - 1;
            assert!(map.is_range_all_ready() == is_last);
        }
    }
}
