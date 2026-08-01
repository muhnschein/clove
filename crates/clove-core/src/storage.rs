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

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sha1::{Digest, Sha1};

use crate::metainfo::MetaInfo;

/// How many of a torrent's files may be open at once.
///
/// Storage used to hold one descriptor per file for the torrent's whole life,
/// so a torrent's file count was a descriptor count — and a metainfo is small
/// compared to what it can describe. A valid 221 KB torrent declaring 8,192
/// files reached the shipped systemd unit's `LimitNOFILE=8192` on its own,
/// which is a remotely-supplied number choosing when the daemon runs out of
/// descriptors for everything else: peer streams, the control socket, the SAM
/// connection.
///
/// `metainfo::MAX_FILES` now caps the file count too, but a cap alone would
/// still mean thousands of descriptors held for a torrent nobody chose to
/// receive. This is the half that decouples the two: what a torrent declares
/// no longer decides what clove holds.
///
/// 64 is well above the working set of a sequential or rarest-first download —
/// pieces touch one or two files at a time, a few more when a piece straddles
/// a boundary — so the cache does its job without a reopen per block.
const OPEN_FILE_LIMIT: usize = 64;

/// One file's placement in the torrent's global byte space.
struct Region {
    /// Validated components, relative to `root`. Held instead of a descriptor
    /// so the file can be opened on demand and closed under pressure.
    path: Vec<String>,
    /// Global offset of this file's first byte.
    global_start: u64,
    /// File length in bytes.
    length: u64,
}

/// Most-recently-used first. A `Vec` rather than a map because
/// [`OPEN_FILE_LIMIT`] entries is a scan of a few dozen machine words, against
/// a hash of the key on every block read.
type OpenFiles = Vec<(usize, Arc<File>)>;

/// The on-disk backing for one torrent.
pub struct Storage {
    root: PathBuf,
    regions: Vec<Region>,
    /// Bounded cache of open descriptors, keyed by region index.
    ///
    /// The lock is held for the lookup and the bookkeeping, never across the
    /// read or write itself — those take an `Arc<File>` clone and run outside
    /// it, so the positioned-I/O property this module is built on (disjoint
    /// regions need no lock) survives.
    open: Mutex<OpenFiles>,
    piece_length: u64,
    total_length: u64,
    piece_hashes: Vec<[u8; 20]>,
}

fn lock_open(m: &Mutex<OpenFiles>) -> MutexGuard<'_, OpenFiles> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
            // Every file is still created up front: `clove verify` and the
            // initial scan both expect the layout to exist the moment a
            // torrent is added, and `preallocate` has nowhere else to happen.
            // The descriptor is dropped at the end of the iteration — creating
            // a file and holding it open are separate decisions, and only the
            // first is needed here.
            //
            // Walks and opens by directory descriptor with `O_NOFOLLOW`, so a
            // symlinked component is refused by the kernel rather than by a
            // check that could be stale by the time we act on it.
            let file = open_beneath(root, &entry.path)?;
            if preallocate && entry.length > 0 {
                file.set_len(entry.length)?;
            }
            regions.push(Region {
                path: entry.path.clone(),
                global_start: global,
                length: entry.length,
            });
            global += entry.length;
        }
        Ok(Storage {
            root: root.to_path_buf(),
            regions,
            open: Mutex::new(Vec::new()),
            piece_length: u64::from(meta.piece_length),
            total_length: meta.total_length,
            piece_hashes: meta.pieces.clone(),
        })
    }

    /// The open descriptor for region `index`, opening it if the cache does
    /// not have it and evicting the least recently used one if that fills it.
    ///
    /// Reopening goes through [`open_beneath`] again, so the `O_NOFOLLOW`
    /// walk is repeated on every cache miss rather than trusted once at
    /// creation — a component that becomes a symlink later is refused then
    /// too, which the old open-once-and-hold arrangement could not do.
    fn file_for(&self, index: usize) -> io::Result<Arc<File>> {
        {
            let mut open = lock_open(&self.open);
            if let Some(pos) = open.iter().position(|(i, _)| *i == index) {
                let entry = open.remove(pos);
                let file = Arc::clone(&entry.1);
                open.insert(0, entry);
                return Ok(file);
            }
        }

        // Opened without the lock held: a slow open on a cold cache must not
        // stop other threads reading files that are already in it.
        let region = self
            .regions
            .get(index)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "storage: no such region"))?;
        let file = Arc::new(open_beneath(&self.root, &region.path)?);

        let mut open = lock_open(&self.open);
        // Another thread may have raced us to the same region. Either handle
        // works — positioned I/O carries its own offset — so keep one and let
        // the other drop.
        open.retain(|(i, _)| *i != index);
        open.insert(0, (index, Arc::clone(&file)));
        while open.len() > OPEN_FILE_LIMIT {
            if let Some((_, evicted)) = open.pop() {
                // Flushed before we lose our handle to it. Dropping a `File`
                // does not sync, so an evicted file's writes would otherwise
                // sit in the page cache with nothing left to sync them —
                // `Storage::sync_all` can only reach what is still open. A
                // resume record that says "verified" over data a power cut can
                // still take back is precisely the state-corruption case
                // SECURITY.md names.
                evicted.sync_all()?;
            }
        }
        Ok(file)
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
        // Check the range before allocating for it. `len` is a u32 from the
        // wire, so an unbounded caller would otherwise reserve up to 4 GiB on
        // its way to being told the range does not exist.
        self.check_range(start, u64::from(len))?;
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
    /// Only the files currently open are synced, which is all of them that
    /// can need it: a file leaves the cache only through eviction, and
    /// eviction syncs it on the way out. Opening every file to sync it would
    /// reintroduce the descriptor fan-out this cache exists to remove.
    pub fn sync_all(&self) -> io::Result<()> {
        // Cloned out under the lock and synced outside it: an fsync is slow,
        // and holding the cache lock across all of them would stall every
        // reader for the duration.
        let files: Vec<Arc<File>> = lock_open(&self.open)
            .iter()
            .map(|(_, f)| Arc::clone(f))
            .collect();
        for file in files {
            file.sync_all()?;
        }
        Ok(())
    }

    /// The end of a range inside the torrent's byte space, or
    /// [`io::ErrorKind::InvalidInput`] if it does not fit.
    fn check_range(&self, global_start: u64, len: u64) -> io::Result<u64> {
        global_start
            .checked_add(len)
            .filter(|&e| e <= self.total_length)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "storage: range outside torrent",
                )
            })
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
        let end = self.check_range(global_start, len)?;
        if len == 0 {
            return Ok(());
        }
        for (index, region) in self.regions.iter().enumerate() {
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
            // Opened here rather than at creation, and only for the files this
            // range actually touches — which is one or two per block, however
            // many the torrent declares.
            let file = self.file_for(index)?;
            op(&file, file_off, buf_range)?;
        }
        Ok(())
    }
}

/// Walk `components` beneath `root` as directory descriptors, refusing to
/// traverse a symbolic link at any level, and creating missing directories.
///
/// Returns the descriptor of the directory holding the last component, and the
/// last component itself.
///
/// `metainfo` guarantees the components are lexically harmless — no separators,
/// no `..`, no NUL — and for a long time a comment in this module concluded from
/// that that the joined path "cannot escape `root`". It cannot lexically; it can
/// through the filesystem. If `downloads/demo` already exists as a symlink to
/// somewhere else, then joining and opening `downloads/demo/a.bin` writes there,
/// and `create_dir_all` walks the link without comment. A torrent cannot create
/// that link, but a torrent is not the only thing that writes under a download
/// directory, and the escape turns "can write in `downloads/`" into "can write
/// wherever the daemon can".
///
/// The refusal is the kernel's, not ours: every step is an `openat` on a single
/// component carrying `O_NOFOLLOW | O_DIRECTORY`, so a symlink fails the syscall
/// rather than failing a check we made a moment earlier. Checking with
/// `symlink_metadata` and then opening by path — which is what this did first —
/// leaves a window between the two in which the component can become a link, and
/// closing that window is the whole reason `openat` exists.
///
/// Landlock stops the escape too where it is available, but `docs/SCOPE.md` §9 is
/// explicit that no layer may assume another is present.
///
/// # Errors
///
/// A component that is a symbolic link or is not a directory, and any
/// filesystem error opening or creating one.
fn walk_beneath<'a>(
    root: &Path,
    components: &'a [String],
) -> io::Result<(rustix::fd::OwnedFd, &'a str)> {
    use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};

    let Some((last, parents)) = components.split_last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a file entry with no path components",
        ));
    };

    // The root is the configured download directory, not anything a torrent
    // named, so it is created the ordinary way. Everything below it is
    // attacker-influenced and gets the careful treatment.
    std::fs::create_dir_all(root)?;
    let mut dir = openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;

    for component in parents {
        match mkdirat(&dir, component.as_str(), Mode::from_bits_truncate(0o755)) {
            // Already there is the ordinary case; whether it is usable is the
            // next line's question, and it asks the kernel.
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(refused(component, e)),
        }
        dir = openat(
            &dir,
            component.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| refused(component, e))?;
    }
    Ok((dir, last.as_str()))
}

/// Turn the errno a refused component produces into something an operator can
/// act on.
///
/// `ELOOP` from an `O_NOFOLLOW` open and `ENOTDIR` from a non-directory in the
/// middle of a path both mean "this name is not what a torrent's path component
/// may be", and both arrive here as bare numbers.
fn refused(component: &str, e: rustix::io::Errno) -> io::Error {
    let why = match e {
        rustix::io::Errno::LOOP => "path component is a symbolic link",
        rustix::io::Errno::NOTDIR => "path component is not a directory",
        _ => return io::Error::from(e),
    };
    io::Error::new(io::ErrorKind::InvalidInput, format!("{component}: {why}"))
}

/// Open (creating if absent) the file `components` names beneath `root`.
///
/// # Errors
///
/// Anything [`walk_beneath`] refuses, or a final component that is a symbolic
/// link or cannot be opened.
fn open_beneath(root: &Path, components: &[String]) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, openat};

    let (dir, last) = walk_beneath(root, components)?;
    // `O_NOFOLLOW` here is what stops the *file* itself being a link to
    // somewhere else — the last component is the one that gets the bytes.
    let fd = openat(
        &dir,
        last,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )
    .map_err(|e| refused(last, e))?;
    Ok(File::from(fd))
}

/// Delete the file `components` names beneath `root`, if it is there.
///
/// Deleting through a symbolic link is the same escape pointed the other way,
/// and takes somebody else's file with it. Absent is success: there is nothing
/// to delete.
///
/// # Errors
///
/// Anything the private `walk_beneath` walk refuses, or an unlink error
/// other than the file
/// already being gone.
pub fn remove_beneath(root: &Path, components: &[String]) -> io::Result<()> {
    use rustix::fs::{AtFlags, unlinkat};

    let (dir, last) = match walk_beneath(root, components) {
        Ok(pair) => pair,
        // Nothing laid out beneath the root at all: nothing to remove.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // `unlinkat` removes the link itself, never what it points at, so a
    // symlinked *final* component costs the attacker their own link and nothing
    // of ours. The parents are what needed care, and `walk_beneath` gave it.
    match unlinkat(&dir, last, AtFlags::empty()) {
        // Gone already is the outcome we wanted either way.
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(e) => Err(io::Error::from(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::{FileEntry, InfoHash, MetaInfo};
    use sha1::{Digest, Sha1};
    use std::path::PathBuf;
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
            raw_info: Vec::new(),
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
    fn an_absurd_read_length_is_refused_before_it_is_allocated() {
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
        // A peer-supplied length is a u32; the range check has to come first,
        // or this reserves four gigabytes on its way to an error.
        let err = st.read_block(0, 0, u32::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // And a length that fits the torrent still works.
        st.write_block(0, 0, &content).unwrap();
        assert_eq!(st.read_block(0, 0, 10).unwrap().len(), 10);
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

    /// A symlinked directory component must not be written through.
    ///
    /// The components are lexically clean — no `..`, no separators — and that
    /// was taken as proof the path could not escape. It is not: the escape is
    /// the filesystem's doing, not the name's. `outside` here stands in for
    /// anywhere the daemon can write, which is the point of the finding.
    #[test]
    fn a_symlinked_directory_component_is_refused() {
        let dir = TempDir::new();
        let root = dir.0.join("downloads");
        let outside = dir.0.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("demo")).unwrap();

        let content: Vec<u8> = vec![7; 10];
        let meta = meta_for(
            vec![FileEntry {
                path: vec!["demo".into(), "escaped.bin".into()],
                length: 10,
            }],
            16,
            &content,
        );

        let Err(err) = Storage::create(&meta, &root, false) else {
            panic!("a symlinked component must not be written through");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert!(
            !outside.join("escaped.bin").exists(),
            "a file was created outside the download root"
        );
    }

    /// And a symlink where the *file* itself goes: the last component is the
    /// one that gets the bytes.
    #[test]
    fn a_symlinked_file_is_refused_rather_than_written_through() {
        let dir = TempDir::new();
        let root = dir.0.join("downloads");
        std::fs::create_dir_all(root.join("demo")).unwrap();
        let victim = dir.0.join("victim.txt");
        std::fs::write(&victim, b"original").unwrap();
        std::os::unix::fs::symlink(&victim, root.join("demo").join("a.bin")).unwrap();

        let content: Vec<u8> = vec![7; 10];
        let meta = meta_for(
            vec![FileEntry {
                path: vec!["demo".into(), "a.bin".into()],
                length: 10,
            }],
            16,
            &content,
        );

        let Err(err) = Storage::create(&meta, &root, false) else {
            panic!("a symlinked file must be refused");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"original",
            "the linked-to file was written through"
        );
    }

    /// The ordinary case still works, including creating nested directories
    /// that do not exist yet — the fix must not cost the feature.
    #[test]
    fn nested_directories_are_still_created() {
        let dir = TempDir::new();
        let content: Vec<u8> = vec![3; 10];
        let meta = meta_for(
            vec![FileEntry {
                path: vec![
                    "demo".into(),
                    "deep".into(),
                    "deeper".into(),
                    "a.bin".into(),
                ],
                length: 10,
            }],
            16,
            &content,
        );
        let st = Storage::create(&meta, &dir.0, false).expect("nested layout");
        st.write_block(0, 0, &content).unwrap();
        assert!(dir.0.join("demo/deep/deeper/a.bin").is_file());
        // Re-opening an existing layout is the common path and must not trip
        // the new-file branch.
        let again = Storage::create(&meta, &dir.0, false).expect("reopen");
        assert!(again.verify_all().unwrap().has(0));
    }

    /// Deletion refuses the same links, and treats absence as success.
    #[test]
    fn remove_beneath_refuses_links_and_tolerates_absence() {
        let dir = TempDir::new();
        let root = dir.0.join("downloads");
        std::fs::create_dir_all(root.join("demo")).unwrap();

        remove_beneath(&root, &["demo".into(), "absent.bin".into()])
            .expect("an absent file is not an error for a deletion");

        let real = root.join("demo").join("real.bin");
        std::fs::write(&real, b"x").unwrap();
        remove_beneath(&root, &["demo".into(), "real.bin".into()]).expect("remove a real file");
        assert!(!real.exists(), "the file was not removed");

        // A symlinked *parent* must not be walked: the file on the other side
        // is not ours to delete.
        let outside = dir.0.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let keep = outside.join("keep.txt");
        std::fs::write(&keep, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        let err = remove_beneath(&root, &["linked".into(), "keep.txt".into()])
            .expect_err("deleting through a link must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert!(keep.exists(), "a file outside the root was deleted");

        // A symlinked *final* component costs the attacker their link and
        // nothing of ours: `unlinkat` removes the link, never its target.
        let target = dir.0.join("target.txt");
        std::fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, root.join("demo").join("link.bin")).unwrap();
        remove_beneath(&root, &["demo".into(), "link.bin".into()]).expect("unlink the link itself");
        assert!(target.exists(), "unlinkat followed the link");
        assert!(!root.join("demo").join("link.bin").exists());
    }

    /// Count this process's open descriptors that point somewhere under
    /// `root`. Scoped to the directory rather than counting every descriptor,
    /// so the number is this torrent's and not whatever else the test binary
    /// has open on its other threads. Linux-only, which every supported target
    /// is.
    fn open_fds_under(root: &Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .expect("/proc/self/fd")
            .filter_map(Result::ok)
            .filter(|e| {
                std::fs::read_link(e.path()).is_ok_and(|target| target.starts_with(root))
            })
            .count()
    }

    /// A torrent's file count used to be a descriptor count, held for the
    /// torrent's whole life. Since a metainfo can describe far more files than
    /// it costs bytes to write, that made a remote party's number decide when
    /// the daemon ran out of descriptors for peer streams, the control socket
    /// and the SAM connection.
    ///
    /// `MAX_FILES` caps the count; this is the half that makes the count stop
    /// mattering. Correctness across the eviction boundary is the risk the
    /// cache introduces, so it is checked at the same time.
    #[test]
    fn descriptors_are_bounded_however_many_files_a_torrent_declares() {
        let dir = TempDir::new();

        // Comfortably more files than the cache holds, so eviction runs many
        // times over during a single pass.
        let count = OPEN_FILE_LIMIT * 4;
        let piece_length = 16u32;
        let per_file = 16u64;
        let content: Vec<u8> = (0..count as u64 * per_file)
            .map(|i| u8::try_from(i % 251).expect("byte"))
            .collect();
        let files: Vec<FileEntry> = (0..count)
            .map(|i| FileEntry {
                path: vec!["many".into(), format!("f{i:04}.bin")],
                length: per_file,
            })
            .collect();
        let meta = meta_for(files, piece_length, &content);

        let storage = Storage::create(&meta, &dir.0, false).expect("create");

        // Creating the layout must not leave a descriptor per file behind.
        let after_create = open_fds_under(&dir.0);
        assert!(
            after_create <= OPEN_FILE_LIMIT,
            "creation held {after_create} descriptors for {count} files"
        );

        // Write every piece, which walks the whole file set and forces the
        // cache to turn over.
        for (index, chunk) in content.chunks(piece_length as usize).enumerate() {
            let index = u32::try_from(index).expect("piece index");
            storage.write_block(index, 0, chunk).expect("write block");
        }
        let after_write = open_fds_under(&dir.0);
        assert!(
            after_write <= OPEN_FILE_LIMIT,
            "writing held {after_write} descriptors for {count} files"
        );

        // And the data is right, across every eviction that happened on the
        // way — the failure a cache like this introduces is a read served
        // from the wrong file, not a descriptor count.
        storage.sync_all().expect("sync");
        for (index, chunk) in content.chunks(piece_length as usize).enumerate() {
            let len = u32::try_from(chunk.len()).expect("piece length");
            let got = storage
                .read_block(u32::try_from(index).expect("piece index"), 0, len)
                .expect("read block");
            assert_eq!(got, chunk, "piece {index} came back wrong");
        }
        assert_eq!(
            storage.verify_all().expect("verify").count(),
            u32::try_from(meta.pieces.len()).expect("piece count"),
            "not every piece verified after eviction"
        );
    }
}
