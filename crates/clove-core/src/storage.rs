//! File-backed piece storage with SHA-1 verification.
//!
//! A torrent is one linear byte space (its files concatenated in order);
//! pieces and blocks index into that space and may straddle file
//! boundaries. Storage maps (piece, offset) to the right files and does
//! positioned reads and writes (`pread`/`pwrite`), so concurrent access to
//! disjoint regions needs no lock — the sync engine's disk worker and peer
//! threads can touch different pieces at once (SCOPE §4).
//!
//! mmap is deliberately not used (predictable memory over speed, SCOPE §4),
//! and for the same reason nothing here reads a whole piece at once:
//! verification streams the piece through a block-sized buffer into SHA-1 and
//! compares the result to the metainfo expectation. A download is only trusted
//! after it verifies.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use sha1::{Digest, Sha1};

use crate::metainfo::MetaInfo;

/// How much of a piece is read at a time when verifying it.
///
/// The block size, so the read pattern is the one the rest of the engine
/// already makes, and small enough that the buffer is a rounding error next to
/// a peer connection's own state.
const VERIFY_CHUNK: u32 = crate::wire::BLOCK_LEN;

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
    /// Shared with the [`MetaInfo`] it came from rather than copied: see
    /// [`MetaInfo::pieces`].
    piece_hashes: Arc<[[u8; 20]]>,
}

impl Storage {
    /// Create (or open) the torrent's files under `root`, creating parent
    /// directories as needed. With `preallocate`, each file's blocks are
    /// claimed up front; otherwise files grow as blocks are written.
    ///
    /// # Errors
    ///
    /// Any filesystem error creating directories, opening files, or
    /// preallocating.
    pub fn create(meta: &MetaInfo, root: &Path, preallocate: bool) -> io::Result<Self> {
        let mut regions = Vec::with_capacity(meta.files.len());
        let mut global = 0u64;
        for entry in &meta.files {
            // Walks and opens by directory descriptor with `O_NOFOLLOW`, so a
            // symlinked component is refused by the kernel rather than by a
            // check that could be stale by the time we act on it.
            let file = open_beneath(root, &entry.path)?;
            if preallocate && entry.length > 0 {
                claim_blocks(&file, entry.length, &entry.path)?;
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
            piece_hashes: Arc::clone(&meta.pieces),
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
        let mut out = Vec::new();
        self.read_block_into(index, begin, len, &mut out)?;
        Ok(out)
    }

    /// [`read_block`](Storage::read_block) into a caller-owned buffer, which is
    /// resized to exactly `len` bytes and filled.
    ///
    /// For callers that read block after block — verification, and anything
    /// serving a peer's request pipeline — so the buffer is allocated once
    /// rather than once per block.
    ///
    /// # Errors
    ///
    /// As [`read_block`](Storage::read_block). On error the buffer's contents
    /// are unspecified; it is scratch space, not a result.
    pub fn read_block_into(
        &self,
        index: u32,
        begin: u32,
        len: u32,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let start = u64::from(index) * self.piece_length + u64::from(begin);
        // Check the range before sizing the buffer for it. `len` is a u32 from
        // the wire, so an unbounded caller would otherwise reserve up to 4 GiB
        // on its way to being told the range does not exist.
        self.check_range(start, u64::from(len))?;
        out.clear();
        out.resize(len as usize, 0);
        self.for_each_segment(start, out.len(), |file, file_off, seg| {
            let lo = usize::try_from(seg.start).unwrap_or(usize::MAX);
            let hi = usize::try_from(seg.end).unwrap_or(usize::MAX);
            file.read_exact_at(&mut out[lo..hi], file_off)
        })
    }

    /// Whether piece `index` currently on disk matches its expected SHA-1.
    ///
    /// # Errors
    ///
    /// A read error other than a short/absent region; a not-yet-written
    /// piece reads short and is reported as unverified (`Ok(false)`), not an
    /// error.
    pub fn verify_piece(&self, index: u32) -> io::Result<bool> {
        let mut scratch = Vec::new();
        self.verify_piece_into(index, &mut scratch)
    }

    /// [`verify_piece`](Storage::verify_piece), reusing `scratch` as the read
    /// buffer.
    ///
    /// The buffer is the whole reason this exists. Verification used to read the
    /// entire piece into a fresh allocation and hash it in one go, which is a
    /// buffer the *torrent* sizes:
    /// [`MAX_PIECE_LENGTH`](crate::metainfo::MAX_PIECE_LENGTH) is 128 MiB, so a torrent
    /// could name a figure and have the daemon allocate it, once per piece, on
    /// every recheck. SHA-1 is a streaming hash and has never needed the piece
    /// in one contiguous run, so this feeds it a block at a time out of
    /// one buffer that the caller keeps: the peak is a constant instead of a
    /// number an attacker picks, and a full recheck stops asking the allocator
    /// for a fresh megabyte per piece.
    ///
    /// # Errors
    ///
    /// As [`verify_piece`](Storage::verify_piece).
    pub fn verify_piece_into(&self, index: u32, scratch: &mut Vec<u8>) -> io::Result<bool> {
        let len = self.piece_len(index);
        if len == 0 {
            return Ok(false);
        }
        let Some(expected) = self.piece_hashes.get(index as usize) else {
            return Ok(false);
        };
        let mut hasher = Sha1::new();
        let mut done = 0u32;
        while done < len {
            let chunk = (len - done).min(VERIFY_CHUNK);
            match self.read_block_into(index, done, chunk, scratch) {
                Ok(()) => hasher.update(&scratch[..chunk as usize]),
                // A piece that has not been written yet reads short. Not an
                // error: it is simply a piece we do not hold.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(e) => return Err(e),
            }
            done += chunk;
        }
        let got: [u8; 20] = hasher.finalize().into();
        Ok(*expected == got)
    }

    /// Verify every piece, returning the set that currently matches — a full
    /// recheck (the `verify` command, or resume validation).
    ///
    /// # Errors
    ///
    /// Any read error other than short/absent regions.
    pub fn verify_all(&self) -> io::Result<crate::bitfield::Bitfield> {
        let mut have = crate::bitfield::Bitfield::empty(self.num_pieces());
        // One buffer for the whole pass, not one per piece: a recheck walks
        // every piece of every file a torrent has, and this is the difference
        // between one allocation and hundreds of thousands of them.
        let mut scratch = Vec::new();
        for index in 0..self.num_pieces() {
            if self.verify_piece_into(index, &mut scratch)? {
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

/// Claim `length` bytes of disk for `file`, so the download cannot later fail
/// part-way through for want of space.
///
/// `fallocate(2)` reserves blocks; setting the length alone would leave the
/// file sparse, which reserves nothing and is what `preallocate no` already
/// does. A filesystem that cannot reserve says `EOPNOTSUPP`, and there the
/// length is set instead and the operator is told the space is not claimed.
///
/// # Errors
///
/// Out of space, or any other filesystem error.
fn claim_blocks(file: &File, length: u64, components: &[String]) -> io::Result<()> {
    use rustix::fs::{FallocateFlags, fallocate};

    match fallocate(file, FallocateFlags::empty(), 0, length) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::OPNOTSUPP) => {
            eprintln!(
                "clove: {}: this filesystem cannot reserve space; the file is sparse and the \
                 download can still run out of disk",
                crate::text::scrub(&components.join("/"))
            );
            file.set_len(length)
        }
        Err(e) => Err(io::Error::from(e)),
    }
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
            pieces: pieces.into(),
            files,
            total_length: total,
            private: true,
            trackers: vec![],
            skipped_trackers: 0,
            raw_info: Arc::from([].as_slice()),
        }
    }

    #[test]
    fn preallocation_reserves_blocks_rather_than_leaving_a_hole() {
        use std::os::unix::fs::MetadataExt;

        // Big enough that a sparse file and a reserved one cannot be confused:
        // a hole costs no blocks at all.
        const LEN: u64 = 1 << 20;
        let content = vec![0u8; usize::try_from(LEN).unwrap()];

        for (preallocate, why) in [(true, "reserved"), (false, "sparse")] {
            let dir = TempDir::new();
            let meta = meta_for(
                vec![FileEntry {
                    path: vec!["big.bin".into()],
                    length: LEN,
                }],
                1 << 14,
                &content,
            );
            Storage::create(&meta, &dir.0, preallocate).unwrap();

            let md = std::fs::metadata(dir.0.join("big.bin")).unwrap();
            // Both spellings claim the length; only one claims the disk.
            assert_eq!(md.len(), if preallocate { LEN } else { 0 }, "{why}: length");
            let claimed = md.blocks() * 512;
            if preallocate {
                assert!(
                    claimed >= LEN,
                    "{why}: {claimed} bytes of blocks for a {LEN}-byte file — \
                     the space was not actually claimed"
                );
            } else {
                assert_eq!(claimed, 0, "{why}: an untouched file took disk");
            }
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

    /// Verification hashes a piece in block-sized chunks, so a piece several
    /// chunks long is the case that would break if the streaming loop dropped,
    /// repeated, or reordered one.
    ///
    /// Every piece here is three chunks and a bit, so the loop runs four times
    /// with a short final read — and the last piece of the torrent is short
    /// again, which is the other end of the same arithmetic.
    #[test]
    fn a_piece_longer_than_one_chunk_verifies_chunk_by_chunk() {
        let dir = TempDir::new();
        let piece_length = VERIFY_CHUNK * 3 + 1024;
        // Two full pieces and a short one, with content that would hash the
        // same under a dropped or duplicated chunk only by collision.
        let total = piece_length as usize * 2 + 777;
        let content: Vec<u8> = (0..total).map(|i| u8::try_from(i % 251).unwrap()).collect();
        let meta = meta_for(
            vec![FileEntry {
                path: vec!["big.bin".into()],
                length: total as u64,
            }],
            piece_length,
            &content,
        );
        let st = Storage::create(&meta, &dir.0, false).unwrap();
        assert_eq!(st.num_pieces(), 3);

        // A piece that is only partly written reads short: not held, not an
        // error, and not a panic on the chunk that runs off the end.
        st.write_block(0, 0, &content[..VERIFY_CHUNK as usize])
            .unwrap();
        assert!(!st.verify_piece(0).unwrap(), "a partial piece is not held");

        for p in 0..st.num_pieces() {
            let start = p as usize * piece_length as usize;
            let len = st.piece_len(p) as usize;
            st.write_block(p, 0, &content[start..start + len]).unwrap();
        }
        assert!(st.verify_all().unwrap().is_full());

        // A flipped byte in the *last* chunk of a piece must still be caught:
        // a loop that stopped early would call this piece good.
        let last_chunk_start = piece_length - 1;
        st.write_block(0, last_chunk_start, &[0xFF]).unwrap();
        assert!(
            !st.verify_piece(0).unwrap(),
            "corruption in a piece's final chunk went unnoticed"
        );
    }

    /// The scratch buffer is reused across pieces, so a longer piece followed
    /// by a shorter one must hash the shorter one's bytes and nothing left over
    /// from before.
    #[test]
    fn a_reused_verify_buffer_does_not_leak_the_previous_piece() {
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
        for p in 0..st.num_pieces() {
            let start = p as usize * 16;
            let len = st.piece_len(p) as usize;
            st.write_block(p, 0, &content[start..start + len]).unwrap();
        }
        // Pieces 0..2 are 16 bytes, piece 3 is 2. Walk them in that order
        // through one buffer, which is what `verify_all` does.
        let mut scratch = Vec::new();
        for p in 0..st.num_pieces() {
            assert!(
                st.verify_piece_into(p, &mut scratch).unwrap(),
                "piece {p} through a reused buffer"
            );
        }
        // And backwards, so the buffer is oversized rather than undersized.
        for p in (0..st.num_pieces()).rev() {
            assert!(
                st.verify_piece_into(p, &mut scratch).unwrap(),
                "piece {p} through an oversized reused buffer"
            );
        }
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
}
