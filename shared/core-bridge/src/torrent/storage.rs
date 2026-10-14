//! The pieces of one torrent on the console's disk: what has been fetched, where each
//! block sits, and whether a piece a peer sent is the piece the torrent says it should
//! be.
//!
//! The data of every file of a torrent is kept in one file, in the order the torrent lays
//! it out, so a piece is a plain offset in it. That file is made as large as the torrent
//! and only written where data has arrived, which is what keeps a half-downloaded torrent
//! from taking the room it would once finished.
//!
//! What is held is said twice over, in two small files beside the data: which pieces are
//! whole, and which blocks of the pieces that are not whole have arrived. A piece that is
//! whole is checked against the torrent's own hash of it and is then kept whole: its block
//! bytes are cleared, because they would say nothing that the piece's own byte does not.

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::env::put_in_place;

use super::magnet::info_hash_to_hex;
use super::metadata::Meta;

/// The block size a torrent is fetched and read in. Blocks have to be the same size all
/// the way through a piece, so this is the size a piece is cut to, and never more than a
/// piece.
const TARGET_BLOCK: u32 = 64 * 1024;

/// A torrent larger than this is a description that is not to be believed, and is not
/// worth making a room for. Nothing of this size is played, and a card cannot hold it: the
/// data file is made whole from the first moment, so this bounds what one can ask of the
/// card rather than what is read from it.
const LIMIT: u64 = 64 << 30;

/// What every index file starts with: a name for this file's own format, so that a file
/// left by an older build, or by something else altogether, is not read as an index.
const MAGIC: &[u8; 8] = b"strmidx2";
/// How long the head of an index file is: the magic, the info hash, the digest of the
/// pieces' hashes, the piece count, the piece length, the block length, the torrent's
/// size, and how many bytes follow.
const HEAD: usize = 8 + 20 + 20 + 4 + 4 + 4 + 8 + 8;

/// One torrent's data in a folder of its own, named for the torrent's hash.
///
/// Nothing is public: the session holds one of these and asks it for pieces. Writing,
/// reading and saving all want the whole of it, so it goes behind a lock of the session's
/// own; the data file has one position that each of them moves for itself.
pub struct Storage {
    /// The torrent's own description: the pieces, their sizes, and the hash each is
    /// checked against.
    meta: Meta,
    /// Every file's data in torrent order, as large as the whole torrent.
    data: File,
    /// The folder the files are in, for `save`.
    folder: PathBuf,
    /// The torrent's hash in hex, which is what every file in the folder is named for.
    name: String,
    /// The description's own digest of its pieces' hashes, kept in the index files' head.
    fingerprint: [u8; 20],
    /// How many pieces there are. As many as there are hashes for, which is what makes
    /// every piece's hash safe to look up.
    pieces: u32,
    /// Whether each piece is whole: one byte per piece.
    have: Vec<u8>,
    /// Whether each block of a piece that is not whole is held: one byte per block, in
    /// piece-major order, `piece * (per_piece + 1) + block`.
    blocks: Vec<u8>,
    /// The size of a block, apart from the last block of each piece.
    block_len: u32,
    /// How many blocks a whole piece is in, which is what the blocks are laid out by.
    per_piece: u32,
    /// The torrent's own description as it was given, kept so that `save` can put it back
    /// where a later run will look for it.
    torrent: Option<Vec<u8>>,
}

impl Storage {
    /// Opens the folder for one torrent, making it if it is not there, and reads what a
    /// previous run left. `torrent_bytes` is the torrent's own description when it is
    /// known; it is kept so the description does not have to be asked for again.
    ///
    /// The folder holds:
    ///
    /// - `<hash>.torrent`: the description, when there is one to keep
    /// - `<hash>.have`: which pieces are whole
    /// - `<hash>.blocks`: which parts of the pieces that are not whole are held
    /// - `<hash>.data`: the pieces themselves
    ///
    /// The three index files are read back only when they match the description in hand:
    /// another torrent under the same name (the same hash, a different description, which
    /// the peers have to be caught for) is thrown away rather than believed.
    ///
    /// Only a data file that cannot be opened, and a description that makes no pieces at
    /// all, are failures. A folder with nothing in it, and an index file that is short,
    /// truncated or left by another build, are both an ordinary first run.
    pub fn open(folder: &Path, meta: &Meta, torrent_bytes: Option<&[u8]>) -> std::io::Result<Self> {
        // A description with no piece length, no hashes or an impossible size says
        // nothing about where any byte goes, so there is nothing to open.
        let pieces = meta.piece_count().min(meta.hashes.len() as u32);
        if meta.piece_length == 0 || pieces == 0 || meta.total_length > LIMIT {
            return Err(Error::new(ErrorKind::InvalidInput, "the torrent makes no pieces to fetch"));
        }
        std::fs::create_dir_all(folder)?;
        let name = info_hash_to_hex(&meta.info_hash);

        // The description is put down now, and again by `save`, so a run that is asked for
        // one does not have to fetch it to keep it.
        let torrent = match torrent_bytes {
            Some(bytes) => {
                put(&folder.join(format!("{name}.torrent")), bytes)?;
                Some(bytes.to_vec())
            }
            None => None,
        };

        // The data file is the size of the whole torrent from the first moment, and only
        // the blocks that arrive are written into it.
        let data_path = folder.join(format!("{name}.data"));
        let data = OpenOptions::new().read(true).write(true).create(true).open(&data_path)?;
        if data.metadata()?.len() != meta.total_length {
            data.set_len(meta.total_length)?;
        }

        // Blocks are the size wanted, and a whole number of them to a piece, so a piece is
        // never cut in the middle of one. What is left over of a piece that does not
        // divide evenly goes to its last block.
        let per_piece = (meta.piece_length / TARGET_BLOCK).max(1);
        let mut storage = Storage {
            fingerprint: digest_of_hashes(&meta.hashes),
            meta: meta.clone(),
            data,
            folder: folder.to_path_buf(),
            name,
            pieces,
            have: vec![0; pieces as usize],
            blocks: Vec::new(),
            block_len: meta.piece_length / per_piece,
            per_piece,
            torrent,
        };
        // A piece whose length is not a whole number of blocks takes one byte more per piece
        // than the round ones do, so the blocks are addressed from a fixed width. The
        // index file says how long it is, which is what tells an index written under the
        // old rule from one written under this one.
        let last = storage.pieces - 1;
        let width = per_piece as usize + 1;
        let held = storage.pieces as usize * width;
        storage.blocks = vec![0; held.max(storage.blocks_per_piece(last) as usize)];
        storage.have = storage.read_index(&storage.have_path(), storage.have.len());
        storage.blocks = storage.read_index(&storage.blocks_path(), storage.blocks.len());
        Ok(storage)
    }

    /// The block size this torrent is fetched and read in: 64 KiB, or the piece length
    /// when a piece is shorter, and always a whole number of blocks to a piece.
    pub fn block_len(&self) -> u32 {
        self.block_len
    }

    /// How many blocks a piece is in. Zero for a piece that is not there, so that a peer
    /// cannot make this answer by being wrong.
    pub fn blocks_per_piece(&self, piece: u32) -> u32 {
        if piece >= self.pieces || self.block_len == 0 {
            return 0;
        }
        let block_len = u64::from(self.block_len);
        let size = self.meta.piece_size(piece);
        size.div_ceil(block_len) as u32
    }

    /// The size of one block in bytes: the block size, except in a piece's last block,
    /// which is what is left of the piece. Zero when there is no such block, so that a
    /// peer cannot make this answer by being wrong.
    pub fn block_size(&self, piece: u32, block: u32) -> u32 {
        self.block_in(piece, block).unwrap_or(0)
    }

    /// Whether a piece is whole and checked. False for a piece that is not there.
    pub fn has_piece(&self, piece: u32) -> bool {
        self.have.get(piece as usize).is_some_and(|whole| *whole != 0)
    }

    /// Whether one block of a piece is held. Every block of a piece that is whole counts
    /// as held: its data is in the file and has been checked against the torrent's hash of
    /// that piece, so it is there to be read even though its own byte has been cleared.
    pub fn has_block(&self, piece: u32, block: u32) -> bool {
        self.block_in(piece, block).is_some() && (self.has_piece(piece) || self.block_is_held(piece, block))
    }

    /// Keeps a block a peer sent, and checks the piece when its last block arrives.
    /// Returns true when the piece is now whole and its hash matched, false when it did
    /// not (the piece is thrown away, and asked for again from another peer).
    ///
    /// A block that is not wanted is refused rather than written, also with false: one
    /// that is out of range, one whose data is not `block_size` long, and one already
    /// held. A block of a piece that is whole and checked returns true, because the piece
    /// is there and there is nothing to do about it.
    pub fn write_block(&mut self, piece: u32, block: u32, data: &[u8]) -> std::io::Result<bool> {
        let Some(size) = self.block_in(piece, block) else {
            return Ok(false);
        };
        if data.len() != size as usize {
            return Ok(false);
        }
        if self.has_piece(piece) {
            return Ok(true);
        }
        if self.block_is_held(piece, block) {
            return Ok(false);
        }
        self.data.seek(SeekFrom::Start(self.block_offset(piece, block)))?;
        self.data.write_all(data)?;
        let index = self.block_index(piece, block);
        if let Some(held) = self.blocks.get_mut(index) {
            *held = 1;
        }
        if (0..self.blocks_per_piece(piece)).any(|block| !self.block_is_held(piece, block)) {
            return Ok(false);
        }
        // The piece is whole, so it is checked against the hash the torrent has for it.
        if self.piece_hash(piece)? == self.meta.hashes[piece as usize] {
            if let Some(whole) = self.have.get_mut(piece as usize) {
                *whole = 1;
            }
            self.forget_blocks(piece);
            return Ok(true);
        }
        // It is not the piece the torrent says it is, so none of its blocks are kept: it
        // is fetched again, from a peer that has it right.
        self.forget_blocks(piece);
        Ok(false)
    }

    /// Reads a block that is held into `out`, which must be `block_size` bytes long.
    ///
    /// A block of a whole piece reads back like any other: a piece is kept whole, so its
    /// blocks are in the data file whether or not they are still marked.
    pub fn read_block(&mut self, piece: u32, block: u32, out: &mut [u8]) -> std::io::Result<()> {
        let size = self.block_in(piece, block).ok_or_else(no_such_block)?;
        if out.len() != size as usize {
            return Err(Error::new(ErrorKind::InvalidInput, "the buffer is not a block long"));
        }
        if !self.has_block(piece, block) {
            return Err(Error::new(ErrorKind::NotFound, "the block has not arrived"));
        }
        self.data.seek(SeekFrom::Start(self.block_offset(piece, block)))?;
        self.data.read_exact(out)?;
        Ok(())
    }

    /// How many of the torrent's pieces are whole.
    pub fn done_pieces(&self) -> u32 {
        self.have.iter().filter(|whole| **whole != 0).count() as u32
    }

    /// Reads `out` out of a block that is held, starting `offset` bytes into that block.
    ///
    /// This is what the player reads through: it asks for a few kilobytes at a time, and
    /// reading a whole block for each of those would put sixteen times the traffic through
    /// the card for the same film.
    pub fn read_at(&mut self, piece: u32, block: u32, offset: u32, out: &mut [u8]) -> std::io::Result<()> {
        let size = self.block_in(piece, block).ok_or_else(no_such_block)?;
        let offset = offset.min(size);
        let wanted = out.len().min(size as usize - offset as usize);
        if !self.has_block(piece, block) {
            return Err(Error::new(ErrorKind::NotFound, "the block has not arrived"));
        }
        self.data.seek(SeekFrom::Start(
            self.block_offset(piece, block) + u64::from(offset),
        ))?;
        self.data.read_exact(&mut out[..wanted])?;
        Ok(())
    }

    /// How many bytes of the torrent are held: whole pieces, and the blocks of the rest.
    /// A block of a whole piece is not counted again.
    pub fn done_bytes(&self) -> u64 {
        let mut done = 0;
        for piece in 0..self.pieces {
            if self.has_piece(piece) {
                done += self.meta.piece_size(piece);
                continue;
            }
            for block in 0..self.blocks_per_piece(piece) {
                if self.block_is_held(piece, block) {
                    done += u64::from(self.block_in(piece, block).unwrap_or(0));
                }
            }
        }
        done
    }

    /// The torrent's whole size in bytes.
    pub fn total_bytes(&self) -> u64 {
        self.meta.total_length
    }

    /// The number of pieces.
    pub fn piece_count(&self) -> u32 {
        self.pieces
    }

    /// Writes the index files, so that a later run picks the download up where this one
    /// left it. Called when the torrent is closed, and now and then while it runs.
    ///
    /// Each file is written beside itself and moved over the top of the real name, so a
    /// console that loses its power leaves either the old file or the new one and never
    /// half of each. The description is put back too, when there is one to keep.
    ///
    /// The data is put out of the card's hands before the index that claims it is there is
    /// put out of them: an index that outlives the data it describes would have the reader
    /// serving zeros from a piece that is whole and checked.
    pub fn save(&self) -> std::io::Result<()> {
        self.data.sync_data()?;
        let mut have = self.head(self.have.len()).write();
        have.extend_from_slice(&self.have);
        put(&self.have_path(), &have)?;
        let mut blocks = self.head(self.blocks.len()).write();
        blocks.extend_from_slice(&self.blocks);
        put(&self.blocks_path(), &blocks)?;
        if let Some(torrent) = &self.torrent {
            put(&self.folder.join(format!("{}.torrent", self.name)), torrent)?;
        }
        Ok(())
    }

    /// The size of the block a piece's block starts within the torrent's data, in bytes.
    /// This is the offset a piece message's `begin` is measured from.
    ///
    /// A block that is not there has no offset of its own, but the number its piece and
    /// number work out to is still returned: this is arithmetic, and a peer must not be
    /// able to make it anything else.
    pub fn block_offset(&self, piece: u32, block: u32) -> u64 {
        u64::from(piece)
            .saturating_mul(u64::from(self.meta.piece_length))
            .saturating_add(u64::from(block).saturating_mul(u64::from(self.block_len)))
    }

    /// The size of one block, or None when there is no such block. The only way in to a
    /// piece's block: a peer decides which piece and block it is sending, and the wrong
    /// answer here is None rather than a panic.
    fn block_in(&self, piece: u32, block: u32) -> Option<u32> {
        if piece >= self.pieces || block >= self.blocks_per_piece(piece) {
            return None;
        }
        let begin = u64::from(block) * u64::from(self.block_len);
        let left = self.meta.piece_size(piece).saturating_sub(begin).min(u64::from(self.block_len));
        Some(left as u32)
    }

    /// Whether a block's byte is set in the blocks file. A whole piece's blocks are not,
    /// because the piece's own byte says all there is to say.
    fn block_is_held(&self, piece: u32, block: u32) -> bool {
        self.blocks.get(self.block_index(piece, block)).is_some_and(|held| *held != 0)
    }

    /// Where a block's byte sits.
    ///
    /// A piece length that is not a whole number of blocks leaves a piece with one block
    /// more than the round ones before it (what is left of the piece goes to its last
    /// block), so the address of a block is counted from a fixed width rather than from the
    /// piece length of the ones before it: with the piece length alone, the last block of
    /// one piece and the first of the next would share a byte, and clearing one would
    /// clear the other.
    fn block_index(&self, piece: u32, block: u32) -> usize {
        let width = self.per_piece as usize + 1;
        piece as usize * width + block as usize
    }

    fn have_path(&self) -> PathBuf {
        self.folder.join(format!("{}.have", self.name))
    }

    fn blocks_path(&self) -> PathBuf {
        self.folder.join(format!("{}.blocks", self.name))
    }

    /// The head an index file of `length` bytes is written with.
    fn head(&self, length: usize) -> Header {
        Header {
            info_hash: self.meta.info_hash,
            fingerprint: self.fingerprint,
            piece_count: self.pieces,
            piece_length: self.meta.piece_length,
            block_len: self.block_len,
            total_length: self.meta.total_length,
            length: length as u64,
        }
    }

    /// Reads an index file: what it says, when its head says it was written for this
    /// torrent in this shape and its bytes are all there, and nothing held when it does
    /// not. An index file that is short, truncated, or left by another build is a first
    /// run, not a failure.
    fn read_index(&self, path: &Path, length: usize) -> Vec<u8> {
        let empty = vec![0; length];
        let Ok(bytes) = std::fs::read(path) else {
            return empty;
        };
        match Header::read(&bytes) {
            Some((found, body)) if found == self.head(length) => body.to_vec(),
            _ => empty,
        }
    }

    /// A piece's SHA-1, read out of the data file in chunks: a piece can be far larger
    /// than anything worth keeping in memory.
    fn piece_hash(&mut self, piece: u32) -> std::io::Result<[u8; 20]> {
        self.data.seek(SeekFrom::Start(self.block_offset(piece, 0)))?;
        let mut hasher = Sha1::new();
        let mut chunk = [0u8; 64 * 1024];
        let mut left = self.meta.piece_size(piece);
        while left > 0 {
            let want = left.min(chunk.len() as u64) as usize;
            self.data.read_exact(&mut chunk[..want])?;
            hasher.update(&mut chunk[..want]);
            left -= want as u64;
        }
        Ok(hash_of(hasher.finalize().as_slice()))
    }

    /// Throws a piece's blocks away: because the piece is whole and is read as one piece,
    /// or because it did not hash to what the torrent says.
    fn forget_blocks(&mut self, piece: u32) {
        let begin = self.block_index(piece, 0);
        let count = self.blocks_per_piece(piece) as usize;
        for held in self.blocks.iter_mut().skip(begin).take(count) {
            *held = 0;
        }
    }
}

fn no_such_block() -> Error {
    Error::new(ErrorKind::InvalidInput, "the torrent has no such block")
}

/// A short digest of every piece's hash. Two descriptions can have the same hash, the
/// same piece length and the same number of pieces and still check their pieces
/// differently, and the index files of one are then no use to the other.
fn digest_of_hashes(hashes: &[[u8; 20]]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    for hash in hashes {
        hasher.update(hash);
    }
    hash_of(hasher.finalize().as_slice())
}

/// Twenty bytes of a finished digest.
fn hash_of(digest: &[u8]) -> [u8; 20] {
    let mut hash = [0u8; 20];
    let count = digest.len().min(20);
    hash[..count].copy_from_slice(&digest[..count]);
    hash
}

/// Writes a file the way a console that can lose its power needs to: into a `.part` file
/// beside it, flushed to the disk, and then moved over the real name, so that what is
/// there is either all of the new file or all of the old one.
fn put(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    let part = path.with_file_name(name);
    {
        let mut file = File::create(&part)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    put_in_place(&part, path)
}

/// The head of an index file: which torrent it is about, and how that torrent's pieces are
/// cut into blocks. A file whose head is not this torrent's is not read as an index of
/// it, whatever its name says.
#[derive(Clone, Copy, PartialEq)]
struct Header {
    info_hash: [u8; 20],
    fingerprint: [u8; 20],
    piece_count: u32,
    piece_length: u32,
    block_len: u32,
    total_length: u64,
    /// The bytes that follow the head.
    length: u64,
}

impl Header {
    fn write(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEAD + self.length as usize);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.info_hash);
        bytes.extend_from_slice(&self.fingerprint);
        bytes.extend_from_slice(&self.piece_count.to_le_bytes());
        bytes.extend_from_slice(&self.piece_length.to_le_bytes());
        bytes.extend_from_slice(&self.block_len.to_le_bytes());
        bytes.extend_from_slice(&self.total_length.to_le_bytes());
        bytes.extend_from_slice(&self.length.to_le_bytes());
        bytes
    }

    /// The head of a file, and the bytes after it. None when the file is not one of ours,
    /// is too short to hold a head, or says its own length and does not have it.
    fn read(bytes: &[u8]) -> Option<(Self, &[u8])> {
        if bytes.len() < HEAD || &bytes[..8] != MAGIC {
            return None;
        }
        let head = Header {
            info_hash: bytes.get(8..28)?.try_into().ok()?,
            fingerprint: bytes.get(28..48)?.try_into().ok()?,
            piece_count: word(bytes, 48)?,
            piece_length: word(bytes, 52)?,
            block_len: word(bytes, 56)?,
            total_length: wide(bytes, 60)?,
            length: wide(bytes, 68)?,
        };
        let body = bytes.get(HEAD..)?;
        (head.length as usize == body.len()).then_some((head, body))
    }
}

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn wide(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    use crate::torrent::metadata::File;

    /// A folder of this test's own under the system's temporary one: the tests run at the
    /// same time, and two runs of the same test must not be left looking at each other's
    /// files.
    fn folder(name: &str) -> PathBuf {
        static RUN: AtomicU32 = AtomicU32::new(0);
        let number = RUN.fetch_add(1, Ordering::Relaxed);
        let unique = std::env::temp_dir().join(format!(
            "stremio-torrent-storage-{}-{name}-{number}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&unique);
        unique
    }

    /// The torrent's data: one pattern over its whole length, so that every piece, and
    /// every block of it, is a part of the same data, and one byte changed in a piece
    /// changes its hash.
    fn bytes_at(offset: u64, length: u64) -> Vec<u8> {
        (offset..offset + length).map(|at| (at * 31 % 251) as u8).collect()
    }

    /// A piece's size: the piece length, or whatever is left of the torrent for the last.
    fn size_of(piece: u32, piece_length: u32, total: u64) -> u64 {
        let start = u64::from(piece) * u64::from(piece_length);
        total.saturating_sub(start).min(u64::from(piece_length))
    }

    /// A torrent of `count` pieces of `piece_length` bytes in one file, whose hashes are
    /// of `bytes_at`, so that a piece written from that pattern checks.
    fn torrent(info_hash: [u8; 20], piece_length: u32, count: u32, total: u64) -> Meta {
        let hashes = (0..count)
            .map(|piece| hash_of_piece(piece, piece_length, total))
            .collect();
        Meta {
            info_hash,
            name: "a film".to_owned(),
            piece_length,
            hashes,
            files: vec![File {
                path: "a film.mkv".to_owned(),
                length: total,
                offset: 0,
            }],
            total_length: total,
            private: false,
        }
    }

    fn hash_of_piece(piece: u32, piece_length: u32, total: u64) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(bytes_at(
            u64::from(piece) * u64::from(piece_length),
            size_of(piece, piece_length, total),
        ));
        hash_of(hasher.finalize().as_slice())
    }

    /// The data of one block, as a peer that has the torrent would send it.
    fn block(storage: &Storage, piece: u32, block: u32) -> Vec<u8> {
        bytes_at(
            storage.block_offset(piece, block),
            u64::from(storage.block_size(piece, block)),
        )
    }

    fn open(folder: &Path, meta: &Meta) -> Storage {
        Storage::open(folder, meta, None).expect("the torrent opens")
    }

    /// Pieces of 128 KiB, two blocks of 64 KiB to a piece: the shape a fetched torrent is
    /// usually in, and one where the counts move by half a piece at a time.
    const PIECE: u32 = 128 * 1024;

    #[test]
    fn a_piece_is_whole_only_when_its_last_block_arrives() {
        let folder = folder("whole");
        let total = 3 * u64::from(PIECE);
        let meta = torrent([0x11; 20], PIECE, 3, total);
        let mut storage = open(&folder, &meta);
        assert_eq!(storage.piece_count(), 3);
        assert_eq!(storage.total_bytes(), total);
        assert_eq!(storage.done_pieces(), 0);
        assert_eq!(storage.done_bytes(), 0);

        for piece in 0..3 {
            let blocks = storage.blocks_per_piece(piece);
            for part in 0..blocks {
                let data = block(&storage, piece, part);
                assert!(!storage.has_piece(piece));
                // Every block but the last one leaves the piece unfinished, and the last
                // one checks it: that is the one that says true.
                let last = part + 1 == blocks;
                assert_eq!(storage.write_block(piece, part, &data).unwrap(), last);
                assert!(storage.has_block(piece, part));
                assert_eq!(storage.has_piece(piece), last);
                assert_eq!(storage.done_pieces(), piece + u32::from(last));
                // The blocks of this piece so far, and the pieces before it whole.
                let held = u64::from(piece) * u64::from(PIECE) + u64::from(part + 1) * u64::from(storage.block_len());
                assert_eq!(storage.done_bytes(), held, "piece {piece} block {part}");
            }
            assert!(storage.has_piece(piece));
            assert_eq!(storage.done_pieces(), piece + 1);
            assert_eq!(storage.done_bytes(), u64::from(piece + 1) * u64::from(PIECE));
        }
        assert_eq!(storage.done_bytes(), storage.total_bytes());
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A piece shorter than a block is fetched as one block, and is whole as soon as that
    /// block has arrived.
    #[test]
    fn a_piece_shorter_than_a_block_is_one_block() {
        let folder = folder("short");
        let piece_length = 16 * 1024;
        let meta = torrent([0x12; 20], piece_length, 3, 3 * u64::from(piece_length));
        let mut storage = open(&folder, &meta);
        assert_eq!(storage.block_len(), piece_length);
        assert_eq!(storage.blocks_per_piece(0), 1);
        let first = block(&storage, 0, 0);
        assert!(storage.write_block(0, 0, &first).unwrap());
        assert!(storage.has_piece(0));
        assert_eq!(storage.done_bytes(), u64::from(piece_length));
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// How a torrent's pieces are cut into blocks: 64 KiB where a piece divides into it,
    /// the piece's own length where the piece is shorter, and what is left over of a piece
    /// that does not divide evenly to its last block.
    #[test]
    fn pieces_are_cut_into_blocks() {
        let folder = folder("cut");

        // 256 KiB pieces: four blocks of 64 KiB.
        let wide = torrent([0x21; 20], 256 * 1024, 2, 2 * 256 * 1024);
        let storage = open(&folder, &wide);
        assert_eq!(storage.block_len(), 64 * 1024);
        assert_eq!(storage.blocks_per_piece(0), 4);
        assert_eq!(storage.block_size(0, 0), 64 * 1024);
        assert_eq!(storage.block_size(0, 3), 64 * 1024);
        assert_eq!(storage.block_offset(0, 0), 0);
        assert_eq!(storage.block_offset(0, 2), 131072);
        assert_eq!(storage.block_offset(1, 0), 262144);

        // 16 KiB pieces: shorter than a block, so a block is the piece.
        let little = torrent([0x22; 20], 16 * 1024, 2, 2 * 16 * 1024);
        let storage = open(&folder, &little);
        assert_eq!(storage.block_len(), 16 * 1024);
        assert_eq!(storage.blocks_per_piece(0), 1);
        assert_eq!(storage.block_size(0, 0), 16 * 1024);
        assert_eq!(storage.block_offset(1, 0), 16 * 1024);

        // 100 KiB pieces: not a whole number of 64 KiB blocks, and shorter than two of
        // them, so the piece is one block.
        let odd = torrent([0x23; 20], 100 * 1024, 1, 100 * 1024);
        let storage = open(&folder, &odd);
        assert_eq!(storage.block_len(), 100 * 1024);
        assert_eq!(storage.blocks_per_piece(0), 1);
        assert_eq!(storage.block_size(0, 0), 100 * 1024);

        // 200 KiB pieces: three blocks do not come to the whole piece, so the last block
        // is what is left of it.
        let uneven = torrent([0x24; 20], 200 * 1024, 1, 200 * 1024);
        let storage = open(&folder, &uneven);
        assert_eq!(storage.block_len(), 68266);
        assert_eq!(storage.blocks_per_piece(0), 4);
        assert_eq!(storage.block_size(0, 2), 68266);
        assert_eq!(storage.block_size(0, 3), 2);
        let blocks: u64 = (0..storage.blocks_per_piece(0))
            .map(|block| u64::from(storage.block_size(0, block)))
            .sum();
        assert_eq!(blocks, 200 * 1024);

        // A last piece shorter than the piece length is as many blocks as fit in it.
        let short = torrent([0x25; 20], PIECE, 3, 2 * u64::from(PIECE) + 5000);
        let storage = open(&folder, &short);
        assert_eq!(storage.blocks_per_piece(0), 2);
        assert_eq!(storage.blocks_per_piece(1), 2);
        assert_eq!(storage.blocks_per_piece(2), 1);
        assert_eq!(storage.block_size(2, 0), 5000);
        assert_eq!(storage.block_offset(2, 0), 2 * u64::from(PIECE));

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A piece length that is not a whole number of blocks gives every piece an extra
    /// block, and the byte that says a block is held must not be shared with the piece
    /// after it: when it was, completing one piece threw away the first block of the next,
    /// and that piece could then never be finished.
    #[test]
    fn blocks_of_adjacent_pieces_keep_their_own_place() {
        let folder = folder("adjacent");
        let piece_length = 200 * 1024;
        let total = 4 * u64::from(piece_length);
        let meta = torrent([0x33; 20], piece_length, 4, total);
        let mut storage = open(&folder, &meta);
        assert!(storage.blocks_per_piece(0) > 3, "the piece length is the uneven one");

        // The first piece, whole.
        for part in 0..storage.blocks_per_piece(0) {
            storage.write_block(0, part, &block(&storage, 0, part)).expect("written");
        }
        assert!(storage.has_piece(0), "the first piece checks out");

        // Then the second, which must not have lost its first block to the first.
        for part in 0..storage.blocks_per_piece(1) {
            assert!(
                !storage.has_piece(1),
                "the second piece is finished before it is fetched"
            );
            storage.write_block(1, part, &block(&storage, 1, part)).expect("written");
        }
        assert!(storage.has_piece(1), "the second piece checks out too");
        assert_eq!(storage.done_pieces(), 2);
        assert_eq!(storage.done_bytes(), 2 * u64::from(piece_length));
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The session's peer threads and its reader all reach the storage, so it has to be
    /// able to go from one thread to another.
    #[test]
    fn the_storage_can_be_moved_between_threads() {
        fn carried<T: Send>() {}
        carried::<Storage>();
    }

    /// A piece or block that is not there has no size: zero rather than a panic, because
    /// a peer says which piece and block it is sending.
    #[test]
    fn blocks_that_are_not_there_have_no_size() {
        let folder = folder("range");
        let meta = torrent([0x31; 20], PIECE, 2, 2 * u64::from(PIECE));
        let storage = open(&folder, &meta);
        assert_eq!(storage.blocks_per_piece(2), 0);
        assert_eq!(storage.block_size(2, 0), 0);
        assert_eq!(storage.block_size(0, 2), 0);
        assert_eq!(storage.block_size(u32::MAX, u32::MAX), 0);
        assert!(!storage.has_piece(2));
        assert!(!storage.has_piece(u32::MAX));
        assert!(!storage.has_block(2, 0));
        assert!(!storage.has_block(0, 2));
        assert!(!storage.has_block(u32::MAX, u32::MAX));
        assert_eq!(storage.done_pieces(), 0);
        assert_eq!(storage.done_bytes(), 0);
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// What was written comes back byte for byte, a block that has not arrived is refused,
    /// and a buffer that is not a block long is refused with it.
    #[test]
    fn blocks_read_back_as_they_were_written() {
        let folder = folder("read");
        let meta = torrent([0x41; 20], PIECE, 2, 2 * u64::from(PIECE));
        let mut storage = open(&folder, &meta);
        let first = block(&storage, 0, 0);
        let second = block(&storage, 0, 1);
        let other = block(&storage, 1, 0);
        let mut buffer = vec![0u8; storage.block_size(0, 0) as usize];

        // Nothing has arrived yet.
        assert!(storage.read_block(0, 0, &mut buffer).is_err());
        assert!(!storage.write_block(0, 0, &first).unwrap());

        storage.read_block(0, 0, &mut buffer).unwrap();
        assert_eq!(buffer, first);
        // A buffer of the wrong size is refused, whichever way about it is wrong.
        assert!(storage.read_block(0, 0, &mut buffer[..1]).is_err());
        assert!(storage.read_block(0, 0, &mut [0u8; 1024]).is_err());
        assert!(storage.read_block(9, 0, &mut buffer).is_err());
        assert!(storage.read_block(0, 9, &mut buffer).is_err());

        // The piece completes, and its blocks are still there to read.
        assert!(storage.write_block(0, 1, &second).unwrap());
        buffer.clear();
        buffer.resize(storage.block_size(0, 1) as usize, 0);
        storage.read_block(0, 1, &mut buffer).unwrap();
        assert_eq!(buffer, second);
        buffer.clear();
        buffer.resize(storage.block_size(0, 0) as usize, 0);
        storage.read_block(0, 0, &mut buffer).unwrap();
        assert_eq!(buffer, first);

        // A second piece is a different piece, and reads as its own data.
        assert!(!storage.write_block(1, 0, &other).unwrap());
        assert!(storage.write_block(1, 1, &block(&storage, 1, 1)).unwrap());
        buffer.clear();
        buffer.resize(storage.block_size(1, 0) as usize, 0);
        storage.read_block(1, 0, &mut buffer).unwrap();
        assert_eq!(buffer, other);

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A piece that does not hash to what the torrent says is thrown away whole: none of
    /// its blocks are left half believed, and it is fetched again from another peer.
    #[test]
    fn a_piece_that_does_not_hash_is_thrown_away() {
        let folder = folder("corrupt");
        let meta = torrent([0x51; 20], PIECE, 1, u64::from(PIECE));
        let mut storage = open(&folder, &meta);
        let first = block(&storage, 0, 0);
        let second = block(&storage, 0, 1);

        // The second block is not the block the torrent says it is.
        let mut wrong = second.clone();
        wrong[0] = wrong[0].wrapping_add(1);
        assert!(!storage.write_block(0, 0, &first).unwrap());
        assert!(!storage.write_block(0, 1, &wrong).unwrap());

        assert!(!storage.has_piece(0));
        assert!(!storage.has_block(0, 0));
        assert!(!storage.has_block(0, 1));
        assert_eq!(storage.done_pieces(), 0);
        assert_eq!(storage.done_bytes(), 0);

        // Asked for again, and sent right this time.
        assert!(!storage.write_block(0, 0, &first).unwrap());
        assert!(storage.write_block(0, 1, &second).unwrap());
        assert!(storage.has_piece(0));
        assert_eq!(storage.done_pieces(), 1);
        assert_eq!(storage.done_bytes(), u64::from(PIECE));
        let mut buffer = vec![0u8; storage.block_size(0, 1) as usize];
        storage.read_block(0, 1, &mut buffer).unwrap();
        assert_eq!(buffer, second);

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// What was fetched is still there after a restart: the index files say which pieces
    /// are whole and which blocks are held, and the data file has the bytes behind them.
    #[test]
    fn what_was_fetched_survives_a_restart() {
        let folder = folder("restart");
        let meta = torrent([0x61; 20], PIECE, 3, 3 * u64::from(PIECE));
        let description = b"d8:announce3:truel".to_vec();
        let first;
        {
            let mut storage =
                Storage::open(&folder, &meta, Some(&description)).expect("the torrent opens");
            let second = block(&storage, 0, 1);
            let third = block(&storage, 1, 1);
            let fourth = block(&storage, 2, 0);
            first = block(&storage, 0, 0);
            assert!(!storage.write_block(0, 0, &first).unwrap());
            assert!(storage.write_block(0, 1, &second).unwrap());
            assert!(!storage.write_block(1, 1, &third).unwrap());
            assert!(!storage.write_block(2, 0, &fourth).unwrap());
            storage.save().unwrap();
        }
        // The description is kept beside the data, for a run that has to fetch it again.
        let kept = folder.join(format!("{}.torrent", info_hash_to_hex(&meta.info_hash)));
        assert_eq!(std::fs::read(&kept).unwrap(), description);

        let mut storage = open(&folder, &meta);
        assert!(storage.has_piece(0));
        assert!(storage.has_block(1, 1));
        assert!(!storage.has_block(1, 0));
        assert!(storage.has_block(2, 0));
        assert!(!storage.has_piece(1));
        assert_eq!(storage.done_pieces(), 1);
        let block_len = u64::from(storage.block_len());
        assert_eq!(storage.done_bytes(), u64::from(PIECE) + 2 * block_len);
        let mut buffer = vec![0u8; storage.block_size(0, 0) as usize];
        storage.read_block(0, 0, &mut buffer).unwrap();
        assert_eq!(buffer, first);
        assert_eq!(std::fs::read(&kept).unwrap(), description);

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Index files that are not this torrent's are not believed: another torrent under the
    /// same name has its pieces in other places, and reading them as if they were this
    /// one's would play one film out of another's blocks.
    #[test]
    fn another_torrents_index_is_not_believed() {
        let folder = folder("other");
        let meta = torrent([0x71; 20], PIECE, 2, 2 * u64::from(PIECE));
        {
            let mut storage = open(&folder, &meta);
            let first = block(&storage, 0, 0);
            let second = block(&storage, 0, 1);
            let third = block(&storage, 1, 0);
            assert!(!storage.write_block(0, 0, &first).unwrap());
            assert!(storage.write_block(0, 1, &second).unwrap());
            assert!(!storage.write_block(1, 0, &third).unwrap());
            storage.save().unwrap();
        }

        // The same hash and the same shape, and the pieces are checked differently.
        let other = Meta {
            hashes: vec![[0x72; 20]; 2],
            ..meta.clone()
        };
        let storage = open(&folder, &other);
        assert_eq!(storage.done_pieces(), 0);
        assert_eq!(storage.done_bytes(), 0);
        assert!(!storage.has_piece(0));
        assert!(!storage.has_piece(1));
        assert!(!storage.has_block(0, 0));
        assert!(!storage.has_block(1, 0));

        // And a description that shapes its pieces differently is not believed either.
        let smaller = torrent([0x71; 20], 64 * 1024, 2, 2 * 64 * 1024);
        let storage = open(&folder, &smaller);
        assert_eq!(storage.done_pieces(), 0);
        assert!(!storage.has_block(1, 0));

        // An index file that is not one of ours at all is a first run, not a failure. This one is
        // thrown away on its own: the other is still read, and what it says still stands.
        let have = folder.join(format!("{}.have", info_hash_to_hex(&meta.info_hash)));
        std::fs::write(&have, b"not an index").unwrap();
        let storage = open(&folder, &meta);
        assert_eq!(storage.done_pieces(), 0);
        assert!(storage.has_block(1, 0));
        assert_eq!(storage.done_bytes(), u64::from(storage.block_len()));

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A data file of the wrong length is made the length the description says, so that
    /// every piece is at the place it is read from.
    #[test]
    fn a_data_file_of_the_wrong_length_is_corrected() {
        let folder = folder("length");
        let meta = torrent([0x81; 20], PIECE, 2, 2 * u64::from(PIECE));
        let data = folder.join(format!("{}.data", info_hash_to_hex(&meta.info_hash)));

        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(&data, [0u8; 100]).unwrap();
        open(&folder, &meta);
        assert_eq!(std::fs::metadata(&data).unwrap().len(), meta.total_length);

        std::fs::write(&data, vec![7u8; meta.total_length as usize + 4096]).unwrap();
        open(&folder, &meta);
        assert_eq!(std::fs::metadata(&data).unwrap().len(), meta.total_length);

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A block of nothing, a buffer of the wrong size, and a block of a piece that is
    /// whole are all answered: what a peer sends is refused, never believed and never a
    /// crash.
    #[test]
    fn what_a_peer_sends_is_answered_not_panicked_at() {
        let folder = folder("peer");
        let meta = torrent([0x91; 20], PIECE, 2, 2 * u64::from(PIECE));
        let mut storage = open(&folder, &meta);
        let data = block(&storage, 0, 0);

        // A piece and a block of nothing.
        assert!(!storage.write_block(2, 0, &data).unwrap());
        assert!(!storage.write_block(0, 2, &data).unwrap());
        assert!(!storage.write_block(u32::MAX, u32::MAX, &data).unwrap());
        // A block of the wrong length, in either direction.
        assert!(!storage.write_block(0, 0, &data[..data.len() - 1]).unwrap());
        assert!(!storage.write_block(0, 0, &vec![0u8; data.len() + 1]).unwrap());
        assert!(!storage.write_block(0, 0, &[]).unwrap());
        assert_eq!(storage.done_bytes(), 0);
        assert!(!storage.has_piece(0));
        assert!(!storage.has_block(0, 0));

        // A block that has already been sent is not taken again.
        assert!(!storage.write_block(0, 0, &data).unwrap());
        assert!(!storage.write_block(0, 0, &data).unwrap());
        assert!(storage.has_block(0, 0));
        assert_eq!(storage.done_bytes(), data.len() as u64);

        // A block of a piece that is whole: the piece is checked and in place, so there
        // is nothing to do about it, and nothing is fetched twice.
        let second = block(&storage, 0, 1);
        assert!(storage.write_block(0, 1, &second).unwrap());
        assert!(storage.has_piece(0));
        assert!(storage.write_block(0, 0, &data).unwrap());
        assert!(storage.write_block(0, 1, &second).unwrap());
        assert!(!storage.write_block(0, 0, &data[..1]).unwrap());
        assert_eq!(storage.done_bytes(), u64::from(PIECE));
        assert_eq!(storage.done_pieces(), 1);

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Saving is done more than once in a run, and is done on a folder that was made a
    /// moment ago: neither is a problem, and what was saved is there for the next run.
    #[test]
    fn saving_twice_and_saving_a_new_folder() {
        let folder = folder("save");
        let meta = torrent([0xa1; 20], PIECE, 1, u64::from(PIECE));
        let mut storage = open(&folder, &meta);
        // Nothing has been fetched, and the folder was made by `open` a moment ago.
        storage.save().unwrap();
        storage.save().unwrap();

        let first = block(&storage, 0, 0);
        let second = block(&storage, 0, 1);
        assert!(!storage.write_block(0, 0, &first).unwrap());
        storage.save().unwrap();
        assert!(storage.write_block(0, 1, &second).unwrap());
        storage.save().unwrap();
        storage.save().unwrap();
        drop(storage);

        let storage = open(&folder, &meta);
        assert!(storage.has_piece(0));
        assert_eq!(storage.done_pieces(), 1);
        assert_eq!(storage.done_bytes(), meta.total_length);

        let _ = std::fs::remove_dir_all(&folder);
    }
}