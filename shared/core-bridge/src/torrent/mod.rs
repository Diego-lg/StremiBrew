//! Torrent playback: a small BitTorrent client that downloads a torrent and reads it like
//! a file, so the player can start on the first pieces and carry on as the rest arrives.
//!
//! It is built the way the player needs it, which is not the way a torrent client usually
//! works. There is one reader (the video player), it reads forward at the pace a film
//! plays at, and it seeks about when the user scrubs. What is wanted is therefore the
//! fewest pieces around the reading position fetched first, the rest of the file fetched
//! in order behind it, and every byte kept so that a seek back is instant.
//!
//! The pieces are held in one sparse file per torrent ([`storage`]), so what has been
//! played is still there tomorrow. Nothing here seeds, and a torrent that is private
//! (every peer one account) is never asked for its pieces.
//!
//! What is here:
//!
//! | Module | What it does |
//! |---|---|
//! | [`bencode`] | Reading and writing bencode: torrent files, tracker answers, extended handshakes |
//! | [`magnet`] | What a `magnet:` link says, and how to build one |
//! | [`metadata`] | The torrent's own description: its pieces, its files, which one to play |
//! | [`wire`] | What peers say to each other (BEP 3, 7, 9, 10 and 11) |
//! | [`tracker`] | Asking trackers for peers (HTTP and UDP, BEP 3 and 15) |
//! | [`storage`] | The pieces on disk, checked against the torrent's hashes |
//! | [`session`] | The client itself: peers, what is asked of them, and the reader |
//! | [`trim`] | Letting films go, oldest first, so the folder stays within its room |
//!
//! No DHT: a magnet link that arrives with no trackers cannot find peers here.

pub mod bencode;
pub mod magnet;
pub mod metadata;
pub mod session;
pub mod storage;
pub mod tracker;
pub mod trim;
pub mod wire;

use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use session::Session;

/// How long opening a torrent waits for its description: long enough for a thin tracker
/// and a slow swarm to answer, short enough that the player gives up rather than waits
/// for ever on a torrent that has none.
const OPEN_WAIT: Duration = Duration::from_secs(60);

/// How much room downloaded films may take until the app says otherwise: 8 GiB, which is
/// more than a card is and little enough that a fresh install is bounded before anything
/// has been told to it.
const DEFAULT_LIMIT: u64 = 8 << 30;

/// Where torrent data is kept between runs. The app sets it once, at start-up, and before
/// the limit: what has been played stays on the card until [`stremio_torrent_trim`] or a
/// smaller [`stremio_torrent_set_limit`] says otherwise.
static CACHE: OnceLock<PathBuf> = OnceLock::new();

/// How much room downloaded films may take, in bytes; 0 for no limit. The app's own
/// setting overwrites this at start-up, and again whenever that setting changes.
static LIMIT: AtomicU64 = AtomicU64::new(DEFAULT_LIMIT);

/// Set by [`stremio_torrent_cancel`] when the player has given up on whatever is being
/// opened, so that a wait for peers ends at once rather than running its course.
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Whether the app that links this bridge has a player that can read a torrent.
///
/// The Switch's has ([`stremio_torrent_open`], through the AVIO context in its player).
/// The PS5's has not been given one yet, so its details list still marks a torrent as
/// something it cannot play rather than offering one and then failing.
pub const PLAYABLE: bool = cfg!(target_os = "horizon");

/// Why the last open or read failed, for the app to show or log.
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

/// The app's C interface (below) is the same shape as the one for HTTP streams
/// (`stream_io.rs`), so the player reads both through the same code.
pub struct TorrentStream {
    session: Arc<Session>,
    /// Where the next read starts.
    position: u64,
}

/// A line for the console's log, and the record of what went wrong.
pub(crate) fn note_failure(error: &str) {
    if let Ok(mut last) = LAST_ERROR.lock() {
        *last = error.to_owned();
    }
    crate::kernel_log(&format!("torrent: {error}"));
}

/// The record is cleared once a torrent opens, so that a reason left by an earlier video
/// is never offered as the reason this one failed.
pub(crate) fn note_success() {
    if let Ok(mut last) = LAST_ERROR.lock() {
        last.clear();
    }
}

fn c_str<'a>(value: *const c_char) -> Option<&'a str> {
    if value.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(value) }.to_str().ok()
}

/// Sets where torrent data is kept. Called once, before any torrent is opened, and before
/// the limit so that lowering the limit at start-up has a folder to empty.
#[no_mangle]
pub extern "C" fn stremio_torrent_set_cache(path: *const c_char) {
    let Some(path) = c_str(path) else { return };
    let _ = CACHE.set(PathBuf::from(path));
}

/// Sets how much room downloaded films may take, in bytes; 0 means no limit. Asking for
/// less than the folder already holds deletes the oldest films until it fits. The app
/// calls this once at start-up with what its own setting says, and whenever that setting
/// changes.
#[no_mangle]
pub extern "C" fn stremio_torrent_set_limit(bytes: u64) {
    let was = LIMIT.swap(bytes, Ordering::SeqCst);
    // Only a smaller limit has anything to do: a room that has been made bigger is not
    // filled by asking for it, and emptying the folder would only throw films away.
    if bytes < was {
        if let Some(folder) = CACHE.get() {
            let freed = trim::trim(folder, bytes);
            if freed > 0 {
                crate::kernel_log(&format!("torrent: {freed} bytes freed for a smaller room"));
            }
        }
    }
}

/// How much room downloaded films may take, in bytes; 0 for no limit. The app's setting
/// is filled in with this at start-up.
#[no_mangle]
pub extern "C" fn stremio_torrent_limit() -> u64 {
    LIMIT.load(Ordering::SeqCst)
}

/// Frees what can be freed, oldest first, and says how many bytes that was. A film being
/// played is never touched.
#[no_mangle]
pub extern "C" fn stremio_torrent_trim() -> u64 {
    // With no folder set there is nothing to trim, and the current directory is not a
    // thing to go through deleting files in.
    let Some(folder) = CACHE.get() else { return 0 };
    trim::trim(folder, LIMIT.load(Ordering::SeqCst))
}

/// How much room downloaded films may take right now, in bytes; 0 for no limit. What
/// [`Session::close`] trims the folder to when a film is let go.
pub fn limit_now() -> u64 {
    LIMIT.load(Ordering::SeqCst)
}

/// Opens a torrent: a `magnet:` link, or an address a `.torrent` file can be fetched from.
/// Waits for the torrent's own description to arrive (from the link's trackers and peers,
/// or from the file at the address) and for the file to be chosen, so the caller has a
/// size to work with. Null when it could not be opened, with `stremio_torrent_last_error`
/// saying why.
#[no_mangle]
pub extern "C" fn stremio_torrent_open(source: *const c_char) -> *mut TorrentStream {
    let Some(source) = c_str(source) else {
        note_failure("no torrent was given");
        return std::ptr::null_mut();
    };
    let Some(folder) = CACHE.get().cloned() else {
        note_failure("no folder was set for torrent data");
        return std::ptr::null_mut();
    };
    let session = match Session::open(source, &folder) {
        Ok(session) => Arc::new(session),
        Err(error) => {
            note_failure(&error);
            return std::ptr::null_mut();
        }
    };
    // The player asks for the file's size before it reads a byte of it (FFmpeg cannot work
    // out what a file is without one), so the description is waited for here rather than
    // left to the first read. The wait is in short goes, so that giving up (the player says
    // so with `stremio_torrent_cancel`) and the wait running out are both noticed at once,
    // rather than only at the end of it.
    CANCELLED.store(false, Ordering::SeqCst);
    let until = Instant::now() + OPEN_WAIT;
    loop {
        match session.wait_ready(Duration::from_millis(250)) {
            Ok(()) => break,
            Err(error) => {
                let given_up = CANCELLED.swap(false, Ordering::SeqCst);
                if session.failed() {
                    note_failure(&error);
                    return std::ptr::null_mut();
                }
                if given_up {
                    note_failure("the torrent was given up on");
                    return std::ptr::null_mut();
                }
                if Instant::now() >= until {
                    note_failure(&error);
                    return std::ptr::null_mut();
                }
            }
        }
    }
    let stream = Box::new(TorrentStream { session, position: 0 });
    note_success();
    Box::into_raw(stream)
}

/// Ends the wait [`stremio_torrent_open`] is in, for a torrent that has not yet found its
/// peers. The player calls this when the user backs out of the stream.
#[no_mangle]
pub extern "C" fn stremio_torrent_cancel() {
    CANCELLED.store(true, Ordering::SeqCst);
}

/// Reads up to `length` bytes at the reading position, waiting for the torrent to have
/// them. Returns how many were read, 0 at the end of the file, or -1 when the wait ran
/// out or the download failed (`stremio_torrent_last_error` then says why).
#[no_mangle]
pub extern "C" fn stremio_torrent_read(stream: *mut TorrentStream, buffer: *mut u8, length: usize) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let stream = unsafe { &mut *stream };
    let buffer = unsafe { std::slice::from_raw_parts_mut(buffer, length) };
    match stream.session.read(stream.position, buffer) {
        Ok((count, next)) => {
            stream.position = next;
            count as i64
        }
        Err(error) => {
            note_failure(&error);
            -1
        }
    }
}

/// Sets where the next read starts. The pieces around there are asked for first, which is
/// how a seek in the video moves the download.
#[no_mangle]
pub extern "C" fn stremio_torrent_seek(stream: *mut TorrentStream, position: u64) {
    if let Some(stream) = unsafe { stream.as_mut() } {
        stream.position = position;
        stream.session.seek(position);
    }
}

/// The chosen file's size in bytes, or -1 when it is not known yet.
#[no_mangle]
pub extern "C" fn stremio_torrent_size(stream: *const TorrentStream) -> i64 {
    unsafe { stream.as_ref() }
        .map(|stream| match stream.session.size() {
            0 => -1,
            size => size as i64,
        })
        .unwrap_or(-1)
}

/// How much of the file is held: bytes so far and the file's size. Returns 0 when the
/// torrent is downloading, 1 while it is still looking for peers, or -1 when it has
/// failed.
#[no_mangle]
pub extern "C" fn stremio_torrent_progress(stream: *const TorrentStream, done: *mut u64, total: *mut u64) -> i32 {
    let Some(stream) = (unsafe { stream.as_ref() }) else {
        return -1;
    };
    let progress = stream.session.progress();
    if !done.is_null() {
        unsafe { done.write_volatile(progress.done) };
    }
    if !total.is_null() {
        unsafe { total.write_volatile(progress.total) };
    }
    progress.state.code()
}

/// Closes the torrent: the download stops and what has been fetched is kept.
#[no_mangle]
pub extern "C" fn stremio_torrent_close(stream: *mut TorrentStream) {
    if !stream.is_null() {
        drop(unsafe { Box::from_raw(stream) });
    }
}

/// Copies the last error to `out`, NUL-terminated. Returns its length, so a result of
/// `capacity` or more means the buffer was too small and nothing was copied.
#[no_mangle]
pub extern "C" fn stremio_torrent_last_error(out: *mut c_char, capacity: usize) -> usize {
    let Ok(last) = LAST_ERROR.lock() else { return 0 };
    if !out.is_null() && last.len() < capacity {
        unsafe {
            std::ptr::copy_nonoverlapping(last.as_ptr(), out as *mut u8, last.len());
            *out.add(last.len()) = 0;
        }
    }
    last.len()
}