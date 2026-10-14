//! Keeping the folder of downloaded films within the room it has been given.
//!
//! [`storage`] puts four files in the cache folder for every torrent, named for its info
//! hash: the description, which pieces are whole, which blocks of the rest are held, and
//! the pieces themselves. Nothing else ever deleted them, so a card that had watched a
//! few films never gave any of the room back. This is the trimming that does.
//!
//! Films are let go oldest first, where "oldest" is when anything in the film was last
//! written: a film being played or downloaded is written to over and over, so the time is
//! real and not a guess. A film that is in use is never let go at all — a session claims
//! its hash while it is open (see [`claim`]), and nothing here deletes a claimed hash, so a
//! trim that happens to run while a film is playing cannot pull the card out from under
//! the player.
//!
//! A file that is not one of the four, named for forty hexadecimal characters, is left
//! alone entirely: it takes up room (so it is counted, since it is on the card) but it is
//! not ours to delete. The folder belongs to the app as much as to this module, and
//! whatever else is put in it should not have to know about torrents.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use once_cell::sync::Lazy;

/// What a file in the cache folder is called after the info hash: the description, which
/// pieces are whole, which blocks of the rest are held, and the pieces themselves.
const SUFFIXES: [&str; 4] = [".torrent", ".have", ".blocks", ".data"];

/// The time given to a film whose files' times cannot be read, which is before any file
/// that has one: an unreadable film is the one to let go first.
const UNKNOWN: SystemTime = SystemTime::UNIX_EPOCH;

/// The torrents that are open, and so must not be deleted: their info hashes, added by
/// [`claim`] when a session opens and taken away by [`release`] when it closes.
///
/// Every film this module lets go goes through here first. It is a [`Mutex`] and not a
/// lock-free set because it is touched a handful of times in the life of a film, and a
/// poisoned lock must not stop a trim: an entry that cannot be added or taken away is
/// logged, and is treated as not being there.
static LIVE: Lazy<Mutex<HashSet<[u8; 20]>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// One film in the folder: the four files of one torrent, named for its info hash.
pub struct Film {
    /// The torrent's own hash, which is what every one of its files is named for, and what
    /// [`claim`] registers it under.
    pub info_hash: [u8; 20],
    /// The files of the film that are there. Not all four of them have to be: a
    /// `.torrent` is only written when there was a description to keep, and an index file
    /// can be lost.
    pub paths: Vec<PathBuf>,
    /// How much room the film takes on the card: the length of every one of its files.
    pub bytes: u64,
    /// When anything in the film was last written: the newest of its files' times, which
    /// is when it was last played or downloaded. [`UNKNOWN`] when none of them can be
    /// read, so that the film sorts first and is let go first.
    pub used: SystemTime,
}

/// Says that a torrent is in use and must not be deleted, which is what a session does for
/// as long as it has the film open. Being claimed twice is one claim, so two sessions on
/// the same torrent are both safe as long as neither has released it.
pub fn claim(info_hash: &[u8; 20]) {
    match LIVE.lock() {
        Ok(mut live) => {
            live.insert(*info_hash);
        }
        Err(_) => crate::kernel_log("torrent: a film could not be marked as in use"),
    }
}

/// Says that a torrent is no longer in use, and may be deleted once the room is wanted
/// again. Releasing a hash that is not claimed is not a failure: [`Drop`](std::ops::Drop)
/// may close a session that has already been closed, and nothing should be said twice.
pub fn release(info_hash: &[u8; 20]) {
    match LIVE.lock() {
        Ok(mut live) => {
            live.remove(info_hash);
        }
        Err(_) => crate::kernel_log("torrent: a film could not be marked as no longer in use"),
    }
}

/// Frees what can be freed until the folder holds no more than `limit` bytes, letting films
/// go oldest first and never touching one that is in use. Returns the bytes that freed.
///
/// A limit of 0 is no limit, and nothing is deleted however much there is — not even read,
/// so a folder that is not there costs nothing. Deleting stops as soon as the folder fits,
/// so a limit larger than what is there leaves every file alone.
pub fn trim(folder: &Path, limit: u64) -> u64 {
    if limit == 0 {
        return 0;
    }
    let mut freed: u64 = 0;
    for film in films(folder) {
        // The folder is measured again for each film rather than kept from the first one,
        // so that what is left is what is really there: a file that could not be deleted
        // goes on being counted, and deleting stops as soon as the folder fits.
        if held(folder) <= limit {
            break;
        }
        // A film that is in use is stepped over rather than stopped at: the next film is
        // older than this limit needs, and can go instead. When everything left is in use
        // the folder goes on holding more than it was asked to, which is better than
        // deleting the film the player is reading.
        if is_claimed(&film.info_hash) {
            continue;
        }
        freed = freed.saturating_add(drop(&film));
    }
    freed
}

/// How much room the folder takes on the card: every file in it, counted once, including
/// the ones this module would not delete. A folder that is not there, or cannot be read,
/// holds nothing.
pub fn held(folder: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(folder) else { return 0 };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        // A folder of its own takes up nothing of its own, only what is in it, and there
        // are none: every torrent writes its files straight into this folder.
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// What is in the folder, oldest film first: each torrent's files as one film, with its
/// size and when it was last written. A folder that is not there, or cannot be read, holds
/// nothing.
pub fn films(folder: &Path) -> Vec<Film> {
    let Ok(entries) = fs::read_dir(folder) else { return Vec::new() };
    // The four files of a film are found by the hash they share, so the folder is read once
    // and each hash is one film however many of its files are there.
    let mut found: Vec<Film> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(info_hash) = hash_of_name(&name) else { continue };
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(error) => {
                let path = entry.path();
                crate::kernel_log(&format!("torrent: {} could not be looked at: {error}", path.display()));
                continue;
            }
        };
        // The path is made from the name rather than read back off the entry, so that a
        // film is made of the files it is known by whatever order the folder answered in.
        let path = folder.join(&name);
        match found.iter_mut().find(|film| film.info_hash == info_hash) {
            Some(film) => add(film, &path, meta),
            None => {
                let mut film = Film {
                    info_hash,
                    paths: Vec::new(),
                    bytes: 0,
                    used: UNKNOWN,
                };
                add(&mut film, &path, meta);
                found.push(film);
            }
        }
    }
    // Oldest first. A film whose times could not be read is at `UNIX_EPOCH`, which is
    // before every file that has one, and so comes first.
    found.sort_by_key(|film| film.used);
    found
}

/// Whether the torrent is in use, and so must not be deleted. A lock that cannot be taken
/// says yes: a claim that cannot be read is one that cannot be trusted, and a film that
/// might be playing is left alone.
fn is_claimed(info_hash: &[u8; 20]) -> bool {
    LIVE.lock().map(|live| live.contains(info_hash)).unwrap_or(true)
}

/// The info hash a file in the cache folder is named for, or None when the name is not
/// one of ours. The name has to be forty hexadecimal characters, a dot, and then one of
/// the four suffixes and nothing else, so a file of somebody else's — a `.txt`, a name too
/// short, one that is not hexadecimal — is left alone. The four suffixes are of different
/// lengths, so what follows the dot is what says which file it is.
fn hash_of_name(name: &str) -> Option<[u8; 20]> {
    if name.len() <= 40 {
        return None;
    }
    let (hash, tail) = name.split_at(40);
    if !SUFFIXES.contains(&tail) {
        return None;
    }
    bytes_of(hash)
}

/// The twenty bytes forty hexadecimal characters are, or None when one of them is not a
/// hexadecimal digit — which is what tells our files from a `.txt` that happens to be
/// forty characters long. Both cases are read, since a name may have been written by a
/// different build of the app.
fn bytes_of(hex: &str) -> Option<[u8; 20]> {
    fn digit(one: u8) -> Option<u8> {
        match one {
            b'0'..=b'9' => Some(one - b'0'),
            b'a'..=b'f' => Some(one - b'a' + 10),
            b'A'..=b'F' => Some(one - b'A' + 10),
            _ => None,
        }
    }
    let mut hash = [0u8; 20];
    for (at, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        hash[at] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(hash)
}

/// Adds one file of a film: its length to the film's size, its path to the files the film
/// is made of, and its time to the film's, which is the newest of them.
fn add(film: &mut Film, path: &Path, meta: fs::Metadata) {
    film.paths.push(path.to_path_buf());
    film.bytes = film.bytes.saturating_add(meta.len());
    if let Ok(modified) = meta.modified() {
        film.used = film.used.max(modified);
    }
}

/// Deletes every file of a film that is still there, and says how much room that freed. A
/// file that has gone missing on its own is not a failure — nothing was lost that was not
/// already gone — and a file that cannot be deleted is logged and left, rather than
/// stopping the films behind it.
fn drop(film: &Film) -> u64 {
    let mut freed: u64 = 0;
    for path in &film.paths {
        // The length is asked for before the file goes: what it held is what it took of
        // the card, and a file that has gone in the meantime freed nothing this time.
        let taken = size_of(path);
        match fs::remove_file(path) {
            Ok(()) => freed = freed.saturating_add(taken),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => crate::kernel_log(&format!("torrent: {} was not deleted: {error}", path.display())),
        }
    }
    freed
}

/// How much room a file takes, which is 0 for one that cannot be read: it has gone, or
/// never could be, and either way freed nothing.
fn size_of(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::super::magnet::info_hash_to_hex;
    use super::*;

    /// A folder of this test's own under the system's temporary one: the tests run at the
    /// same time, and two runs of the same test must not be left looking at each other's
    /// files.
    fn folder(name: &str) -> PathBuf {
        static RUN: AtomicU32 = AtomicU32::new(0);
        let number = RUN.fetch_add(1, Ordering::Relaxed);
        let unique = std::env::temp_dir().join(format!(
            "stremio-torrent-trim-{}-{name}-{number}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&unique);
        fs::create_dir_all(&unique).expect("this test's own folder");
        unique
    }

    /// Writes the four files of one film: a few bytes of description, one byte of each
    /// index, and `data` bytes of pieces. Says the film was last written `age` ago, so
    /// that the films of a test can be put in an order. Returns the hash and the room the
    /// film takes.
    ///
    /// The hash is the name of this test's own folder with the number in its last byte, so
    /// that the films of two tests are two different films: the register of what is in use
    /// is one for the whole run, not one per test.
    fn write_film(folder: &Path, number: u8, data: u64, age: Duration) -> ([u8; 20], u64) {
        let own = folder.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut info_hash = [0u8; 20];
        // The end of this test's own folder name, which is where its name is, and the film
        // number in the last byte. Two tests therefore have two different films however
        // many they number, which matters because the register of what is in use is one for
        // the whole run and not one per test.
        let start = own.len().saturating_sub(19);
        for (at, byte) in own.as_bytes()[start..].iter().enumerate() {
            info_hash[at] = *byte;
        }
        info_hash[19] = number;
        let name = info_hash_to_hex(&info_hash);
        let lengths = [4, 1, 1, data];
        let when = SystemTime::now() - age;
        for (suffix, length) in SUFFIXES.iter().zip(lengths) {
            let path = folder.join(format!("{name}{suffix}"));
            let mut file = File::create(&path).expect("a file of the film");
            file.write_all(&vec![0u8; length as usize]).expect("the film's own bytes");
            file.set_modified(when).expect("the time the file was written");
        }
        (info_hash, lengths.iter().sum())
    }

    /// Whether every one of a film's four files is still in the folder.
    fn there(folder: &Path, info_hash: &[u8; 20]) -> bool {
        let name = info_hash_to_hex(info_hash);
        SUFFIXES.iter().all(|suffix| folder.join(format!("{name}{suffix}")).is_file())
    }

    #[test]
    fn a_folder_under_the_limit_is_left_alone() {
        let folder = folder("under");
        let (first, first_bytes) = write_film(&folder, 1, 100, Duration::from_secs(60));
        let (second, second_bytes) = write_film(&folder, 2, 100, Duration::from_secs(30));

        assert_eq!(trim(&folder, first_bytes + second_bytes), 0);
        assert!(there(&folder, &first));
        assert!(there(&folder, &second));
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn the_oldest_film_goes_when_the_folder_is_over_the_limit() {
        let folder = folder("over");
        let (oldest, oldest_bytes) = write_film(&folder, 1, 100, Duration::from_secs(90));
        let (middle, middle_bytes) = write_film(&folder, 2, 200, Duration::from_secs(60));
        let (newest, newest_bytes) = write_film(&folder, 3, 300, Duration::from_secs(30));
        // Enough room for the two newer films and no more, so only the oldest can go.
        let limit = middle_bytes + newest_bytes;

        assert_eq!(trim(&folder, limit), oldest_bytes);
        assert!(!there(&folder, &oldest));
        assert!(there(&folder, &middle));
        assert!(there(&folder, &newest));
        assert_eq!(held(&folder), limit);
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn only_enough_films_go_to_fit_the_limit() {
        let folder = folder("enough");
        let (oldest, oldest_bytes) = write_film(&folder, 1, 100, Duration::from_secs(90));
        let (middle, middle_bytes) = write_film(&folder, 2, 200, Duration::from_secs(60));
        let (newest, newest_bytes) = write_film(&folder, 3, 300, Duration::from_secs(30));

        assert_eq!(trim(&folder, newest_bytes), oldest_bytes + middle_bytes);
        assert!(!there(&folder, &oldest));
        assert!(!there(&folder, &middle));
        assert!(there(&folder, &newest));
        // Nothing more is wanted now, so the second trim finds nothing to do.
        assert_eq!(trim(&folder, newest_bytes), 0);
        assert_eq!(held(&folder), newest_bytes);
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn a_limit_of_no_limit_frees_nothing() {
        let folder = folder("nolimit");
        let (oldest, _) = write_film(&folder, 1, 100, Duration::from_secs(90));
        write_film(&folder, 2, 100, Duration::from_secs(30));

        assert_eq!(trim(&folder, 0), 0);
        assert!(held(&folder) > 0);
        assert!(there(&folder, &oldest));
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn a_film_in_use_is_never_deleted() {
        let folder = folder("claimed");
        let (playing, playing_bytes) = write_film(&folder, 1, 100, Duration::from_secs(90));
        let (idle, idle_bytes) = write_film(&folder, 2, 100, Duration::from_secs(30));
        claim(&playing);

        // Far over the limit, so the film in use is the only thing between the folder and
        // the room it was asked for: it is stepped over, and the newer film goes instead.
        assert_eq!(trim(&folder, 1), idle_bytes);
        assert!(there(&folder, &playing));
        assert!(!there(&folder, &idle));
        assert!(held(&folder) > 1);

        // Once it is no longer in use, the next trim lets it go.
        release(&playing);
        assert_eq!(trim(&folder, 1), playing_bytes);
        assert!(!there(&folder, &playing));
        assert_eq!(held(&folder), 0);
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn a_file_that_is_not_a_film_is_kept_and_counted() {
        let folder = folder("others");
        let (film, film_bytes) = write_film(&folder, 1, 100, Duration::from_secs(60));
        // A name too short, one of the right length whose head is not hexadecimal, and one
        // that is no torrent's file at all.
        let not_hex = &info_hash_to_hex(&[2; 20])[..39];
        let others = ["short.data".to_owned(), format!("{not_hex}z.data"), "notes.txt".to_owned()];
        for name in &others {
            fs::write(folder.join(name), vec![0u8; 7]).expect("a file that is not a film");
        }

        // The middle one is exactly as long as one of ours and is still not one, which is
        // what the hexadecimal head is for.
        assert_eq!(others[1].len(), 45);

        // Asked for almost no room, the film goes and the three others do not: they are
        // on the card, and they are not ours to delete.
        assert_eq!(trim(&folder, 1), film_bytes);
        assert!(!there(&folder, &film));
        for name in &others {
            assert!(folder.join(name).is_file(), "{name} was deleted");
        }
        assert_eq!(held(&folder), others.len() as u64 * 7);
        assert!(films(&folder).is_empty());
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn a_folder_that_is_not_there_is_no_error() {
        let folder = folder("absent");
        fs::remove_dir_all(&folder).ok();

        assert_eq!(held(&folder), 0);
        assert_eq!(trim(&folder, 1), 0);
        assert!(films(&folder).is_empty());
        assert_eq!(trim(&folder, 0), 0);
    }

    #[test]
    fn a_film_with_a_file_missing_is_still_deleted() {
        let folder = folder("missing");
        let (film, film_bytes) = write_film(&folder, 1, 100, Duration::from_secs(60));
        let (_, other_bytes) = write_film(&folder, 2, 200, Duration::from_secs(30));
        // An index file that has gone on its own is not a failure to let the film go.
        let lost = folder.join(format!("{}.have", info_hash_to_hex(&film)));
        fs::remove_file(&lost).expect("the file to go missing");

        assert_eq!(trim(&folder, 1), film_bytes - 1 + other_bytes);
        assert!(!there(&folder, &film));
        assert_eq!(held(&folder), 0);
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn held_counts_every_file_in_the_folder() {
        let folder = folder("held");
        let (_, data_bytes) = write_film(&folder, 1, 100, Duration::from_secs(60));
        fs::write(folder.join("notes.txt"), vec![0u8; 5]).expect("a file that is not a film");

        // Four files of the film and one of its own: the pieces alone are not the answer.
        assert_eq!(held(&folder), data_bytes + 5);
        let films = films(&folder);
        assert_eq!(films.len(), 1);
        assert_eq!(films[0].bytes, data_bytes);
        fs::remove_dir_all(&folder).ok();
    }

    #[test]
    fn films_are_oldest_first() {
        let folder = folder("order");
        write_film(&folder, 1, 10, Duration::from_secs(120));
        write_film(&folder, 2, 20, Duration::from_secs(10));
        write_film(&folder, 3, 30, Duration::from_secs(60));

        let order: Vec<u8> = films(&folder).iter().map(|film| film.info_hash[19]).collect();
        assert_eq!(order, vec![1, 3, 2]);
        fs::remove_dir_all(&folder).ok();
    }
}