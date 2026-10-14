//! The torrent's own description: what its pieces are, what files they make up, and which
//! of those files the stream that is being played is.

use sha1::{Digest, Sha1};

use super::bencode;

/// Every piece's hash is a SHA-1, which is twenty bytes.
const HASH_LENGTH: usize = 20;

/// The longest piece a description may claim to be cut into: 32 MiB. Nothing larger can be
/// held in memory to check a piece against its hash, and no torrent is cut that fine.
const MAX_PIECE_LENGTH: u32 = 32 * 1024 * 1024;

/// How large a `.torrent` file may be before it is refused. Beyond a few hundred bytes it is
/// all piece hashes, and a torrent that is never played is not worth reading in.
const MAX_TORRENT_BYTES: usize = 64 * 1024 * 1024;

/// The extensions a file that holds a video is usually named with. `iso` and `ts` are there
/// for the streams that are a whole disc or a transport stream rather than a film.
const VIDEO_EXTENSIONS: [&str; 12] = [
    "mkv", "mp4", "avi", "m4v", "mov", "wmv", "ts", "m2ts", "mpg", "mpeg", "webm", "iso",
];

/// One file inside a torrent.
#[derive(Clone, Debug, PartialEq)]
pub struct File {
    /// The file's path within the torrent, its parts joined with `/`.
    pub path: String,
    pub length: u64,
    /// Where the file's data starts within the torrent's whole data, in bytes. Pieces hold
    /// every file in order, so the files' offsets follow from the lengths before them.
    pub offset: u64,
}

/// A torrent's description, as its `info` dictionary.
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    pub info_hash: [u8; 20],
    /// The torrent's name, and the folder its files sit in when there are several.
    pub name: String,
    pub piece_length: u32,
    /// Each piece's SHA-1, in order.
    pub hashes: Vec<[u8; 20]>,
    pub files: Vec<File>,
    /// Every file's bytes together.
    pub total_length: u64,
    /// A private torrent (BEP 27): its peers are one account, and its hashes are asked
    /// for over the peer protocol rather than sent in a handshake.
    pub private: bool,
}

/// The file that was chosen to play, and where it sits within the torrent.
#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    pub file: usize,
    /// The offset of the chosen file's data within the torrent.
    pub base: u64,
    pub length: u64,
}

/// The hash of `bytes`, which is what identifies one description and no other.
fn sha1_of(bytes: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut hash = [0u8; HASH_LENGTH];
    hash.copy_from_slice(out.as_slice());
    hash
}

/// A file's size as the description gives it. It must be a real one: a description with a
/// file of no bytes, or of a negative number of them, is describing something else.
fn length_of(node: &bencode::Node) -> Result<u64, String> {
    let length = node.int().ok_or("a file has no length")?;
    if length <= 0 {
        return Err(format!(
            "a file's length of {length} is not one a torrent holds"
        ));
    }
    Ok(length as u64)
}

/// A file's path: the parts the description gives it, joined with `/`, which is the way a
/// name is written wherever the file itself is written.
fn path_of(node: &bencode::Node) -> Result<String, String> {
    let parts = node
        .get("path")
        .and_then(|path| path.list())
        .ok_or("a file has no path")?;
    let mut names = Vec::with_capacity(parts.len());
    for part in parts {
        let part = part
            .bytes()
            .ok_or("a file's path has a part that is not a name")?;
        if !part.is_empty() {
            names.push(String::from_utf8_lossy(part).into_owned());
        }
    }
    if names.is_empty() {
        return Err("a file has no name to it".to_owned());
    }
    Ok(names.join("/"))
}

/// Whether a file's name looks like it holds a video. The extension says so far more often
/// than not, and a name without one is left to the size instead.
fn looks_like_video(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    VIDEO_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
}

/// The largest of the files that `keep` admits, and the first of them when several are the
/// same size: a torrent holding two copies of one file gives no reason to prefer either.
fn largest_of(files: &[File], keep: impl Fn(&File) -> bool) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (index, file) in files.iter().enumerate() {
        if keep(file) && best.map_or(true, |seen| file.length > files[seen].length) {
            best = Some(index);
        }
    }
    best
}

impl Meta {
    /// Reads the description out of a `.torrent` file's bytes, which also carry the
    /// trackers and the piece hashes' authority (the hash of `info`).
    pub fn from_torrent(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_TORRENT_BYTES {
            return Err(format!(
                "the torrent file is {} bytes, which is too large to be a description",
                bytes.len()
            ));
        }
        let file = bencode::parse(bytes, bytes.len())
            .map_err(|error| format!("the torrent file will not parse: {error}"))?;
        if file.dict().is_none() {
            return Err("the torrent file is not a dictionary".to_owned());
        }
        // Everything besides `info` is the trackers' and the creator's to write, and is
        // passed over however odd it is. What is read is the description alone, and over
        // exactly the bytes it spans, since that is the range its hash was made from.
        let (start, end) = bencode::dictionary_span(bytes, "info")
            .map_err(|error| format!("the torrent file has no info dictionary: {error}"))?;
        let info = &bytes[start..end];
        Self::from_info(info, &sha1_of(info))
    }

    /// Reads the description out of an `info` dictionary on its own, as peers send it
    /// (BEP 9). Fails when the dictionary's hash is not `info_hash`, which is how a peer
    /// that answers with another torrent is caught.
    pub fn from_info(info: &[u8], info_hash: &[u8; 20]) -> Result<Self, String> {
        if &sha1_of(info) != info_hash {
            return Err("this description is for another torrent".to_owned());
        }
        let node = bencode::parse(info, info.len())
            .map_err(|error| format!("the description will not parse: {error}"))?;
        if node.dict().is_none() {
            return Err("the description is not a dictionary".to_owned());
        }
        let name = node
            .get("name")
            .and_then(|name| name.bytes())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .ok_or("the description has no name to it")?;

        let length = node
            .get("piece length")
            .and_then(|length| length.int())
            .ok_or("the description says nothing about how long its pieces are")?;
        if length <= 0 || length > i64::from(MAX_PIECE_LENGTH) {
            return Err(format!(
                "the piece length {length} is not one a torrent is cut into"
            ));
        }
        let piece_length = length as u32;

        let pieces = node
            .get("pieces")
            .and_then(|pieces| pieces.bytes())
            .ok_or("the description has no piece hashes")?;
        if pieces.is_empty() || pieces.len() % HASH_LENGTH != 0 {
            return Err(format!(
                "{} bytes of piece hashes is not a whole number of 20-byte ones",
                pieces.len()
            ));
        }
        let hashes: Vec<[u8; 20]> = pieces
            .chunks_exact(HASH_LENGTH)
            .map(|chunk| {
                let mut hash = [0u8; HASH_LENGTH];
                hash.copy_from_slice(chunk);
                hash
            })
            .collect();

        // BEP 27: `private` is a flag, and it is 1 or it is not there at all. A description
        // carrying anything else under that name is not one to refuse to seed.
        let private = node
            .get("private")
            .and_then(|flag| flag.int())
            .is_some_and(|flag| flag == 1);

        let files = if let Some(length) = node.get("length") {
            // One file in the whole torrent, and its path is the torrent's name.
            vec![File {
                path: name.clone(),
                length: length_of(length)?,
                offset: 0,
            }]
        } else {
            let entries = node
                .get("files")
                .and_then(|files| files.list())
                .ok_or("the description says of neither one file nor several")?;
            if entries.is_empty() {
                return Err("the description lists no files at all".to_owned());
            }
            let mut files = Vec::with_capacity(entries.len());
            let mut offset = 0u64;
            for entry in entries {
                let file = File {
                    path: path_of(entry)?,
                    length: length_of(entry.get("length").ok_or("a file has no length")?)?,
                    offset,
                };
                offset = offset
                    .checked_add(file.length)
                    .ok_or("the files' lengths do not add up to anything")?;
                files.push(file);
            }
            files
        };

        let total_length = files
            .iter()
            .try_fold(0u64, |total, file| total.checked_add(file.length))
            .ok_or("the files' lengths do not add up to anything")?;
        // How many pieces there are is settled by the files and the piece length together,
        // so a description that has been cut short, or written to lie about what it holds,
        // is caught here rather than at the end of a download that could never finish.
        let wanted = total_length.div_ceil(u64::from(piece_length));
        if hashes.len() as u64 != wanted {
            return Err(format!(
                "{total_length} bytes of files need {wanted} pieces, but {} are described",
                hashes.len()
            ));
        }

        Ok(Self {
            info_hash: *info_hash,
            name,
            piece_length,
            hashes,
            files,
            total_length,
            private,
        })
    }

    pub fn piece_count(&self) -> u32 {
        self.hashes.len() as u32
    }

    /// The piece's size in bytes: `piece_length`, except the last piece's, which is
    /// whatever is left of the torrent.
    pub fn piece_size(&self, piece: u32) -> u64 {
        let count = self.hashes.len() as u64;
        if count == 0 || piece as u64 >= count {
            return 0;
        }
        let piece_length = u64::from(self.piece_length);
        if piece as u64 + 1 == count {
            // The last piece is whatever the whole pieces before it did not cover.
            return self
                .total_length
                .saturating_sub(piece as u64 * piece_length);
        }
        piece_length
    }

    /// Which file to play. `hint` is the add-on's own file number, `must_include` the
    /// parts its name must have, and `length` the size the stream gave. Falls back to the
    /// largest file with a name that looks like a video, and then to the largest file
    /// there is.
    pub fn select(
        &self,
        hint: Option<usize>,
        must_include: &[String],
        length: Option<u64>,
    ) -> Result<Selection, String> {
        if self.files.is_empty() {
            return Err("the torrent holds no files to play".to_owned());
        }
        // The add-on's own file number is the best answer there is while it still points
        // somewhere. An add-on can be a version behind the torrent it gave out, so one
        // that no longer does is passed over rather than failed on.
        if let Some(file) = hint.filter(|file| *file < self.files.len()) {
            return Ok(self.chosen(file));
        }
        // The stream's own name is the next closest thing to a statement of which file it
        // is. Only names carrying every part of it are looked at, and among those the
        // largest, which is the film rather than a sample of it.
        if !must_include.is_empty() {
            let parts: Vec<String> = must_include
                .iter()
                .map(|part| part.to_lowercase())
                .collect();
            let wanted = largest_of(&self.files, |file| {
                let path = file.path.to_lowercase();
                parts.iter().all(|part| path.contains(part.as_str()))
            });
            if let Some(file) = wanted {
                return Ok(self.chosen(file));
            }
        }
        // The size the stream gave is the one number that cannot be out of date: a name may
        // have been changed, but a file of that many bytes is that file.
        if let Some(size) = length {
            if let Some(file) = largest_of(&self.files, |file| file.length == size) {
                return Ok(self.chosen(file));
            }
        }
        // What is left is a guess, and the best one available is the largest video, since
        // a torrent that is being played is nearly always mostly film.
        let file = largest_of(&self.files, |file| looks_like_video(&file.path))
            .or_else(|| largest_of(&self.files, |_| true))
            .ok_or("the torrent holds no files to play")?;
        Ok(self.chosen(file))
    }

    /// What is told back about one of the files: where it sits within the torrent, and how
    /// much of it there is.
    fn chosen(&self, file: usize) -> Selection {
        Selection {
            file,
            base: self.files[file].offset,
            length: self.files[file].length,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Puts an `info` dictionary into a `.torrent` file's own shell: a tracker, the list of
    /// them, and a comment. None of it is read, and none of it may make the reading fail.
    fn torrent_file(info: &[u8]) -> Vec<u8> {
        [
            b"d8:announce13:udp://tracker13:announce-listl13:udp://trackere\
              7:comment6:sample4:info",
            info,
            b"e",
        ]
        .concat()
    }

    /// Reads a description written out in a test, hashed as its own author hashed it.
    fn meta_of(info: &[u8]) -> Result<Meta, String> {
        Meta::from_info(info, &sha1_of(info))
    }

    /// One file of 20000 bytes, cut into two pieces of 16384, the last one of 3616.
    fn single_file_info() -> Vec<u8> {
        b"d6:lengthi20000e4:name8:film.mkv12:piece lengthi16384e\
          6:pieces40:0123456789012345678901234567890123456789e"
            .to_vec()
    }

    /// Three files of 30000, 1000 and 500 bytes: 31500 of them, which is two pieces, the
    /// second of them holding the 15116 bytes the first did not.
    fn multi_file_info() -> Vec<u8> {
        b"d5:filesl\
          d6:lengthi30000e4:pathl8:film.mkvee\
          d6:lengthi1000e4:pathl8:subs.srtee\
          d6:lengthi500e4:pathl9:cover.jpgeee\
          4:name9:film-pack12:piece lengthi16384e\
          6:pieces40:0123456789012345678901234567890123456789e"
            .to_vec()
    }

    /// A release with two qualities of the film and a subtitle: 4520 bytes in all, which is
    /// one piece. 1080p and 720p are what the stream's own name tells apart.
    fn release_info() -> Vec<u8> {
        b"d5:filesl\
          d6:lengthi3000e4:pathl39:Some.Release.2024.1080p.WEB-DL.x264.mkvee\
          d6:lengthi1500e4:pathl38:Some.Release.2024.720p.WEB-DL.x264.mkvee\
          d6:lengthi20e4:pathl4:Subs7:eng.srtee\
          e4:name17:Some.Release.202412:piece lengthi16384e\
          6:pieces20:abcdefghijklmnopqrst\
          e"
        .to_vec()
    }

    #[test]
    fn a_single_file_torrent_is_one_file_at_the_start() {
        let info = single_file_info();
        let meta = Meta::from_torrent(&torrent_file(&info)).expect("the torrent should read");

        assert_eq!(meta.name, "film.mkv");
        assert_eq!(meta.info_hash, sha1_of(&info));
        assert_eq!(meta.piece_length, 16384);
        assert_eq!(meta.files.len(), 1);
        assert_eq!(meta.files[0].path, "film.mkv");
        assert_eq!(meta.files[0].length, 20000);
        assert_eq!(meta.files[0].offset, 0);
        assert_eq!(meta.total_length, 20000);
        assert!(!meta.private);
    }

    #[test]
    fn every_piece_is_a_whole_one_but_the_last() {
        let meta = meta_of(&single_file_info()).expect("the description should read");

        assert_eq!(meta.piece_count(), 2);
        assert_eq!(meta.piece_size(0), 16384);
        // The second piece is what is left of 20000 after one whole piece.
        assert_eq!(meta.piece_size(1), 3616);
        // Past the last one there is no piece at all.
        assert_eq!(meta.piece_size(2), 0);
        assert_eq!(meta.piece_size(u32::MAX), 0);
    }

    #[test]
    fn several_files_sit_one_after_another() {
        let meta =
            Meta::from_torrent(&torrent_file(&multi_file_info())).expect("the torrent should read");

        assert_eq!(meta.name, "film-pack");
        assert_eq!(meta.files.len(), 3);
        assert_eq!(meta.files[0].path, "film.mkv");
        assert_eq!(meta.files[0].offset, 0);
        assert_eq!(meta.files[1].path, "subs.srt");
        assert_eq!(meta.files[1].offset, 30000);
        assert_eq!(meta.files[2].path, "cover.jpg");
        assert_eq!(meta.files[2].offset, 31000);
        assert_eq!(meta.total_length, 31500);
        assert_eq!(meta.piece_count(), 2);
        assert_eq!(meta.piece_size(0), 16384);
        assert_eq!(meta.piece_size(1), 15116);
    }

    #[test]
    fn the_description_must_be_the_one_that_was_asked_for() {
        let info = single_file_info();
        assert!(Meta::from_info(&info, &sha1_of(&info)).is_ok());

        let mut wrong = sha1_of(&info);
        wrong[0] ^= 0xff;
        let error = Meta::from_info(&info, &wrong).expect_err("another torrent is not this one");
        assert!(
            error.contains("another torrent"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_private_torrent_says_so() {
        assert!(!meta_of(&single_file_info()).expect("should read").private);

        let private = b"d6:lengthi20000e4:name8:film.mkv12:piece lengthi16384e\
                      6:pieces40:0123456789012345678901234567890123456789\
                      7:privatei1ee"
            .to_vec();
        assert!(meta_of(&private).expect("should read").private);
    }

    #[test]
    fn descriptions_that_make_no_sense_are_refused() {
        let bad: Vec<(&str, Vec<u8>)> = vec![
            (
                "no piece length",
                b"d6:lengthi20000e4:name8:film.mkv\
                  6:pieces40:0123456789012345678901234567890123456789e"
                    .to_vec(),
            ),
            (
                "a piece length of nothing",
                b"d6:lengthi20000e4:name8:film.mkv12:piece lengthi0e\
                  6:pieces40:0123456789012345678901234567890123456789e"
                    .to_vec(),
            ),
            (
                "a piece length past what can be held",
                b"d6:lengthi20000e4:name8:film.mkv12:piece lengthi33554433e\
                  6:pieces40:0123456789012345678901234567890123456789e"
                    .to_vec(),
            ),
            (
                "piece hashes that are not a whole number of them",
                b"d6:lengthi20000e4:name8:film.mkv12:piece lengthi16384e6:pieces41:\
                  01234567890123456789012345678901234567890e"
                    .to_vec(),
            ),
            (
                "no piece hashes at all",
                b"d6:lengthi20000e4:name8:film.mkv\
                  12:piece lengthi16384e6:pieces0:e"
                    .to_vec(),
            ),
            (
                "fewer pieces than the files need",
                b"d6:lengthi20000e4:name8:film.mkv12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "more pieces than the files need",
                b"d6:lengthi20000e4:name8:film.mkv12:piece lengthi16384e6:pieces60:\
                  0123456789012345678901234567890123456789\
                  0123456789012345678901234567890123456789\
                  0123456789012345678901234567890123456789e"
                    .to_vec(),
            ),
            (
                "a length of nothing",
                b"d6:lengthi0e4:name8:film.mkv12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "a length below nothing",
                b"d6:lengthi-20000e4:name8:film.mkv12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "no name",
                b"d6:lengthi20000e12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "no pieces",
                b"d6:lengthi20000e4:name8:film.mkv\
                  12:piece lengthi16384ee"
                    .to_vec(),
            ),
            (
                "no files at all",
                b"d4:name8:film.mkv12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "a list of no files",
                b"d5:filesle4:name5:empty12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "a file with no length",
                b"d5:filesld4:pathl8:film.mkveee4:name1:x12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "a file with no path",
                b"d5:filesld6:lengthi20000eee4:name1:x12:piece lengthi16384e\
                  6:pieces20:abcdefghijklmnopqrste"
                    .to_vec(),
            ),
            (
                "neither bencode nor a dictionary",
                b"not a torrent at all".to_vec(),
            ),
            (
                "a list rather than a dictionary",
                b"l4:name4:testee".to_vec(),
            ),
        ];

        for (what, description) in &bad {
            assert!(
                meta_of(description).is_err(),
                "a description with {what} should not be read"
            );
            // The same description, read out of a whole `.torrent` file, is refused too.
            assert!(
                Meta::from_torrent(&torrent_file(description)).is_err(),
                "a torrent whose description has {what} should not be read"
            );
        }
    }

    #[test]
    fn a_description_cut_short_is_refused() {
        let whole = single_file_info();
        let cut = &whole[..whole.len() / 2];
        assert!(Meta::from_info(cut, &sha1_of(cut)).is_err());

        let file = torrent_file(&whole);
        assert!(Meta::from_torrent(&file[..file.len() / 2]).is_err());
        // So is a file of nothing at all, and one whose info is not a dictionary.
        assert!(Meta::from_torrent(&[]).is_err());
        assert!(Meta::from_torrent(b"d").is_err());
        assert!(Meta::from_torrent(&torrent_file(b"i42e")).is_err());
    }

    #[test]
    fn the_add_ons_own_file_number_is_taken_first() {
        let meta = meta_of(&release_info()).expect("the description should read");

        let chosen = meta
            .select(Some(2), &[], None)
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 2);
        assert_eq!(chosen.base, 4500);
        assert_eq!(chosen.length, 20);
    }

    #[test]
    fn a_stale_file_number_falls_through_to_the_film() {
        let meta = meta_of(&release_info()).expect("the description should read");

        // Number 9 is not in a torrent of three files, but the torrent may still be playable.
        let chosen = meta
            .select(Some(9), &[], None)
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 0);
        assert_eq!(chosen.length, 3000);
    }

    #[test]
    fn the_streams_own_name_is_taken_next() {
        let meta = meta_of(&release_info()).expect("the description should read");

        let wanted = ["720p".to_owned()];
        let chosen = meta
            .select(None, &wanted, None)
            .expect("a file should be chosen");
        // The 1080p file is the larger one, but it is not the one that was asked for.
        assert_eq!(chosen.file, 1);
        assert_eq!(chosen.length, 1500);

        // A part no file has falls through rather than failing: the name may be one this
        // client reads differently, and the film is still there.
        let wanted = ["sample".to_owned()];
        let chosen = meta
            .select(None, &wanted, None)
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 0);
        assert_eq!(chosen.length, 3000);

        // The parts are read as a person reads them, not as bytes.
        let wanted = ["Subs".to_owned(), "ENG.SRT".to_owned()];
        let chosen = meta
            .select(None, &wanted, None)
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 2);
        assert_eq!(chosen.base, 4500);
    }

    #[test]
    fn the_size_the_stream_gave_is_a_file_of_that_size() {
        let meta = meta_of(&release_info()).expect("the description should read");

        let chosen = meta
            .select(None, &[], Some(20))
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 2);

        // A size no file has is not a failure either, only one fewer thing to go on.
        let chosen = meta
            .select(None, &[], Some(21))
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 0);
    }

    #[test]
    fn a_video_is_chosen_over_a_larger_file_that_is_not_one() {
        let description = b"d5:filesl\
            d6:lengthi9000e4:pathl10:archive.7zee\
            d6:lengthi3000e4:pathl9:movie.mp4ee\
            e4:name3:mix12:piece lengthi16384e\
            6:pieces20:abcdefghijklmnopqrste"
            .to_vec();
        let meta = meta_of(&description).expect("the description should read");

        let chosen = meta
            .select(None, &[], None)
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 1);
        assert_eq!(chosen.base, 9000);
        assert_eq!(chosen.length, 3000);
    }

    #[test]
    fn with_no_video_to_find_the_largest_file_is_played() {
        let description = b"d5:filesl\
            d6:lengthi5000e4:pathl11:payload.binee\
            d6:lengthi100e4:pathl10:readme.txtee\
            e4:name5:blobs12:piece lengthi16384e\
            6:pieces20:abcdefghijklmnopqrste"
            .to_vec();
        let meta = meta_of(&description).expect("the description should read");

        let chosen = meta
            .select(None, &[], None)
            .expect("a file should be chosen");
        assert_eq!(chosen.file, 0);
        assert_eq!(chosen.length, 5000);
    }

    /// A block of a file's bytes, made the same way every run so that its hash is fixed.
    fn block(index: u32, length: usize) -> Vec<u8> {
        (0..length)
            .map(|at| (at as u32 * 31 + index * 7 + 11) as u8)
            .collect()
    }

    #[test]
    fn a_torrent_whose_pieces_are_real_says_how_long_the_last_one_is() {
        // 40000 bytes cut into three pieces: two whole ones of 16384, and a last one of
        // whatever is left, hashed the way a torrent hashes its pieces.
        let blocks = [
            block(0, 16384),
            block(1, 16384),
            block(2, 40000 - 2 * 16384),
        ];
        let mut pieces = Vec::new();
        for bytes in &blocks {
            pieces.extend_from_slice(&sha1_of(bytes));
        }
        let info = [
            b"d6:lengthi40000e4:name9:movie.mkv12:piece lengthi16384e6:pieces60:".as_slice(),
            pieces.as_slice(),
            b"e".as_slice(),
        ]
        .concat();

        let meta = Meta::from_torrent(&torrent_file(&info)).expect("the torrent should read");
        assert_eq!(meta.info_hash, sha1_of(&info));
        assert_eq!(meta.piece_count(), 3);
        assert_eq!(meta.total_length, 40000);
        assert_eq!(meta.piece_size(0), 16384);
        assert_eq!(meta.piece_size(1), 16384);
        // 40000 - 2 * 16384 is what the last piece holds.
        assert_eq!(meta.piece_size(2), 7232);
        for (index, bytes) in blocks.iter().enumerate() {
            assert_eq!(meta.hashes[index], sha1_of(bytes));
        }
        let chosen = meta
            .select(None, &[], None)
            .expect("a file should be chosen");
        assert_eq!(chosen.length, 40000);
    }
}
