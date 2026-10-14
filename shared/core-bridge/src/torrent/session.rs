//! The client: the peers, what is asked of them, and the reader the player reads through.
//!
//! One session is one torrent being played. It runs:
//!
//! - a thread that asks the trackers again as often as they say, connects to the peers
//!   they name, drops the ones that go quiet, decides what each peer is asked for, and
//!   writes what arrives into [`Storage`](super::storage);
//! - one thread per connected peer, which greets, answers with its own greeting, and
//!   passes on what the peer says;
//! - the reader, which is the thread the player reads on: it waits for the pieces its
//!   position needs, and by moving that position is what tells the other threads which
//!   pieces matter most.
//!
//! What a player wants from a torrent is not a complete download: it is the pieces around
//! where it is reading, in order, soon. So the queue of what to ask for is rebuilt around
//! the reader every tick, [`Settings::read_ahead`] pieces ahead of it, and the rest of the
//! file is fetched behind that, as the reader gets there.
//!
//! Nothing is uploaded: this client downloads and does not seed, so no socket is ever
//! opened to answer one. A torrent marked private (BEP 27) is not asked for its pieces at
//! all, its hashes being a secret between peers of one account.
//!
//! No DHT (BEP 5): a magnet link that arrives with no tracker, no web seed and no cached
//! description of its own cannot find peers here.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use super::bencode::{self, Node, Value};
use super::magnet::{self, Magnet};
use super::metadata::{Meta, Selection};
use super::storage::Storage;
use super::tracker;
use super::trim;
use super::wire::{self, ExtendedHandshake, Handshake, Message, MetadataMessage};

/// Unlike the agent for the core's own requests, this one puts no limit on how long an
/// answer may take as a whole: a tracker can be slow, and the peers already connected keep
/// sending while it is asked.
static HTTP: Lazy<ureq::Agent> = Lazy::new(|| {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent("stremio-torrent")
        .build()
});

/// What we say we are, in the extended handshake and to a tracker.
const CLIENT: &str = "stremio-torrent/1.0";

/// The largest a torrent's description is taken to be. It grows with the number of pieces,
/// so this is generous rather than tight.
const MAX_DESCRIPTION: usize = 16 << 20;

/// How long the coordinator sleeps when nothing has happened. Short enough that a block
/// arriving is noticed at once, long enough not to spin.
const TICK: Duration = Duration::from_millis(40);

/// How long a peer may be quiet before it is dropped.
const PEER_TIMEOUT: Duration = Duration::from_secs(120);

/// How long between keep-alives to a peer.
const KEEPALIVE: Duration = Duration::from_secs(100);

/// How often the index files are written while the download runs.
const SAVE_EVERY: Duration = Duration::from_secs(15);

/// When no tracker has said anything and nothing has arrived, the download is over.
const GIVE_UP: Duration = Duration::from_secs(180);

/// How many blocks may be outstanding with one peer before it is left to answer the rest.
const PEER_WINDOW: u32 = 32;

/// The most a peer's bitfield is allowed to be long, in bytes: a bit for each of a million
/// pieces, which is far beyond any film and keeps a peer's claim on memory bounded.
const BITFIELD_LIMIT: usize = 1024 * 1024 / 8;

/// How long a block asked of a peer may go unanswered before it is asked of someone else.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to wait between asking a peer for the parts of the torrent's description that
/// have not arrived. Asking again sooner is only a way to be dropped.
const METADATA_ASK_EVERY: Duration = Duration::from_secs(3);

/// How long a peer has to answer a greeting before it is left. It is waited for twice over
/// at most: a peer that sends a part of one and then nothing is not worth a slot for long.
const HANDSHAKE_WAIT: u64 = 8;

/// The extension ids this client uses, as they are sent in the extended handshake and come
/// back in a peer's own.
const UT_METADATA: u8 = 1;
const UT_PEX: u8 = 2;

/// What the download is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No peers yet: trackers are being asked, and nothing can be read.
    Looking,
    /// The torrent is known and its file can be read.
    Downloading,
    /// Nothing will come: no tracker answered, or no peer had any of the file.
    Failed,
}

impl Phase {
    /// What the app is told: 0 downloading, 1 looking, -1 failed.
    pub fn code(self) -> i32 {
        match self {
            Phase::Downloading => 0,
            Phase::Looking => 1,
            Phase::Failed => -1,
        }
    }

    /// A word for the log.
    pub fn text(self) -> &'static str {
        match self {
            Phase::Downloading => "downloading",
            Phase::Looking => "looking for peers",
            Phase::Failed => "failed",
        }
    }
}

/// How far the download has got, for the player and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Bytes of the file that are held. While the description is still being found, 0.
    pub done: u64,
    /// The file's size.
    pub total: u64,
    pub state: Phase,
    /// Peers connected now.
    pub peers: usize,
    /// Peers that hold all of the torrent, so nothing is left to fetch from them.
    pub seeders: usize,
}

/// What a session is allowed to do, so the same client can be pointed at differently in a
/// test than in the app.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// How many peers to keep connected at once. A video is read through a few, and the
    /// console has four cores and one network stack to share between them.
    pub peers: usize,
    /// The port peers are told to reach us on. Nothing listens: this client downloads and
    /// does not seed, and a tracker only uses the number to decide what to tell us.
    pub port: u16,
    /// How long a read waits for the pieces it needs before giving up, so a download that
    /// has stopped reads as an error rather than hanging the player.
    pub read_timeout: Duration,
    /// How long a connection to a peer may take.
    pub connect_timeout: Duration,
    /// How long a tracker may take.
    pub announce_timeout: Duration,
    /// How many pieces around the reader to ask for. Enough to keep a video playing
    /// through a slow moment, few enough that what is fetched first is what is watched
    /// next.
    pub read_ahead: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            peers: 10,
            // A port in the dynamic range, which is what a client that does not listen
            // should say: it never asks for an incoming connection.
            port: 50_000,
            read_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            announce_timeout: Duration::from_secs(15),
            read_ahead: 24,
        }
    }
}

/// What a peer said, or that it has gone.
enum Event {
    /// The peer has greeted and had its greeting answered: it can be asked for things.
    Ready { id: u32 },
    /// The pieces the peer's bitfield named.
    Bitfield { id: u32, bits: Vec<u8> },
    Have { id: u32, piece: u32 },
    Unchoke { id: u32 },
    Choke { id: u32 },
    /// The peer's extended handshake: what it has, and how large this torrent's
    /// description is if it has it.
    Extended { id: u32, handshake: ExtendedHandshake },
    /// Part of the torrent's description (BEP 9).
    Metadata { id: u32, piece: u32, total: Option<u64>, data: Vec<u8> },
    /// Peers a peer already knows (BEP 11).
    Peers { peers: Vec<SocketAddr> },
    /// A block of a piece.
    Piece { id: u32, piece: u32, begin: u32, data: Vec<u8> },
    Gone { id: u32 },
}

/// A connected peer, as the coordinator sees it.
struct Peer {
    id: u32,
    addr: SocketAddr,
    /// What the peer thread is told to write. The socket is the peer thread's own, and
    /// this is how a request, a message or a goodbye reaches it.
    commands: Sender<Command>,
    unchoking: bool,
    /// What the peer says it holds, one bit per piece.
    have: Vec<u8>,
    interested: bool,
    /// How many blocks have been asked of this peer and not answered.
    outstanding: u32,
    /// The peer's extended handshake, once it has sent one.
    extended: Option<ExtendedHandshake>,
    /// When this peer was last asked for the parts of the torrent's description, so that
    /// it is asked again no faster than a swarm expects.
    metadata_asked: Instant,
    last_heard: Instant,
    last_sent: Instant,
}

impl Peer {
    fn holds(&self, piece: u32) -> bool {
        wire::bitfield_has(&self.have, piece)
    }

    /// Writes a message to the peer. False when the peer thread has gone, which is the same
    /// as the peer having gone.
    fn send(&self, message: &Message) -> bool {
        self.commands.send(Command::Say(message.clone())).is_ok()
    }

    /// Tells the peer thread to let the connection go.
    fn shut(&self) {
        let _ = self.commands.send(Command::LetGo);
    }
}

/// A description being put together out of the parts peers send.
struct Description {
    /// How large the whole description is.
    size: u64,
    parts: Vec<Option<Vec<u8>>>,
}

impl Description {
    fn new(size: u64) -> Self {
        let count = size.div_ceil(wire::METADATA_CHUNK as u64);
        Self {
            size,
            parts: vec![None; count as usize],
        }
    }

    /// The description, once every part of it is here.
    fn joined(&self) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(self.size as usize);
        for part in &self.parts {
            bytes.extend_from_slice(part.as_ref()?);
        }
        bytes.truncate(self.size as usize);
        Some(bytes)
    }

    /// How long one part of it should be, which is what a peer is held to: the chunk size,
    /// or whatever is left of the description in the last one.
    fn part_len(&self, part: usize) -> usize {
        let start = (part * wire::METADATA_CHUNK) as u64;
        let left = self.size.saturating_sub(start);
        left.min(wire::METADATA_CHUNK as u64) as usize
    }

    /// Whether a part is one this description is expecting, and whether it is right: a
    /// peer that answers with more than the part asked for is not believed, because the
    /// memory is bounded by what the description itself says it needs.
    fn accepts(&self, part: usize, data: &[u8]) -> bool {
        self.part_len(part) == data.len()
    }
}

/// One torrent's session, all of it under one lock.
struct State {
    magnet: Magnet,
    folder: PathBuf,
    settings: Settings,
    peer_id: [u8; 20],
    meta: Option<Meta>,
    selection: Option<Selection>,
    storage: Option<Storage>,
    peers: Vec<Peer>,
    /// Addresses the trackers have named that are not connected or refused yet.
    wanted: Vec<SocketAddr>,
    /// Addresses already tried, so a tracker naming one again is not news.
    tried: HashSet<SocketAddr>,
    /// What to ask for next, most wanted first.
    queue: VecDeque<(u32, u32)>,
    /// The blocks asked of a peer and not yet answered, with when they were asked: a peer
    /// that takes a request and says nothing about it must not keep the block to itself.
    in_flight: HashMap<(u32, u32), (u32, Instant)>,
    /// Where the reader is, within the file being played.
    read_position: u64,
    /// The description being collected, before the torrent's own is known.
    description: Option<Description>,
    /// A description already in hand: from the folder, a web seed or the address itself.
    given: Option<Vec<u8>>,
    /// Which of the web seeds have been tried, so that a dead one is stepped over rather
    /// than asked again on every turn.
    seeds: std::collections::HashSet<String>,
    /// Each tracker with when it was last asked and when it says to ask again.
    trackers: Vec<Tracker>,
    /// Whether the first announce has been sent, which is what tells a tracker a download
    /// has begun.
    started: bool,
    /// Whether any tracker has answered with peers.
    heard: bool,
    /// Why the download cannot go on.
    error: Option<String>,
    saved: Instant,
}

/// A tracker and its times.
struct Tracker {
    address: String,
    asked: Instant,
    interval: Duration,
}

/// One torrent, being played.
///
/// The session keeps its threads until it is dropped: dropping it stops the download and
/// keeps what has been fetched, which the app's player does when a video is closed.
pub struct Session {
    shared: Arc<Shared>,
}

/// Everything the threads share. The lock is held for short moments only: a read waits on
/// the condition variable rather than holding it, so the peers keep sending while the
/// player waits for a piece.
struct Shared {
    state: Mutex<State>,
    /// Woken when anything a reader is waiting for has changed: a block arrived, the
    /// description came, or the download failed.
    arrived: Condvar,
    stop: Arc<AtomicBool>,
    /// The coordinator's channel, kept so peer threads can be spawned as peers are found.
    events: Mutex<Option<Sender<Event>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// The number for the next peer.
    names: AtomicU32,
}

impl Session {
    /// Opens a torrent: a `magnet:` link, or the address of a `.torrent` file to fetch.
    /// Returns as soon as the download is under way; the file's size is not known until
    /// the description arrives, so the caller waits with [`Session::wait_ready`].
    pub fn open(source: &str, folder: &Path) -> Result<Self, String> {
        Self::open_with(source, folder, Settings::default())
    }

    /// Opens with settings of one's own, for tests.
    pub fn open_with(source: &str, folder: &Path, settings: Settings) -> Result<Self, String> {
        let (magnet, description) = read_source(source)?;
        // The film is in use from the moment it is asked for, which is before the first
        // piece is looked at: a trim running on the thread that closed the last film must
        // not find this one old enough to let go of.
        let info_hash = magnet.info_hash;
        trim::claim(&info_hash);
        let mut peer_id = [0u8; 20];
        // A peer is recognised by these twenty bytes, so they are made once per session
        // and are the first eight of the console's own random numbers.
        peer_id[..4].copy_from_slice(b"SB10");
        if let Err(error) = getrandom::getrandom(&mut peer_id[4..]) {
            trim::release(&info_hash);
            return Err(format!("the console's random numbers did not work: {error}"));
        }
        let state = State {
            magnet,
            folder: folder.to_path_buf(),
            settings,
            peer_id,
            meta: None,
            selection: None,
            storage: None,
            peers: Vec::new(),
            wanted: Vec::new(),
            tried: HashSet::new(),
            queue: VecDeque::new(),
            in_flight: HashMap::new(),
            read_position: 0,
            description: None,
            given: description,
            seeds: std::collections::HashSet::new(),
            trackers: Vec::new(),
            started: false,
            heard: false,
            error: None,
            saved: Instant::now(),
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            arrived: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            events: Mutex::new(None),
            threads: Mutex::new(Vec::new()),
            names: AtomicU32::new(0),
        });
        let (sender, receiver) = mpsc::channel();
        *shared.events.lock().unwrap() = Some(sender);
        let watching = Arc::clone(&shared);
    let coordinator = thread::Builder::new()
            .name("torrent".to_owned())
            .spawn(move || coordinate(watching, receiver));
        // A thread that could not be started is not a session, and nothing is holding the
        // film any more.
        let Ok(coordinator) = coordinator else {
            trim::release(&info_hash);
            return Err("the torrent's own thread would not start".to_owned());
        };
        shared.threads.lock().unwrap().push(coordinator);
        Ok(Self { shared })
    }

    /// Waits until the file's size is known, or `timeout` runs out. Fails with what went
    /// wrong: no tracker answered, nothing was there to fetch, or no peer has the file.
    pub fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap();
        while state.selection.is_none() {
            if let Some(error) = state.error.clone() {
                return Err(error);
            }
            let left = deadline
                .saturating_duration_since(Instant::now())
                .min(state.settings.read_timeout);
            if left.is_zero() {
                return Err(why(&state));
            }
            let (again, _) = self.shared.arrived.wait_timeout(state, left).unwrap();
            state = again;
        }
        Ok(())
    }

    /// Reads at `position` into `buffer`, waiting for the torrent to have the bytes.
    /// `Ok(0)` is the end of the file; the number after the count is where the next read
    /// should start. Fails when the wait ran out or the download failed, with the reason.
    ///
    /// This is also how a seek moves the download: the reader's position is the only thing
    /// that decides which pieces are asked for first.
    pub fn read(&self, position: u64, buffer: &mut [u8]) -> Result<(usize, u64), String> {
        if buffer.is_empty() {
            return Ok((0, position));
        }
        if self.size() == 0 {
            self.wait_ready(Duration::from_secs(1))?;
        }
        let mut state = self.shared.state.lock().unwrap();
        let deadline = Instant::now() + state.settings.read_timeout;
        loop {
            let (Some(meta), Some(selection)) = (state.meta.as_ref(), state.selection.as_ref()) else {
                if let Some(error) = state.error.clone() {
                    return Err(error);
                }
                drop(state);
                self.wait_ready(Duration::from_secs(1))?;
                state = self.shared.state.lock().unwrap();
                continue;
            };
            if position >= selection.length {
                return Ok((0, position));
            }
            let limit = (selection.length - position).min(buffer.len() as u64) as usize;
            let piece_length = meta.piece_length as u64;
            let base = selection.base;
            let storage = match state.storage.as_mut() {
                Some(storage) => storage,
                None => return Err("the torrent's pieces are not open".to_owned()),
            };
            let block_len = storage.block_len() as u64;
            let mut done = 0usize;
            let mut waiting = None;
            while done < limit {
                let at = base + position + done as u64;
                let piece = (at / piece_length) as u32;
                let within = at % piece_length;
                let block = (within / block_len) as u32;
                let offset = (within % block_len) as usize;
                let size = (storage.block_size(piece, block) as usize)
                    .saturating_sub(offset)
                    .min(limit - done);
                if size == 0 {
                    return Err("the torrent's pieces do not fit the read".to_owned());
                }
                if storage.has_block(piece, block) || storage.has_piece(piece) {
                    // Only what was asked for is read: the block is as long as a piece is
                    // wide at most, and a player asks for a few kilobytes at a time.
                    storage
                        .read_at(
                            piece,
                            block,
                            offset as u32,
                            &mut buffer[done..done + size],
                        )
                        .map_err(|error| format!("piece {piece} could not be read: {error}"))?;
                    done += size;
                    continue;
                }
                // Not here yet: put the reader where it needs the data, so the pieces
                // around this spot are the ones asked for next.
                waiting = Some((piece, block));
                break;
            }
            if let Some((_, _)) = waiting {
                state.read_position = position + done as u64;
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    let reason = why(&state);
                    drop(state);
                    return Err(format!("the torrent stopped sending: {reason}"));
                }
                let (again, _) = self.shared.arrived.wait_timeout(state, left).unwrap();
                state = again;
                if let Some(error) = state.error.clone() {
                    return Err(error);
                }
                continue;
            }
            state.read_position = position + done as u64;
            return Ok((done, state.read_position));
        }
    }

    /// Sets where the next read starts, and so which pieces are fetched first.
    pub fn seek(&self, position: u64) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.read_position = position;
        }
    }

    /// The chosen file's size, or 0 while the description has not arrived.
    pub fn size(&self) -> u64 {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.selection.as_ref().map(|selection| selection.length))
            .unwrap_or(0)
    }

    /// How much of the file is held, for the player's own line and the log.
    /// Whether the download has given up for good, as against not knowing yet. A caller
/// waiting for the file's size needs the difference: the first means stop waiting, the
/// second means keep waiting a little longer.
pub fn failed(&self) -> bool {
    self.shared
        .state
        .lock()
        .map_or(true, |state| state.error.is_some())
}

pub fn progress(&self) -> Progress {
        let Ok(state) = self.shared.state.lock() else {
            return Progress {
                done: 0,
                total: 0,
                state: Phase::Failed,
                peers: 0,
                seeders: 0,
            };
        };
        let (done, total) = match (&state.storage, &state.selection) {
            (Some(storage), Some(selection)) => (
                storage
                    .done_bytes()
                    .min(selection.base.saturating_add(selection.length))
                    .saturating_sub(selection.base),
                selection.length,
            ),
            _ => (0, 0),
        };
        let seeders = match state.meta.as_ref() {
            Some(meta) => state
                .peers
                .iter()
                .filter(|peer| (0..meta.piece_count()).all(|piece| peer.holds(piece)))
                .count(),
            None => 0,
        };
        let phase = if state.error.is_some() {
            Phase::Failed
        } else if state.selection.is_some() {
            Phase::Downloading
        } else {
            Phase::Looking
        };
        Progress {
            done,
            total,
            state: phase,
            peers: state.peers.len(),
            seeders,
        }
    }

    /// What is known about the torrent so far, for the log when it cannot be played.
    pub fn describe(&self) -> String {
        let Ok(state) = self.shared.state.lock() else {
            return String::new();
        };
        let mut text = format!(
            "{} ({}): {} peers, {} trackers",
            state.meta.as_ref().map_or_else(
                || state
                    .magnet
                    .name
                    .clone()
                    .unwrap_or_else(|| "unnamed".to_owned()),
                |meta| meta.name.clone()
            ),
            magnet::info_hash_to_hex(&state.magnet.info_hash),
            state.peers.len(),
            state.trackers.len(),
        );
        if let Some(storage) = state.storage.as_ref() {
            text.push_str(&format!(
                ", {} of {} pieces ({:.0}%)",
                storage.done_pieces(),
                storage.piece_count(),
                100.0 * storage.done_bytes() as f64 / storage.total_bytes().max(1) as f64
            ));
        }
        if let Some(error) = state.error.as_ref() {
            text.push_str(&format!(": {error}"));
        }
        text
    }

    /// Stops the download, keeping what has been fetched. Called by [`Drop`], so the app
    /// does not need to call it.
    pub fn close(&self) {
        if self.shared.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let (folder, trackers, info_hash, peer_id, port) = {
            let mut state = self.shared.state.lock().unwrap();
            for peer in &state.peers {
                peer.shut();
            }
            state.peers.clear();
            if let Some(storage) = state.storage.as_mut() {
                let _ = storage.save();
            }
            (
                state.folder.clone(),
                state.trackers.iter().map(|tracker| tracker.address.clone()).collect::<Vec<_>>(),
                state.magnet.info_hash,
                state.peer_id,
                state.settings.port,
            )
        };
        self.shared.arrived.notify_all();
        // The trackers are told the download is over, so a leecher's ratio is not spoiled
        // by one that has finished playing and stopped. This is on a thread of its own
        // and waits for none of them: closing a video must not hang on a tracker that is
        // slow or gone, and a few trackers are enough to say it.
        let telling = thread::Builder::new()
            .name("torrent-stopped".to_owned())
            .spawn(move || {
                for address in trackers.into_iter().take(3) {
                    let _ = tracker::announce(
                        &address,
                        &info_hash,
                        &peer_id,
                        port,
                        0,
                        "stopped",
                        Duration::from_secs(3),
                    );
                }
            });
        let _ = telling;
        let mut threads = self.shared.threads.lock().unwrap();
        for handle in threads.drain(..) {
            let _ = handle.join();
        }
        // Nothing can write the film now, so it is no longer in use and a trim is free to
        // let it go if it is among the oldest. This is at the end of `close` rather than in
        // `Drop`, and the guard at the top has already made a second call nothing, so the
        // film is let go once however many times this is reached.
        trim::release(&info_hash);
        // The folder is kept to the room the app asked for as each film is let go, rather
        // than on a timer of its own, so the card cannot fill up while nothing is being
        // watched. This is on a thread of its own and is not waited for, so that closing a
        // video never waits on deleting the next one; a film still in use is stepped over
        // rather than waited for.
        let trimming = thread::Builder::new()
            .name("torrent-trim".to_owned())
            .spawn(move || {
                let freed = trim::trim(&folder, super::limit_now());
                if freed > 0 {
                    crate::kernel_log(&format!("torrent: {freed} bytes freed for a full folder"));
                }
            });
        let _ = trimming;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// Why the player was given nothing, in words it can show.
fn why(state: &State) -> String {
    if let Some(error) = state.error.as_ref() {
        return error.clone();
    }
    if state.trackers.is_empty() && state.magnet.web_seeds.is_empty() {
        return "the link has no trackers, and no other way to find peers".to_owned();
    }
    if state.peers.is_empty() && !state.heard {
        return "no tracker answered".to_owned();
    }
    "no peer has this file".to_owned()
}

/// Reads what the app was given: a magnet link, or the address of a `.torrent` file.
fn read_source(source: &str) -> Result<(Magnet, Option<Vec<u8>>), String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err("no torrent was given".to_owned());
    }
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("magnet:") {
        return Ok((Magnet::parse(trimmed)?, None));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let response = HTTP
            .get(trimmed)
            .timeout(Duration::from_secs(30))
            .call()
            .map_err(|error| format!("the torrent file could not be fetched: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_DESCRIPTION as u64)
            .read_limited(&mut bytes)
            .map_err(|error| format!("the torrent file could not be read: {error}"))?;
        return Ok((magnet_of(&bytes)?, Some(bytes)));
    }
    Err(format!("a torrent cannot be opened from \"{trimmed}\""))
}

/// Reads to the end of what is there, and no further, so a description that is far larger
/// than any torrent's does not fill the console's memory.
trait ReadLimited {
    fn read_limited(&mut self, out: &mut Vec<u8>) -> std::io::Result<()>;
}

impl<R: Read> ReadLimited for R {
    fn read_limited(&mut self, out: &mut Vec<u8>) -> std::io::Result<()> {
        let mut buffer = [0u8; 32 * 1024];
        loop {
            let read = self.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            out.extend_from_slice(&buffer[..read]);
        }
    }
}

/// A magnet out of a torrent's own bytes: its hash and the trackers it names, so the
/// description never has to be asked of a peer.
fn magnet_of(bytes: &[u8]) -> Result<Magnet, String> {
    let meta = Meta::from_torrent(bytes)?;
    let mut trackers: Vec<String> = Vec::new();
    if let Ok(document) = bencode::parse(bytes, MAX_DESCRIPTION) {
        if let Some(announce) = document.get("announce").and_then(|node| node.text()) {
            trackers.push(announce.to_owned());
        }
        if let Some(lists) = document.get("announce-list").and_then(|node| node.list()) {
            for list in lists {
                for entry in list.list().unwrap_or_default() {
                    if let Some(address) = entry.text() {
                        trackers.push(address.to_owned());
                    }
                }
            }
        }
    }
    trackers.dedup();
    Ok(Magnet {
        info_hash: meta.info_hash,
        trackers,
        name: Some(meta.name),
        length: Some(meta.total_length),
        ..Magnet::default()
    })
}

/// The coordinator: trackers, peers, what is asked of them, and what is written.
fn coordinate(shared: Arc<Shared>, events: Receiver<Event>) {
    while !shared.stop.load(Ordering::SeqCst) {
        let mut spoke = false;
        while let Ok(event) = events.try_recv() {
            spoke = true;
            handle_event(&shared, event);
        }
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
tick(&shared);
        // A reader waiting for a piece is woken when that piece lands, which is what this
        // notification is for.
        shared.arrived.notify_all();
        if !spoke {
            // Nothing came: wait a moment rather than looking again at once. It is a sleep
            // and not a receive, because receiving here would take the first thing that
            // arrived off the queue and drop it without ever reaching the state.
            thread::sleep(TICK);
        }
    }
    let mut state = shared.state.lock().unwrap();
    for peer in &state.peers {
        peer.shut();
    }
    if let Some(storage) = state.storage.as_mut() {
        let _ = storage.save();
    }
}

/// One turn of the coordinator's work.
///
/// It happens in three parts, and the middle one is without the lock: a tracker that is
/// slow, or behind a firewall, can take the whole of its timeout, and nothing that has
/// already been found may wait on it. What is asked of the outside world is decided under
/// the lock, asked for without it, and its answers are brought back under it.
fn tick(shared: &Arc<Shared>) {
    let now = Instant::now();
    let asking = what_to_ask(shared, now);

    // Without the lock: the trackers and the web seeds, which is all network and can take
    // as long as a slow one takes.
    let mut answers: Vec<(usize, Result<tracker::Announce, String>)> = Vec::new();
    for ask in asking.trackers {
        answers.push((
            ask.index,
            tracker::announce(
                &ask.address,
                &ask.info_hash,
                &ask.peer_id,
                ask.port,
                ask.left,
                ask.event,
                ask.timeout,
            ),
        ));
    }
    let seeded = asking.seed.as_deref().map(fetch_web_seed);

    // Back under the lock: what came of it, and the rest of the work.
    let mut state = shared.state.lock().unwrap();
    if shared.stop.load(Ordering::SeqCst) {
        return;
    }
    if let Some(bytes) = seeded.flatten() {
        match take_description(&mut state, &bytes) {
            Ok(()) => state.given = Some(bytes),
            Err(error) => crate::kernel_log(&format!("torrent: a web seed's answer was refused: {error}")),
        }
    }
    for (index, answer) in answers {
        match answer {
            Ok(answer) => {
                if let Some(tracker) = state.trackers.get_mut(index) {
                    tracker.asked = now;
                    tracker.interval = answer.interval;
                }
                state.started = true;
                if !answer.peers.is_empty() {
                    state.heard = true;
                }
                for address in answer.peers {
                    if state.peers.len() + state.wanted.len() < state.settings.peers * 3
                        && state.tried.insert(address)
                    {
                        state.wanted.push(address);
                    }
                }
            }
            Err(error) => {
                // A tracker that cannot be reached is not the end of the download: the
                // others may answer, and the peers already connected keep going.
                crate::kernel_log(&format!("torrent: {}", error));
                if let Some(tracker) = state.trackers.get_mut(index) {
                    tracker.asked = now;
                    tracker.interval = Duration::from_secs(300);
                }
            }
        }
    }

    connect_to_peers(shared, &mut state);
    drop_quiet_peers(&mut state, now);

    if state.meta.is_none() {
        collect_description(&mut state);
    } else {
        expire_requests(&mut state, now);
        queue_work(&mut state);
        dispatch(&mut state);
        if now.duration_since(state.saved) > SAVE_EVERY {
            if let Some(storage) = state.storage.as_mut() {
                let _ = storage.save();
            }
            state.saved = now;
        }
    }

    // A link with no tracker, no web seed and nothing kept from a run before has no way to
    // find peers at all, and waiting for it to look like it is trying would only leave the
    // player waiting on something that will never happen.
    if state.meta.is_none() && state.trackers.is_empty() && state.error.is_none() {
        fail(&mut state, "the link has no trackers, and no other way to find peers");
    }

    // Nothing has come for long enough that nothing will.
    if state.error.is_none() && !state.heard && state.peers.is_empty() && !state.started {
        let since = state
            .trackers
            .iter()
            .map(|tracker| now.duration_since(tracker.asked))
            .max()
            .unwrap_or_default();
        if since > GIVE_UP {
            fail(&mut state, "no tracker answered, so no peers could be found");
        }
    }
}

/// A tracker to be asked, with everything about the ask decided before the lock is let go
/// of it.
struct Ask {
    index: usize,
    address: String,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    port: u16,
    left: u64,
    event: &'static str,
    timeout: Duration,
}

/// What this turn is to ask of the outside world.
struct Asking {
    trackers: Vec<Ask>,
    /// The one web seed to try, when nothing else has given the torrent's description.
    seed: Option<String>,
}

/// The first half of a turn: everything decided from what is already in hand, which is
/// done under the lock because it is the state being read and changed.
fn what_to_ask(shared: &Arc<Shared>, now: Instant) -> Asking {
    let mut state = shared.state.lock().unwrap();
    if shared.stop.load(Ordering::SeqCst) {
        return Asking { trackers: Vec::new(), seed: None };
    }
    // The torrent's own description: what the app gave, then what a run before left, before
    // any peer or web seed is worth asking.
    if state.meta.is_none() {
        if let Some(bytes) = state.given.take() {
            match take_description(&mut state, &bytes) {
                Ok(()) => state.given = Some(bytes),
                // The description the stream itself carried is all there will ever be, so
                // what went wrong with it is what the player is told.
                Err(error) => {
                    crate::kernel_log(&format!("torrent: description refused: {error}"));
                    fail(&mut state, &error);
                }
            }
        }
    }
    if state.meta.is_none() && state.error.is_none() {
        if let Some(bytes) = cached_description(&state) {
            match take_description(&mut state, &bytes) {
                Ok(()) => state.given = Some(bytes),
                // What a run before left is only a way in: peers may still have the
                // description, so this one is logged and stepped over.
                Err(error) => {
                    crate::kernel_log(&format!("torrent: kept description refused: {error}"))
                }
            }
        }
    }
    if state.trackers.is_empty() {
        state.trackers = state
            .magnet
            .trackers
            .iter()
            .map(|address| Tracker {
                address: address.clone(),
                asked: now - Duration::from_secs(3600),
                interval: Duration::ZERO,
            })
            .collect();
    }
    let trackers = state
        .trackers
        .iter()
        .enumerate()
        .filter(|(_, tracker)| tracker.interval.is_zero() || now.duration_since(tracker.asked) >= tracker.interval)
        .map(|(index, tracker)| Ask {
            index,
            address: tracker.address.clone(),
            info_hash: state.magnet.info_hash,
            peer_id: state.peer_id,
            port: state.settings.port,
            left: state.storage.as_ref().map_or(0, |storage| {
                storage.total_bytes().saturating_sub(storage.done_bytes())
            }),
            event: if state.started { "" } else { "started" },
            timeout: state.settings.announce_timeout,
        })
        .collect();
    let seed = (state.meta.is_none() && state.error.is_none())
        .then(|| state.magnet.web_seeds.first().cloned())
        .flatten();
    Asking { trackers, seed }
}

/// A web seed's answer (BEP 53): a whole torrent file, or the first piece of one, put back
/// into the form a `.torrent` file is. It is fetched without the lock, because it is a web
/// request and can take as long as the site takes.
fn fetch_web_seed(seed: &str) -> Option<Vec<u8>> {
    let response = HTTP.get(seed).timeout(Duration::from_secs(10)).call().ok()?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_DESCRIPTION as u64)
        .read_limited(&mut bytes)
        .ok()?;
    whole_torrent(&bytes).ok()
}
/// Takes a torrent's description and opens its pieces.
fn take_description(state: &mut State, bytes: &[u8]) -> Result<(), String> {
    let meta = Meta::from_torrent(bytes)?;
    if meta.info_hash != state.magnet.info_hash {
        return Err("the description is for another torrent".to_owned());
    }
    let storage = Storage::open(&state.folder, &meta, Some(bytes))
        .map_err(|error| format!("the torrent's pieces could not be opened: {error}"))?;
    let selection = meta.select(
        state.magnet.file_index,
        &state.magnet.file_must_include,
        state.magnet.length,
    )?;
    crate::kernel_log(&format!(
        "torrent: {} playing \"{}\", {} bytes of {} files",
        meta.name,
        meta.files[selection.file].path,
        selection.length,
        meta.files.len(),
    ));
    state.description = None;
    state.meta = Some(meta);
    state.selection = Some(selection);
    state.storage = Some(storage);
    Ok(())
}

/// The description kept by an earlier run, when it is this torrent's.
fn cached_description(state: &State) -> Option<Vec<u8>> {
    let path = state.folder.join(format!(
        "{}.torrent",
        magnet::info_hash_to_hex(&state.magnet.info_hash)
    ));
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > MAX_DESCRIPTION {
        return None;
    }
    match Meta::from_torrent(&bytes) {
        Ok(meta) if meta.info_hash == state.magnet.info_hash => Some(bytes),
        _ => None,
    }
}

/// The torrent file a web seed's answer is put back into: what peers are asked for is the
/// `info` dictionary, and what is kept on the card is a torrent file, whose hash is taken
/// over exactly those bytes.
fn whole_torrent(source: &[u8]) -> Result<Vec<u8>, String> {
    let node = bencode::parse(source, MAX_DESCRIPTION)?;
    let info = node
        .get("info")
        .ok_or_else(|| "there is no info dictionary in it".to_owned())?;
    let mut torrent = b"d4:infod".to_vec();
    torrent.extend_from_slice(&source[info.start..info.end]);
    torrent.push(b'e');
    torrent.push(b'e');
    Ok(torrent)
}

/// Fills in the description out of the parts peers send (BEP 9).
fn collect_description(state: &mut State) {
    let now = Instant::now();
    for index in 0..state.peers.len() {
        let (Some(id), Some(size)) = (
            state.peers[index].extended.as_ref().and_then(|handshake| handshake.ut_metadata),
            state.peers[index]
                .extended
                .as_ref()
                .and_then(|handshake| handshake.metadata_size),
        ) else {
            continue;
        };
        if size == 0 || size as usize > MAX_DESCRIPTION {
            continue;
        }
        // Every peer that says it has the description is asked, not only the first: one
        // that advertises it and then says nothing must not keep the rest from answering.
        // Asking is rate-limited, which is both what a swarm expects and what keeps a
        // request from being repeated four hundred times a second.
        let asked_now = now.duration_since(state.peers[index].metadata_asked) < METADATA_ASK_EVERY;
        if asked_now {
            continue;
        }
        if state
            .description
            .as_ref()
            .is_none_or(|description| description.size != size)
        {
            if state
                .description
                .as_ref()
                .is_some_and(|partly| !partly.parts.iter().all(Option::is_none))
            {
                // What is being put together is left alone: a second size is a peer that
                // is not answering about the same torrent.
                continue;
            }
            state.description = Some(Description::new(size));
        }
        let Some(description) = state.description.as_ref() else {
            continue;
        };
        let wanted: Vec<u32> = (0..description.parts.len() as u32)
            .filter(|part| description.parts[*part as usize].is_none())
            .take(16)
            .collect();
        state.peers[index].metadata_asked = now;
        for part in wanted {
            let request = MetadataMessage::Request { piece: part }.message(id);
            state.peers[index].send(&request);
        }
    }
    let Some(description) = state.description.as_ref() else {
        return;
    };
    let Some(bytes) = description.joined() else {
        return;
    };
    let info_hash = state.magnet.info_hash;
    match Meta::from_info(&bytes, &info_hash) {
        Ok(meta) => {
            state.description = None;
            match take_meta(state, meta) {
                Ok(()) => {}
                Err(error) => fail(state, &error),
            }
        }
        Err(error) => {
            crate::kernel_log(&format!("torrent: a peer's description was refused: {error}"));
            state.description = None;
        }
    }
}

/// Takes a description that came from a peer, and opens the pieces.
fn take_meta(state: &mut State, meta: Meta) -> Result<(), String> {
    let selection = meta.select(
        state.magnet.file_index,
        &state.magnet.file_must_include,
        state.magnet.length,
    )?;
    let folder = state.folder.clone();
    let storage = Storage::open(&folder, &meta, None)
        .map_err(|error| format!("the torrent's pieces could not be opened: {error}"))?;
    crate::kernel_log(&format!(
        "torrent: {} playing \"{}\", {} bytes, from peers",
        meta.name,
        meta.files[selection.file].path,
        selection.length,
    ));
    state.meta = Some(meta);
    state.selection = Some(selection);
    state.storage = Some(storage);
    Ok(())
}

/// Blocks asked of a peer that have gone unanswered for long enough are asked of someone
/// else. Without this a peer that accepts a request and then says nothing about it keeps
/// the block to itself until it is dropped, and the reader waits for a piece that another
/// peer would have sent at once.
fn expire_requests(state: &mut State, now: Instant) {
    let stale: Vec<(u32, u32)> = state
        .in_flight
        .iter()
        .filter(|(_, (_, asked))| now.duration_since(*asked) > REQUEST_TIMEOUT)
        .map(|(key, _)| *key)
        .collect();
    for key in stale {
        if let Some((who, _)) = state.in_flight.remove(&key) {
            if let Some(index) = find_peer(state, who) {
                state.peers[index].outstanding = state.peers[index].outstanding.saturating_sub(1);
            }
            state.queue.push_front(key);
        }
    }
}

/// Fills the queue with the blocks around the reader, and behind it in order.
fn queue_work(state: &mut State) {
    let (Some(meta), Some(selection)) = (state.meta.as_ref(), state.selection.as_ref()) else {
        return;
    };
    let Some(storage) = state.storage.as_ref() else {
        return;
    };
    let piece_length = meta.piece_length as u64;
    let count = meta.piece_count();
    if count == 0 {
        return;
    }
    let first = ((selection.base.saturating_add(state.read_position) / piece_length) as u32)
        .min(count.saturating_sub(1));
    let mut held: HashSet<(u32, u32)> = state.in_flight.keys().copied().collect();
    held.extend(state.queue.iter().copied());
    let block_len = storage.block_len() as u64;
    for piece in first..(first + state.settings.read_ahead as u32).min(count) {
        if storage.has_piece(piece) {
            continue;
        }
        let blocks = storage.blocks_per_piece(piece);
        for block in 0..blocks {
            let key = (piece, block);
            if storage.has_block(piece, block) || !held.insert(key) {
                continue;
            }
            // The first piece of the file is shared with the files before it: what comes
            // before the file is skipped rather than fetched.
            let start = piece as u64 * piece_length + block as u64 * block_len;
            if start + storage.block_size(piece, block) as u64 <= selection.base {
                continue;
            }
            state.queue.push_back(key);
        }
    }
    // The queue is kept to what a reader is waiting for and what is just behind it.
    while state.queue.len() > state.settings.peers * 64 {
        state.queue.pop_back();
    }
}

/// Hands the queued blocks to the peers that can send them.
fn dispatch(state: &mut State) {
    if state
        .meta
        .as_ref()
        .is_some_and(|meta| meta.private)
    {
        // A private torrent's hashes are not fetched over the peer protocol: its peers
        // are all one account and there is no key here to prove anything with.
        return;
    }
    let mut unsent: Vec<(u32, u32)> = Vec::new();
    while let Some((piece, block)) = state.queue.pop_front() {
        let Some(storage) = state.storage.as_ref() else {
            return;
        };
        let begin = (block as u64 * storage.block_len() as u64) as u32;
        let length = storage.block_size(piece, block);
        let chosen = state
            .peers
            .iter()
            .enumerate()
            .filter(|(_, peer)| peer.unchoking && peer.outstanding < PEER_WINDOW && peer.holds(piece))
            .min_by_key(|(_, peer)| peer.outstanding)
            .map(|(index, _)| index);
        let Some(index) = chosen else {
            // Nobody can send it yet: it goes back to the front, where it is the first
            // thing tried once a peer that has the piece unchokes.
            unsent.push((piece, block));
            continue;
        };
        let mut sent = false;
        if !state.peers[index].interested {
            if state.peers[index].send(&Message::Interested) {
                state.peers[index].interested = true;
            }
        }
        if state.peers[index].send(&Message::Request { piece, begin, length }) {
            state.peers[index].outstanding += 1;
            state.in_flight.insert((piece, block), (state.peers[index].id, Instant::now()));
            sent = true;
        }
        if !sent {
            unsent.push((piece, block));
        }
    }
    for key in unsent.into_iter().rev() {
        state.queue.push_front(key);
    }
}

/// What a peer said, applied to the state.
fn handle_event(shared: &Arc<Shared>, event: Event) {
    let mut state = shared.state.lock().unwrap();
    match event {
        Event::Ready { id } => {
            if let Some(index) = find_peer(&state, id) {
                state.peers[index].last_heard = Instant::now();
            }
        }
        Event::Bitfield { id, bits } => {
            if let Some(index) = find_peer(&state, id) {
                // A bitfield is as long as the peer cares to make it, and a long one is
                // memory this client did not ask for: it is taken as far as the pieces it
                // can actually use.
                let wanted = state
                    .meta
                    .as_ref()
                    .map(|meta| (meta.piece_count() as usize).div_ceil(8) + 1)
                    .unwrap_or(BITFIELD_LIMIT)
                    .min(BITFIELD_LIMIT);
                state.peers[index].have = bits.into_iter().take(wanted).collect();
                state.peers[index].last_heard = Instant::now();
            }
        }
        Event::Have { id, piece } => {
            if let Some(index) = find_peer(&state, id) {
                // A piece number is a number a peer chose, and the bitfield it is written
                // into grows to reach it: one claiming the largest number there is would
                // otherwise have this client make half a gigabyte of memory for it.
                let beyond = state.meta.as_ref().is_some_and(|meta| piece >= meta.piece_count());
                let too_far = (piece as usize) / 8 >= BITFIELD_LIMIT;
                if !beyond && !too_far {
                    if (state.peers[index].have.len() as u64) <= piece as u64 / 8 {
                        state.peers[index].have.resize(piece as usize / 8 + 1, 0);
                    }
                    wire::bitfield_set(&mut state.peers[index].have, piece);
                }
                state.peers[index].last_heard = Instant::now();
            }
        }
        Event::Unchoke { id } => {
            if let Some(index) = find_peer(&state, id) {
                state.peers[index].unchoking = true;
                state.peers[index].last_heard = Instant::now();
            }
        }
        Event::Choke { id } => {
            if let Some(index) = find_peer(&state, id) {
                state.peers[index].unchoking = false;
                state.peers[index].outstanding = 0;
            }
            reclaim(&mut state, id);
        }
        Event::Extended { id, handshake } => {
            if let Some(index) = find_peer(&state, id) {
                state.peers[index].last_heard = Instant::now();
                state.peers[index].extended = Some(handshake);
                // A peer that speaks the peer extension is asked which peers it knows
                // (BEP 11), which is how a magnet finds peers when its trackers are thin.
                let pex = state.peers[index]
                    .extended
                    .as_ref()
                    .is_some_and(|handshake| handshake.ut_pex.is_some());
                if pex {
                    let request = bencode::encode(&Value::Dict(vec![
                        (
                            b"added".to_vec(),
                            Node { value: Value::Int(0), start: 0, end: 0 },
                        ),
                        (
                            b"dropped".to_vec(),
                            Node { value: Value::Int(0), start: 0, end: 0 },
                        ),
                    ]));
                    state.peers[index].send(&Message::Extended { id: UT_PEX, payload: request });
                }
            }
        }
        Event::Metadata { id, piece, total, data } => {
            if let Some(index) = find_peer(&state, id) {
                state.peers[index].last_heard = Instant::now();
            }
            let size = total.unwrap_or_else(|| {
                state
                    .description
                    .as_ref()
                    .map(|description| description.size)
                    .unwrap_or_default()
            });
            if size == 0 || size as usize > MAX_DESCRIPTION {
                return;
            }
            if state
                .description
                .as_ref()
                .is_none_or(|description| description.size != size)
            {
                // A description being put together is never thrown away partway: a peer
                // that says a different size after some parts have arrived is not believed,
                // because one such message would otherwise keep it from ever being whole.
                if state.description.as_ref().is_some_and(|partly| !partly.parts.iter().all(Option::is_none))
                {
                    return;
                }
                state.description = Some(Description::new(size));
            }
            let (Some(description), Ok(part)) = (state.description.as_mut(), usize::try_from(piece))
            else {
                return;
            };
            // A part is only taken when it is the size the description says that part is,
            // so what is held stays within what was asked for.
            if !description.accepts(part, &data) {
                return;
            }
            if let Some(part) = description.parts.get_mut(part) {
                *part = Some(data);
            }
        }
        Event::Peers { peers } => {
            for address in peers {
                if state.peers.len() + state.wanted.len() < state.settings.peers * 4
                    && state.tried.insert(address)
                {
                    state.wanted.push(address);
                }
            }
        }
        Event::Piece { id, piece, begin, data } => {
            let piece_length = state.meta.as_ref().map_or(0, |meta| meta.piece_length as u64);
            let block_len = state
                .storage
                .as_ref()
                .map_or(0, |storage| storage.block_len() as u64);
            if piece_length == 0 || block_len == 0 {
                return;
            }
            let block = ((begin as u64 % piece_length) / block_len) as u32;
            state.in_flight.remove(&(piece, block));
            if let Some(index) = find_peer(&state, id) {
                state.peers[index].outstanding = state.peers[index].outstanding.saturating_sub(1);
                state.peers[index].last_heard = Instant::now();
            }
            let Some(storage) = state.storage.as_mut() else {
                return;
            };
            match storage.write_block(piece, block, &data) {
                // The piece is whole, and checks out.
                Ok(true) => {}
                // It did not: what was kept of it is thrown away, and it is asked for
                // again from whoever is left.
                Ok(false) => {
                    let same: Vec<(u32, u32)> = state
                        .in_flight
                        .keys()
                        .copied()
                        .filter(|(held, _)| *held == piece)
                        .collect();
                    for key in same {
                        state.in_flight.remove(&key);
                        state.queue.push_front(key);
                    }
                    crate::kernel_log(&format!(
                        "torrent: piece {piece} did not check out, and was asked for again"
                    ));
                }
                Err(error) => crate::kernel_log(&format!("torrent: piece {piece}: {error}")),
            }
        }
        Event::Gone { id } => drop_peer(&mut state, id),
    }
}

/// Where a peer is in the list.
fn find_peer(state: &State, id: u32) -> Option<usize> {
    state.peers.iter().position(|peer| peer.id == id)
}

/// Everything that was asked of a peer that has gone or choked is asked of someone else.
fn reclaim(state: &mut State, id: u32) {
    let orphaned: Vec<(u32, u32)> = state
        .in_flight
        .iter()
        .filter(|(_, (who, _))| *who == id)
        .map(|(key, _)| *key)
        .collect();
    for key in orphaned {
        state.in_flight.remove(&key);
        state.queue.push_front(key);
    }
}

/// Forgets a peer, and whatever was asked of it.
fn drop_peer(state: &mut State, id: u32) {
    if let Some(index) = find_peer(state, id) {
        state.peers[index].shut();
        state.peers.remove(index);
    }
    reclaim(state, id);
}

/// Peers that have gone quiet, dropped so that others can take their place.
fn drop_quiet_peers(state: &mut State, now: Instant) {
    let quiet: Vec<(u32, SocketAddr)> = state
        .peers
        .iter()
        .filter(|peer| now.duration_since(peer.last_heard) > PEER_TIMEOUT)
        .map(|peer| (peer.id, peer.addr))
        .collect();
    for (id, addr) in quiet {
        crate::kernel_log(&format!("torrent: {addr} went quiet, and was left"));
        drop_peer(state, id);
    }
    // A peer is told now and then that we are still here.
    for peer in state.peers.iter_mut() {
        if now.duration_since(peer.last_sent) > KEEPALIVE && peer.send(&Message::KeepAlive) {
            peer.last_sent = now;
        }
    }
}

/// Starts a thread for each address gathered, up to the number of peers wanted.
fn connect_to_peers(shared: &Arc<Shared>, state: &mut State) {
    let Some(events) = shared.events.lock().unwrap().clone() else {
        return;
    };
    while state.peers.len() < state.settings.peers {
        let Some(address) = state.wanted.pop() else { break };
        let info_hash = state.magnet.info_hash;
        let peer_name = state.peer_id;
        let timeout = state.settings.connect_timeout;
        let stop = Arc::clone(&shared.stop);
        let id = shared.names.fetch_add(1, Ordering::SeqCst);
        let (asker, asked) = mpsc::channel();
        state.peers.push(Peer {
            id,
            addr: address,
            commands: asker,
            unchoking: false,
            have: Vec::new(),
            interested: false,
            outstanding: 0,
            extended: None,
            metadata_asked: Instant::now() - METADATA_ASK_EVERY,
            last_heard: Instant::now(),
            last_sent: Instant::now(),
        });
        let to_events = events.clone();
        let spawned = thread::Builder::new()
            .name(format!("torrent-peer-{id}"))
            .spawn(move || run_peer(id, address, info_hash, peer_name, asked, to_events, stop, timeout));
        if let Err(error) = spawned {
            crate::kernel_log(&format!("torrent: no thread for {address}: {error}"));
            drop_peer(state, id);
        }
    }
}

/// What the coordinator asks a peer thread to do with the socket it holds.
enum Command {
    /// Write this to the peer.
    Say(Message),
    /// The peer is being let go.
    LetGo,
}

/// A peer thread: greet, greet back, then hand what the peer says to the coordinator,
/// and write what the coordinator asks for.
///
/// The socket belongs to this thread alone. Nothing duplicates it: what is to be written
/// is handed over on a channel and answered here, between the reads. That is why the
/// socket is read without blocking after the greeting: a peer that has nothing to say
/// costs a read that fails at once, and a message waiting to be written is written
/// without waiting for the peer to speak again.
fn run_peer(
    id: u32,
    address: SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    commands: Receiver<Command>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
    connect_timeout: Duration,
) {
    let gone = |events: &Sender<Event>| {
        let _ = events.send(Event::Gone { id });
    };
    let mut socket = match TcpStream::connect_timeout(&address, connect_timeout) {
        Ok(socket) => socket,
        Err(error) => {
            crate::kernel_log(&format!("torrent: {address} did not answer: {error}"));
            gone(&events);
            return;
        }
    };
    let _ = socket.set_nodelay(true);
    // The greeting is blocking: a peer that has just been connected to may take a moment
    // to answer, and there is nothing else to do until it does.
    let _ = socket.set_read_timeout(Some(Duration::from_secs(HANDSHAKE_WAIT)));
    if socket
        .write_all(&Handshake::new(&info_hash, &peer_id).bytes())
        .is_err()
    {
        gone(&events);
        return;
    }
    let mut greeting = [0u8; wire::HANDSHAKE_LEN];
    if !read_fully(&mut socket, &mut greeting, &stop) {
        gone(&events);
        return;
    }
    match Handshake::parse(&greeting) {
        Ok(their) if their.info_hash == info_hash => {}
        Ok(_) => {
            crate::kernel_log(&format!(
                "torrent: {address} greeted for another torrent, and was left"
            ));
            gone(&events);
            return;
        }
        Err(error) => {
            crate::kernel_log(&format!("torrent: {address} did not greet: {error}"));
            gone(&events);
            return;
        }
    }
    let ours = ExtendedHandshake {
        ut_metadata: Some(UT_METADATA),
        ut_pex: Some(UT_PEX),
        metadata_size: None,
        client: Some(CLIENT.to_owned()),
    };
    if wire::write_message(&mut socket, &ours.message(0)).is_err() {
        gone(&events);
        return;
    }
    if events.send(Event::Ready { id }).is_err() {
        return;
    }
    // From here nothing waits on the peer: the reads never block, and what is to be
    // written is written between them.
    if socket.set_nonblocking(true).is_err() {
        gone(&events);
        return;
    }
    let mut inbox = Inbox::default();
    let mut scratch = [0u8; 32 * 1024];
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Everything waiting to be written, written.
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Say(message) => {
                    if wire::write_message(&mut socket, &message).is_err() {
                        gone(&events);
                        return;
                    }
                }
                Command::LetGo => return,
            }
        }
        // Whatever the peer has said.
        loop {
            match socket.read(&mut scratch) {
                Ok(0) => {
                    gone(&events);
                    return;
                }
                Ok(read) => inbox.take(&scratch[..read]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    continue
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(error) => {
                    crate::kernel_log(&format!("torrent: {address}: {error}"));
                    gone(&events);
                    return;
                }
            }
        }
        while let Some(message) = match inbox.next() {
            Ok(message) => message,
            Err(error) => {
                crate::kernel_log(&format!("torrent: {address}: {error}"));
                gone(&events);
                return;
            }
        } {
            if !pass_on(id, &message, &events) {
                gone(&events);
                return;
            }
        }
        // Then wait a moment for something to write, rather than for the peer to speak.
        match commands.recv_timeout(TICK) {
            Ok(Command::Say(message)) => {
                if wire::write_message(&mut socket, &message).is_err() {
                    gone(&events);
                    return;
                }
            }
            Ok(Command::LetGo) => return,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                gone(&events);
                return;
            }
        }
    }
    gone(&events);
}

/// What has arrived from a peer, and what has been made of it so far: a peer sends its
/// messages in whatever sizes the network hands over, so a message is only read once all
/// of its bytes are here.
#[derive(Default)]
struct Inbox {
    bytes: Vec<u8>,
}

impl Inbox {
    fn take(&mut self, more: &[u8]) {
        // A peer that keeps talking without ever finishing a message is let go rather
        // than allowed to use up the console's memory.
        if self.bytes.len() + more.len() > wire::MAX_MESSAGE + 4 {
            self.bytes.clear();
            return;
        }
        self.bytes.extend_from_slice(more);
    }

    /// The next whole message, when there is one.
    fn next(&mut self) -> Result<Option<Message>, String> {
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes([self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]]) as usize;
        if length == 0 {
            self.bytes.drain(..4);
            return Ok(Some(Message::KeepAlive));
        }
        if length > wire::MAX_MESSAGE {
            return Err(format!("a message of {length} bytes is more than any this reads"));
        }
        if self.bytes.len() < 4 + length {
            return Ok(None);
        }
        // The framing is read back through the protocol's own reader, so what is made of
        // these bytes here is exactly what is made of them anywhere else.
        let framed: Vec<u8> = self.bytes.drain(..4 + length).collect();
        wire::read_message(&mut std::io::Cursor::new(framed))
            .map_err(|error| format!("a message did not read: {error}"))
    }
}
/// Turns a peer's message into an event. False when the coordinator has gone, which is the
/// same as the peer being of no further use.
fn pass_on(id: u32, message: &Message, events: &Sender<Event>) -> bool {
    let event = match message {
        Message::KeepAlive | Message::Interested | Message::NotInterested => return true,
        Message::Choke => Event::Choke { id },
        Message::Unchoke => Event::Unchoke { id },
        Message::Have(piece) => Event::Have { id, piece: *piece },
        Message::Bitfield(bits) => Event::Bitfield { id, bits: bits.clone() },
        Message::Piece { piece, begin, data } => Event::Piece {
            id,
            piece: *piece,
            begin: *begin,
            data: data.clone(),
        },
        // Nothing is uploaded, so an ask, a cancel or a port goes past.
        Message::Cancel { .. } | Message::Request { .. } | Message::Port(_) => return true,
        Message::Extended { id: extension, payload } => {
            if *extension == 0 {
                match ExtendedHandshake::parse(payload) {
                    Ok(handshake) => Event::Extended { id, handshake },
                    Err(_) => return true,
                }
            } else if *extension == UT_METADATA {
                match MetadataMessage::parse(payload) {
                    Ok(MetadataMessage::Data { piece, total, data }) => {
                        Event::Metadata { id, piece, total, data }
                    }
                    // A reject, or a part of a description nobody asked for.
                    _ => return true,
                }
            } else if *extension == UT_PEX {
                match wire::parse_added_peers(payload) {
                    Ok(peers) => Event::Peers {
                        peers: peers.iter().filter_map(|bytes| endpoint(bytes)).collect(),
                    },
                    Err(_) => return true,
                }
            } else {
                return true;
            }
        }
        Message::Unknown { .. } => return true,
    };
    events.send(event).is_ok()
}

/// An address and a port as peers are sent them: four bytes and two.
fn endpoint(bytes: &[u8; 6]) -> Option<SocketAddr> {
    Some(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
    )))
}

/// Reads exactly `buffer.len()` bytes, through the read timeout, until the whole is there
/// or the peer stops sending.
fn read_fully<R: Read>(reader: &mut R, buffer: &mut [u8], stop: &AtomicBool) -> bool {
    let mut filled = 0usize;
    let mut quiet = 0;
    while filled < buffer.len() {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => return false,
            Ok(read) => {
                filled += read;
                quiet = 0;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                quiet += 1;
                if quiet >= 2 {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    true
}

/// Nothing will come, and the player is told why.
fn fail(state: &mut State, reason: &str) {
    if state.error.is_none() {
        crate::kernel_log(&format!("torrent: {reason}"));
        state.error = Some(reason.to_owned());
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{TcpListener, UdpSocket};
    use std::sync::atomic::AtomicUsize;

    use sha1::{Digest, Sha1};

    /// A torrent of this many pieces, each this big, made for these tests.
    const PIECES: u32 = 6;
    const PIECE_LEN: usize = 16 * 1024;

    /// The bytes of one piece: a pattern, so a mistake in a peer's answer shows up.
    fn piece(piece: u32) -> Vec<u8> {
        (0..PIECE_LEN)
            .map(|index| ((piece as usize * 31 + index * 7) % 251) as u8)
            .collect()
    }

    /// The whole file, in piece order.
    fn file() -> Vec<u8> {
        (0..PIECES).flat_map(piece).collect()
    }

    /// A torrent's `info` dictionary, written by hand: its keys in the order bencode puts
    /// them (length, name, piece length, pieces), with the real hash of every piece.
    fn info() -> Vec<u8> {
        let mut pieces = Vec::new();
        for index in 0..PIECES {
            let mut hasher = Sha1::new();
            hasher.update(piece(index));
            pieces.extend_from_slice(&hasher.finalize());
        }
        let mut info = b"d".to_vec();
        info.extend_from_slice(format!("6:lengthi{}e", PIECES as usize * PIECE_LEN).as_bytes());
        info.extend_from_slice(b"4:name8:test.bin");
        info.extend_from_slice(format!("12:piece lengthi{PIECE_LEN}e").as_bytes());
        info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
        info.extend_from_slice(&pieces);
        info.extend_from_slice(b"e");
        info.extend_from_slice(b"e");
        info
    }

    fn sha1(bytes: &[u8]) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        let mut out = [0u8; 20];
        out.copy_from_slice(&hasher.finalize());
        out
    }

    /// A folder of this test's own, made and emptied.
    fn folder(name: &str) -> PathBuf {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "stremio-torrent-test-{name}-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a folder for the test");
        path
    }

    /// A tracker (BEP 15 over UDP) that names one peer, and a seeder that has the whole
    /// file: the swarm a torrent is played out of, without a network.
    struct Swarm {
        tracker: SocketAddr,
        stop: Arc<AtomicBool>,
    }

    impl Swarm {
        fn start(info: &[u8], data: &[u8]) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let tracker = UdpSocket::bind("127.0.0.1:0").expect("a tracker port");
            tracker
                .set_read_timeout(Some(Duration::from_millis(200)))
                .expect("a timeout on the tracker");
            let tracker_address = tracker.local_addr().expect("the tracker's address");
            let seeder = TcpListener::bind("127.0.0.1:0").expect("a seeder port");
            seeder
                .set_nonblocking(true)
                .expect("a non-blocking seeder");
            let seeder_address = seeder.local_addr().expect("the seeder's address");

            let tracker_stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut buffer = [0u8; 2048];
                while !tracker_stop.load(Ordering::SeqCst) {
                    let Ok((read, from)) = tracker.recv_from(&mut buffer) else {
                        continue;
                    };
// A connect request (16 bytes) is answered with a connection to use; an announce
                    // request (98) with the one peer there is. Both answers start with the
                    // action and the transaction id, which is the one that was asked with.
                    let mut answer = Vec::with_capacity(30);
                    let action = if read == 16 { 0u32 } else { 1u32 };
                    // Both requests carry the transaction id in the last four bytes of
                    // their sixteen-byte head.
                    answer.extend_from_slice(&action.to_be_bytes());
                    answer.extend_from_slice(&buffer[12..16]);
                    if action == 0 {
                        answer.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_be_bytes());
                    } else {
                        answer.extend_from_slice(&1800u32.to_be_bytes()); // interval
                        answer.extend_from_slice(&0u32.to_be_bytes()); // leechers
                        answer.extend_from_slice(&1u32.to_be_bytes()); // seeders
                        if let SocketAddr::V4(address) = seeder_address {
                            answer.extend_from_slice(&address.ip().octets());
                            answer.extend_from_slice(&address.port().to_be_bytes());
                        }
                    }
                    let _ = tracker.send_to(&answer, from);
                }
            });

            let seeder_stop = Arc::clone(&stop);
            let info = info.to_vec();
            let data = data.to_vec();
            thread::spawn(move || {
                while !seeder_stop.load(Ordering::SeqCst) {
                    let Ok((socket, _)) = seeder.accept() else {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    let info = info.clone();
                    let data = data.clone();
                    let stop = Arc::clone(&seeder_stop);
                    // One thread per peer that connects, as a real seeder has.
thread::spawn(move || {
                        // The listener is non-blocking so it can be left, but what it hands
                        // over is not: a socket of its own, with a timeout on its reads.
                        let _ = socket.set_nonblocking(false);
                        let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                        let Ok(mut reader) = socket.try_clone() else {
                            return;
                        };
                        let mut greeting = [0u8; wire::HANDSHAKE_LEN];
        if read_fully(&mut reader, &mut greeting, &stop) {
                            let Ok(theirs) = Handshake::parse(&greeting) else {
                                return;
                            };
                            let mut writer = socket;
                            if writer
                                .write_all(
                                    &Handshake::new(&theirs.info_hash, b"-SB0001-abcdefghijkl")
                                        .bytes(),
                                )
                                .is_err()
                            {
                                return;
                            }
                            let mut bits = vec![0u8; ((PIECES + 7) / 8) as usize];
                            for index in 0..PIECES {
                                wire::bitfield_set(&mut bits, index);
                            }
    let ours = ExtendedHandshake {
                                ut_metadata: Some(1),
                                ut_pex: None,
                                metadata_size: Some(info.len() as u64),
                                client: Some("test seeder".to_owned()),
                            };
                            let said = [
                                Message::Bitfield(bits),
                                Message::Unchoke,
                                ours.message(0),
                            ];
                            for message in said {
                                if wire::write_message(&mut writer, &message).is_err() {
                                    return;
                                }
                            }
                            while !stop.load(Ordering::SeqCst) {
let message = match wire::read_message(&mut reader) {
                                    Ok(Some(message)) => { message },
                                    // A real seeder keeps its connection open through a
                                    // quiet moment; only a closed one ends this.
                                    Err(error)
                                        if matches!(
                                            error.kind(),
                                            std::io::ErrorKind::WouldBlock
                                                | std::io::ErrorKind::TimedOut
                                        ) =>
                                    {
                                        continue
                                    }
_ => break,
                                };
                                match message {
                                    Message::Request { piece, begin, length } => {
                                        let start = piece as usize * PIECE_LEN + begin as usize;
                                        let end = (start + length as usize).min(data.len());
                                        let block = Message::Piece {
                                            piece,
                                            begin,
                                            data: data[start..end].to_vec(),
                                        };
                                        if wire::write_message(&mut writer, &block).is_err() {
                                            break;
                                        }
                                    }
                                    Message::Extended { id: 1, payload } => {
                                        let Ok(MetadataMessage::Request { piece }) =
                                            MetadataMessage::parse(&payload)
                                        else {
                                            continue;
                                        };
                                        let start = piece as usize * wire::METADATA_CHUNK;
                                        let end = (start + wire::METADATA_CHUNK).min(info.len());
                                        let answer = MetadataMessage::Data {
                                            piece,
                                            total: (piece == 0).then_some(info.len() as u64),
                                            data: info[start..end].to_vec(),
                                        };
                                        if wire::write_message(&mut writer, &answer.message(1))
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    });
                }
            });

// The two servers outlive the client: what they are holding is let go by Drop.
            Swarm {
                tracker: tracker_address,
                stop,
            }
        }

        /// The magnet link of this torrent, pointing at this tracker.
        fn magnet(&self, info: &[u8]) -> String {
            format!(
                "magnet:?xt=urn:btih:{}&tr=udp://{}",
                magnet::info_hash_to_hex(&sha1(info)),
                self.tracker
            )
        }
    }

    impl Drop for Swarm {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Settings for a test: few peers, and no waiting about.
    fn quick() -> Settings {
        Settings {
            peers: 2,
            read_timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(5),
            announce_timeout: Duration::from_secs(5),
            read_ahead: 4,
            ..Settings::default()
        }
    }

/// The whole file, read a bit at a time from `from`.
    fn read_from(session: &Session, from: u64, size: u64) -> Vec<u8> {
        let mut got = Vec::with_capacity(size as usize);
        let mut at = from;
        while at - from < size {
            let mut buffer = vec![0u8; 4096];
            let (count, next) = session
                .read(at, &mut buffer)
                .unwrap_or_else(|error| panic!("{size} bytes wanted from {at}: {error}"));
            assert!(count > 0, "nothing was read at {at}");
            got.extend_from_slice(&buffer[..count]);
            at = next;
        }
        got
    }

    #[test]
    fn plays_a_torrent_that_is_still_downloading() {
        let info = info();
        let data = file();
        let swarm = Swarm::start(&info, &data);
        let folder = folder("play");
        let session = Session::open_with(&swarm.magnet(&info), &folder, quick())
            .expect("the torrent to open");
        session
            .wait_ready(Duration::from_secs(30))
            .unwrap_or_else(|error| panic!("the description to arrive: {error}: {}", session.describe()));
        assert_eq!(session.size(), data.len() as u64, "the file's size");

// The first bytes come before the rest has been fetched: the file is read through
        // the pieces around the reading position, so a player can start on them.
        assert_eq!(read_from(&session, 0, 4096), data[..4096]);
        assert_eq!(session.progress().state, Phase::Downloading);

        let all = read_from(&session, 0, data.len() as u64);
        assert_eq!(all.len(), data.len());
        assert!(all == data, "the file that came back is not the file that was seeded");
        assert_eq!(session.progress().done, data.len() as u64);
        drop(session);
        // The pieces are kept on the card, so a run after this one starts where it is.
        let kept = std::fs::read_dir(&folder)
            .expect("the torrent's folder")
            .filter_map(Result::ok)
            .count();
        assert!(kept >= 2, "the torrent's pieces were not kept: {kept} files");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn seeks_into_what_is_already_there() {
        let info = info();
        let data = file();
        let swarm = Swarm::start(&info, &data);
        let folder = folder("seek");
        let session = Session::open_with(&swarm.magnet(&info), &folder, quick())
            .expect("the torrent to open");
        session
            .wait_ready(Duration::from_secs(30))
            .expect("the description to arrive");

        let middle = PIECE_LEN * 3 + 512;
        session.seek(middle as u64);
        let from_middle = read_from(&session, middle as u64, 8192);
        assert_eq!(from_middle, data[middle..middle + 8192]);
        // And back to the beginning, which is what a user does after seeking.
        session.seek(0);
        assert_eq!(read_from(&session, 0, 4096), data[..4096]);
        drop(session);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn picks_up_where_a_previous_run_left_it() {
        let info = info();
        let data = file();
        let swarm = Swarm::start(&info, &data);
        let folder = folder("resume");
        {
            let session = Session::open_with(&swarm.magnet(&info), &folder, quick())
                .expect("the torrent to open");
            session
                .wait_ready(Duration::from_secs(30))
                .expect("the description to arrive");
            assert_eq!(read_from(&session, 0, 4096), data[..4096]);
        }
        // A second run of the same torrent: its description is on the card, so it is ready
        // at once, with nothing asked of any tracker.
        let session = Session::open_with(&swarm.magnet(&info), &folder, quick())
            .expect("the torrent to open again");
        session
            .wait_ready(Duration::from_secs(5))
            .expect("the kept description to be used");
        assert_eq!(session.size(), data.len() as u64);
        assert!(session.progress().done > 0, "nothing was kept from the first run");
        assert_eq!(read_from(&session, 0, 4096), data[..4096]);
        drop(session);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn says_why_a_torrent_that_cannot_be_played_cannot() {
        let folder = folder("nothing");
        // A magnet with no tracker has no way to find a peer, and says so rather than
        // waiting for ever.
        let magnet = format!(
            "magnet:?xt=urn:btih:{}",
            magnet::info_hash_to_hex(&sha1(b"nothing to find"))
        );
        let session = Session::open_with(&magnet, &folder, quick()).expect("the torrent to open");
        let error = session
            .wait_ready(Duration::from_secs(2))
            .expect_err("there is nothing to play");
        assert!(error.contains("tracker"), "{error}");
        drop(session);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
