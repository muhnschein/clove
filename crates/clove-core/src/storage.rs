//! File-backed piece storage with SHA-1 verification.
//!
//! A torrent is one linear byte space (its files concatenated in order);
//! pieces and blocks index into that space and may straddle file
//! boundaries. Storage maps (piece, offset) to the right files and does
//! positioned reads and writes (`pread`/`pwrite`), so concurrent access to
//! disjoint regions needs no lock — the sync engine's disk worker and peer
//! threads can touch different pieces at once (SCOPE §4).
//!
//! mmap is deliberately not used (predictable memory over speed, SCOPE §4).
//! Verification reads a whole piece and compares its SHA-1 to the metainfo
//! expectation; a download is only trusted after it verifies.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::metainfo::MetaInfo;

/// One file's placement in the torrent's global byte space.
struct Region {
    file: File,
    /// Global offset of this file's first byte.
    global_start: u64,
    /// File length in bytes.
    length: u64,
}

/// The on-disk backing for one torrent.
pub struct Storage {
    regions: Vec<Region>,
    piece_length: u64,
    total_length: u64,
    piece_hashes: Vec<[u8; 20]>,
}

impl Storage {
    /// Create (or open) the torrent's files under `root`, creating parent
    /// directories as needed. With `preallocate`, each file is grown to its
    /// full length up front; otherwise files grow as blocks are written.
    ///
    /// # Errors
    ///
    /// Any filesystem error creating directories, opening files, or
    /// preallocating.
    pub fn create(meta: &MetaInfo, root: &Path, preallocate: bool) -> io::Result<Self> {
        let mut regions = Vec::with_capacity(meta.files.len());
        let mut global = 0u64;
        for entry in &meta.files {
            let path = join_under(root, &entry.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            if preallocate && entry.length > 0 {
                file.set_len(entry.length)?;
            }
            regions.push(Region {
                file,
                global_start: global,
                length: entry.length,
            });
            global += entry.length;
        }
        Ok(Storage {
            regions,
            piece_length: u64::from(meta.piece_length),
            total_length: meta.total_length,
            piece_hashes: meta.pieces.clone(),
        })
    }

    /// Number of pieces.
    #[must_use]
    pub fn num_pieces(&self) -> u32 {
        // Piece count comes from the hash list; always fits u32 for any
        // torrent clove accepts.
        u32::try_from(self.piece_hashes.len()).unwrap_or(u32::MAX)
    }

    /// Length of piece `index` in bytes (the final piece may be short).
    /// Zero for an out-of-range index.
    #[must_use]
    pub fn piece_len(&self, index: u32) -> u32 {
        let start = u64::from(index) * self.piece_length;
        if start >= self.total_length {
            return 0;
        }
        let remaining = self.total_length - start;
        u32::try_from(remaining.min(self.piece_length)).unwrap_or(u32::MAX)
    }

    /// Write a block at `begin` within piece `index`.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the block falls outside the
    /// torrent's byte space, or any underlying write error.
    pub fn write_block(&self, index: u32, begin: u32, data: &[u8]) -> io::Result<()> {
        let start = u64::from(index) * self.piece_length + u64::from(begin);
        self.for_each_segment(start, data.len(), |file, file_off, seg| {
            let lo = usize::try_from(seg.start).unwrap_or(usize::MAX);
            let hi = usize::try_from(seg.end).unwrap_or(usize::MAX);
            file.write_all_at(&data[lo..hi], file_off)
        })
    }

    /// Read `len` bytes at `begin` within piece `index`.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the range falls outside the
    /// torrent's byte space, or any underlying read error (including a file
    /// shorter than the requested range — an unwritten region).
    pub fn read_block(&self, index: u32, begin: u32, len: u32) -> io::Result<Vec<u8>> {
        let start = u64::from(index) * self.piece_length + u64::from(begin);
        let mut out = vec![0u8; len as usize];
        self.for_each_segment(start, out.len(), |file, file_off, seg| {
            let lo = usize::try_from(seg.start).unwrap_or(usize::MAX);
            let hi = usize::try_from(seg.end).unwrap_or(usize::MAX);
            file.read_exact_at(&mut out[lo..hi], file_off)
        })?;
        Ok(out)
    }

    /// Whether piece `index` currently on disk matches its expected SHA-1.
    ///
    /// # Errors
    ///
    /// A read error other than a short/absent region; a not-yet-written
    /// piece reads short and is reported as unverified (`Ok(false)`), not an
    /// error.
    pub fn verify_piece(&self, index: u32) -> io::Result<bool> {
        let len = self.piece_len(index);
        if len == 0 {
            return Ok(false);
        }
        let data = match self.read_block(index, 0, len) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(e),
        };
        let got: [u8; 20] = Sha1::digest(&data).into();
        Ok(self.piece_hashes.get(index as usize) == Some(&got))
    }

    /// Verify every piece, returning the set that currently matches — a full
    /// recheck (the `verify` command, or resume validation).
    ///
    /// # Errors
    ///
    /// Any read error other than short/absent regions.
    pub fn verify_all(&self) -> io::Result<crate::bitfield::Bitfield> {
        let mut have = crate::bitfield::Bitfield::empty(self.num_pieces());
        for index in 0..self.num_pieces() {
            if self.verify_piece(index)? {
                have.set(index);
            }
        }
        Ok(have)
    }

    /// Flush all files to disk.
    ///
    /// # Errors
    ///
    /// Any underlying sync error.
    pub fn sync_all(&self) -> io::Result<()> {
        for region in &self.regions {
            region.file.sync_all()?;
        }
        Ok(())
    }

    /// Split a global range into per-file segments and apply `op` to each.
    /// `op` receives the file, the offset within that file, and the segment
    /// range within the caller's buffer.
    fn for_each_segment(
        &self,
        global_start: u64,
        len: usize,
        mut op: impl FnMut(&File, u64, std::ops::Range<u64>) -> io::Result<()>,
    ) -> io::Result<()> {
        let len = len as u64;
        let end = global_start
            .checked_add(len)
            .filter(|&e| e <= self.total_length)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "storage: range outside torrent",
                )
            })?;
        if len == 0 {
            return Ok(());
        }
        for region in &self.regions {
            if region.length == 0 {
                continue;
            }
            let region_end = region.global_start + region.length;
            let lo = global_start.max(region.global_start);
            let hi = end.min(region_end);
            if lo >= hi {
                continue;
            }
            let file_off = lo - region.global_start;
            let buf_range = (lo - global_start)..(hi - global_start);
            op(&region.file, file_off, buf_range)?;
        }
        Ok(())
    }
}

/// Join validated path components under `root`. Components are already
/// checked by `metainfo` (no separators, no `..`), so this cannot escape
/// `root`.
fn join_under(root: &Path, components: &[String]) -> PathBuf {
    let mut path = root.to_path_buf();
    for c in components {
        path.push(c);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::{FileEntry, InfoHash, MetaInfo};
    use sha1::{Digest, Sha1};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway directory under the system temp dir; removed on drop.
    /// Avoids a `tempfile` dependency (SCOPE §9 frugality).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("clove-storage-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn piece_hash(data: &[u8]) -> [u8; 20] {
        Sha1::digest(data).into()
    }

    /// Build a `MetaInfo` whose piece hashes match `content` laid out over
    /// `files`, split into `piece_length`-sized pieces.
    fn meta_for(files: Vec<FileEntry>, piece_length: u32, content: &[u8]) -> MetaInfo {
        let total: u64 = files.iter().map(|f| f.length).sum();
        assert_eq!(total, content.len() as u64);
        let pieces: Vec<[u8; 20]> = content
            .chunks(piece_length as usize)
            .map(piece_hash)
            .collect();
        MetaInfo {
            info_hash: InfoHash([0; 20]),
            name: "t".into(),
            piece_length,
            pieces,
            files,
            total_length: total,
            private: true,
            trackers: vec![],
            skipped_trackers: 0,
        }
    }

    #[test]
    fn single_file_write_read_verify() {
        let dir = TempDir::new();
        let content: Vec<u8> = (0..50u8).collect();
        let meta = meta_for(
            vec![FileEntry {
                path: vec!["a.bin".into()],
                length: 50,
            }],
            16,
            &content,
        );
        let st = Storage::create(&meta, &dir.0, false).unwrap();

        assert_eq!(st.num_pieces(), 4); // 16,16,16,2
        assert_eq!(st.piece_len(0), 16);
        assert_eq!(st.piece_len(3), 2);

        // Nothing written yet.
        assert!(!st.verify_piece(0).unwrap());

        // Write each piece in two blocks where possible.
        for p in 0..st.num_pieces() {
            let len = st.piece_len(p);
            let start = (p as usize) * 16;
            st.write_block(p, 0, &content[start..start + len as usize])
                .unwrap();
        }
        for p in 0..st.num_pieces() {
            assert!(st.verify_piece(p).unwrap(), "piece {p}");
        }
        assert!(st.verify_all().unwrap().is_full());

        // Read back a block that we wrote.
        assert_eq!(st.read_block(0, 4, 4).unwrap(), vec![4, 5, 6, 7]);
    }

    #[test]
    fn piece_spanning_multiple_files() {
        let dir = TempDir::new();
        // Three files of 10 bytes; piece length 15 -> pieces cross files.
        let content: Vec<u8> = (0..30u8).collect();
        let files = vec![
            FileEntry {
                path: vec!["d".into(), "a".into()],
                length: 10,
            },
            FileEntry {
                path: vec!["d".into(), "b".into()],
                length: 10,
            },
            FileEntry {
                path: vec!["c".into()],
                length: 10,
            },
        ];
        let meta = meta_for(files, 15, &content);
        let st = Storage::create(&meta, &dir.0, true).unwrap();
        assert_eq!(st.num_pieces(), 2);

        // Piece 0 spans file a (all) + file b (first 5).
        st.write_block(0, 0, &content[0..15]).unwrap();
        st.write_block(1, 0, &content[15..30]).unwrap();
        assert!(st.verify_piece(0).unwrap());
        assert!(st.verify_piece(1).unwrap());

        // A read straddling the a/b boundary returns contiguous bytes.
        assert_eq!(st.read_block(0, 8, 4).unwrap(), vec![8, 9, 10, 11]);

        // Preallocation created the third file at full length on disk.
        assert_eq!(std::fs::metadata(dir.0.join("c")).unwrap().len(), 10);
    }

    #[test]
    fn corrupt_block_fails_verification() {
        let dir = TempDir::new();
        let content: Vec<u8> = vec![7; 20];
        let meta = meta_for(
            vec![FileEntry {
                path: vec!["a".into()],
                length: 20,
            }],
            16,
            &content,
        );
        let st = Storage::create(&meta, &dir.0, false).unwrap();
        st.write_block(0, 0, &[7; 16]).unwrap();
        assert!(st.verify_piece(0).unwrap());
        // Flip a byte: verification must now fail.
        st.write_block(0, 5, &[0]).unwrap();
        assert!(!st.verify_piece(0).unwrap());
    }

    #[test]
    fn out_of_range_access_is_rejected() {
        let dir = TempDir::new();
        let content: Vec<u8> = vec![0; 10];
        let meta = meta_for(
            vec![FileEntry {
                path: vec!["a".into()],
                length: 10,
            }],
            16,
            &content,
        );
        let st = Storage::create(&meta, &dir.0, false).unwrap();
        let err = st.write_block(0, 8, &[1, 2, 3, 4]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
