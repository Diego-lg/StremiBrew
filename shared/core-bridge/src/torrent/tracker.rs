//! Asking trackers where the peers are: over HTTP (BEP 3) and over UDP (BEP 15).

use std::collections::HashSet;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::bencode::{self, Node};
use super::magnet;

/// How much of a tracker's answer is read. A hundred peers take twelve hundred bytes, so
/// this is room for every tracker that is generous and no more.
const MAX_ANSWER: usize = 1 << 20;

/// How often a tracker is asked again when it does not say: half an hour.
const DEFAULT_INTERVAL: i64 = 1800;

/// The outside of an interval worth obeying. A tracker that says "come back in a year"
/// (a counter that overflowed, or a machine with the wrong clock) would leave the
/// session without peers until then, so a day is as much as is believed.
const MAX_INTERVAL: i64 = 24 * 60 * 60;

/// Nothing waits on a tracker for less than this, whatever the caller asked for, because
/// a zero timeout on a socket means waiting forever.
const MIN_TIMEOUT: Duration = Duration::from_millis(200);

/// The bytes one peer takes in a UDP answer: four of address and two of port.
const COMPACT_PEER: usize = 6;
/// The same for an IPv6 peer: sixteen of address and two of port.
const COMPACT_PEER6: usize = 18;

/// The number every UDP exchange opens with (BEP 15).
const MAGIC: u64 = 0x4172_7109_980;

/// A tracker's answer: where to look, and when to ask again.
#[derive(Clone, Debug, PartialEq)]
pub struct Announce {
    /// The peers it named. Many trackers send a hundred; the session tries a handful and
    /// asks again later, so the rest are kept in case the first ones go quiet.
    pub peers: Vec<SocketAddr>,
    pub interval: Duration,
}

/// Asks one tracker for peers. `event` is "started" at the beginning and "stopped" at the
/// end; an empty event is the periodic one. `left` is how much of the file is still
/// missing, which trackers use to keep a downloader in line. `port` is the port this
/// client is reachable on.
///
/// `timeout` is how long one exchange may take; over UDP it bounds each of the two
/// requests, so a tracker that answers nothing can take twice as long. Every failure
/// comes back as the tracker's own address and what went wrong, because the session tries
/// several trackers and has to say which one let it down.
pub fn announce(
    tracker: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    left: u64,
    event: &str,
    timeout: Duration,
) -> Result<Announce, String> {
    let scheme = match tracker.split_once("://") {
        Some((scheme, _)) => scheme.to_ascii_lowercase(),
        None => String::new(),
    };
    match scheme.as_str() {
        "udp" | "udp4" | "udp6" => over_udp(
            &scheme, tracker, info_hash, peer_id, port, left, event, timeout,
        ),
        "http" | "https" => over_http(tracker, info_hash, peer_id, port, left, event, timeout),
        _ => Err(format!(
            "{tracker}: not a tracker address (udp, http or https)"
        )),
    }
}

/// How many peers a tracker's answer is read for. A tracker may send many more; the rest
/// are dropped rather than held.
pub const MAX_PEERS: usize = 200;

/// The peers of an answer, in the compact form (four bytes of address, two of port).
/// An IPv6 peer takes sixteen bytes of address instead, which is what makes this worth
/// having a function for.
pub fn compact_peers(peers: &[SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(peers.len() * COMPACT_PEER);
    for peer in peers {
        match peer {
            SocketAddr::V4(address) => {
                out.extend_from_slice(&address.ip().octets());
                out.extend_from_slice(&address.port().to_be_bytes());
            }
            SocketAddr::V6(address) => {
                out.extend_from_slice(&address.ip().octets());
                out.extend_from_slice(&address.port().to_be_bytes());
            }
        }
    }
    out
}

/// Asks over UDP, in the two exchanges BEP 15 asks for: first a connection, whose id the
/// announce carries, then the announce itself.
fn over_udp(
    scheme: &str,
    tracker: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    left: u64,
    event: &str,
    timeout: Duration,
) -> Result<Announce, String> {
    let timeout = timeout.max(MIN_TIMEOUT);
    let (host, port_of_tracker) = host_and_port(tracker)?;
    let address = resolve(&host, port_of_tracker, scheme)?;
    let socket = UdpSocket::bind(bind_for(&address))
        .map_err(|error| format!("{tracker}: a socket could not be opened: {error}"))?;
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("{tracker}: {error}"))?;
    socket
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("{tracker}: {error}"))?;
    // Once connected the socket only hears from the tracker, which is also a check that
    // nothing else on the network can pass itself off as the answer.
    socket
        .connect(address)
        .map_err(|error| format!("{tracker}: {address} could not be reached: {error}"))?;

    let mut request = Vec::with_capacity(98);
    request.extend_from_slice(&MAGIC.to_be_bytes());
    request.extend_from_slice(&0u32.to_be_bytes());
    let transaction = transaction_id();
    request.extend_from_slice(&transaction.to_be_bytes());
    socket
        .send(&request)
        .map_err(|error| format!("{tracker}: the request did not go out: {error}"))?;
    let answer = read(&socket, tracker)?;
    if answer.len() < 16 {
        return Err(format!(
            "{tracker}: the answer is only {} bytes",
            answer.len()
        ));
    }
    // The answer to a connection is the action, the id of the request it answers, and then
    // the connection id to use from here on (BEP 15).
    if be32(&answer[0..4]) != 0 {
        return Err(format!(
            "{tracker}: refused the connection (it answered action {})",
            be32(&answer[0..4])
        ));
    }
    if be32(&answer[4..8]) != transaction {
        return Err(format!("{tracker}: the answer is for another request"));
    }
    let connection = be64(&answer[8..16]);

    let mut request = Vec::with_capacity(98);
    request.extend_from_slice(&connection.to_be_bytes());
    request.extend_from_slice(&1u32.to_be_bytes());
    let transaction = transaction_id();
    request.extend_from_slice(&transaction.to_be_bytes());
    request.extend_from_slice(info_hash);
    request.extend_from_slice(peer_id);
    // Nothing has been fetched or given yet: the session only ever downloads.
    request.extend_from_slice(&0u64.to_be_bytes());
    request.extend_from_slice(&left.to_be_bytes());
    request.extend_from_slice(&0u64.to_be_bytes());
    request.extend_from_slice(&event_code(event).to_be_bytes());
    // The tracker is left to work out where this came from, and the key, which is a
    // number of the client's own choosing that stops identical clients being counted as
    // one, are both left at zero.
    request.extend_from_slice(&[0u8; 4]);
    request.extend_from_slice(&[0u8; 4]);
    request.extend_from_slice(&(-1i32 as u32).to_be_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    socket
        .send(&request)
        .map_err(|error| format!("{tracker}: the announce did not go out: {error}"))?;
    let answer = read(&socket, tracker)?;
    if answer.len() < 20 {
        return Err(format!(
            "{tracker}: the answer is only {} bytes",
            answer.len()
        ));
    }
    if be32(&answer[0..4]) != 1 {
        return Err(format!(
            "{tracker}: the announce was refused (it answered action {})",
            be32(&answer[0..4])
        ));
    }
    if be32(&answer[4..8]) != transaction {
        return Err(format!("{tracker}: the answer is for another request"));
    }
    let mut peers = Vec::new();
    let mut seen = HashSet::new();
    // Past the four numbers (the interval and how many are downloading and seeding, which
    // nothing here uses) come the peers, six bytes each.
    compact(&mut peers, &mut seen, &answer[20..]);
    Ok(Announce {
        peers,
        interval: interval_of(Some(be32(&answer[8..12]) as i64)),
    })
}

/// Asks over HTTP, which is one request whose query says who is asking and for what.
fn over_http(
    tracker: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    left: u64,
    event: &str,
    timeout: Duration,
) -> Result<Announce, String> {
    let timeout = timeout.max(MIN_TIMEOUT);
    let url = query(tracker, info_hash, peer_id, port, left, event)?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .user_agent("stremio-torrent/1")
        .build();
    let response = agent
        .get(&url)
        .call()
        .map_err(|error| format!("{tracker}: {error}"))?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_ANSWER as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("{tracker}: the answer did not arrive: {error}"))?;
    let node = bencode::parse(&body, MAX_ANSWER).map_err(|error| format!("{tracker}: {error}"))?;
    // A tracker that will not answer says so in as many words, and says why.
    if let Some(reason) = node.get("failure reason").and_then(|node| node.text()) {
        return Err(format!("{tracker} will not answer: {reason}"));
    }
    Ok(Announce {
        peers: answer_peers(&node),
        interval: interval_of(node.get("interval").and_then(|node| node.int())),
    })
}

/// The address to ask, with everything a tracker wants to know in it. An address may name
/// where the hash and the peer id go itself, with `{info_hash}` and `{peer_id}`; those are
/// filled in here, and where it does not, the two are added as ordinary parameters. An
/// address that wants the client's own IP cannot be filled in, since what address this
/// client has is not known here.
fn query(
    tracker: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    left: u64,
    event: &str,
) -> Result<String, String> {
    if tracker.contains("{ip}") || tracker.contains("{port}") {
        return Err(format!(
            "{tracker}: the address asks for the client's IP, which is not known here"
        ));
    }
    let hash_given = tracker.contains("{info_hash}");
    let peer_given = tracker.contains("{peer_id}");
    let mut url = tracker.to_owned();
    if hash_given {
        url = url.replace("{info_hash}", &hex(info_hash));
    }
    if peer_given {
        url = url.replace(
            "{peer_id}",
            &String::from_utf8_lossy(&encode_bytes(peer_id)),
        );
    }
    let mut query = vec![
        format!("port={port}"),
        "uploaded=0".to_owned(),
        "downloaded=0".to_owned(),
        format!("left={left}"),
        "compact=1".to_owned(),
    ];
    if !event.is_empty() {
        query.push(format!("event={event}"));
    }
    query.push(format!("numwant={MAX_PEERS}"));
    if !hash_given {
        let mut parameter = b"info_hash=".to_vec();
        parameter.extend_from_slice(&encode_bytes(info_hash));
        query.push(String::from_utf8_lossy(&parameter).into_owned());
    }
    if !peer_given {
        let mut parameter = b"peer_id=".to_vec();
        parameter.extend_from_slice(&encode_bytes(peer_id));
        query.push(String::from_utf8_lossy(&parameter).into_owned());
    }
    // An address that already has a query of its own (the placeholders usually sit in
    // one) is added to; one without gets the first mark.
    url.push(if url.contains('?') { '&' } else { '?' });
    url.push_str(&query.join("&"));
    Ok(url)
}

/// The bytes of a tracker's query as they go on a wire: anything that is not unreserved
/// text is `%` and its byte in hex. Most of what goes in a query is text, and the magnet
/// form's encoder already knows what a wire wants, but a peer id is twenty random bytes
/// and is text hardly ever, so those are encoded here where any byte will do.
fn encode_bytes(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        let mut out = Vec::with_capacity(bytes.len() * 3);
        for byte in bytes {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(*byte)
                }
                _ => out.extend_from_slice(format!("%{byte:02X}").as_bytes()),
            }
        }
        return out;
    };
    magnet::percent_encode(text)
}

/// The 40 lower-case hexadecimal characters of an info hash.
fn hex(info_hash: &[u8; 20]) -> String {
    let mut out = String::with_capacity(40);
    for byte in info_hash {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The peers an answer names, however it chose to name them.
fn answer_peers(node: &Node) -> Vec<SocketAddr> {
    let mut peers = Vec::new();
    let mut seen = HashSet::new();
    // Six bytes each is what is asked for, but a tracker that ignores `compact=1` sends
    // a list of dictionaries instead, and both are read.
    if let Some(raw) = node.get("peers").and_then(|node| node.bytes()) {
        compact(&mut peers, &mut seen, raw);
    } else if let Some(items) = node.get("peers").and_then(|node| node.list()) {
        dictionaries(&mut peers, &mut seen, items);
    }
    if let Some(raw) = node.get("peers6").and_then(|node| node.bytes()) {
        compact6(&mut peers, &mut seen, raw);
    }
    peers
}

/// Adds a peer, unless it has been named already or there is no room for it: a tracker
/// that names one peer a thousand times is not telling us anything new.
fn collect(peers: &mut Vec<SocketAddr>, seen: &mut HashSet<SocketAddr>, peer: SocketAddr) {
    if peers.len() < MAX_PEERS && seen.insert(peer) {
        peers.push(peer);
    }
}

/// Peers as six bytes each: four of address, two of port. Whatever is left over at the
/// end is left alone, since a broken tail is not worth losing the whole answer over.
fn compact(peers: &mut Vec<SocketAddr>, seen: &mut HashSet<SocketAddr>, raw: &[u8]) {
    for entry in raw.chunks_exact(COMPACT_PEER) {
        let address = Ipv4Addr::new(entry[0], entry[1], entry[2], entry[3]);
        let port = u16::from_be_bytes([entry[4], entry[5]]);
        collect(peers, seen, SocketAddr::from((address, port)));
    }
}

/// Peers as eighteen bytes each: sixteen of address, two of port.
fn compact6(peers: &mut Vec<SocketAddr>, seen: &mut HashSet<SocketAddr>, raw: &[u8]) {
    for entry in raw.chunks_exact(COMPACT_PEER6) {
        let mut address = [0u8; 16];
        address.copy_from_slice(&entry[..16]);
        let port = u16::from_be_bytes([entry[16], entry[17]]);
        collect(
            peers,
            seen,
            SocketAddr::from((Ipv6Addr::from(address), port)),
        );
    }
}

/// Peers as a list of dictionaries, each naming an address and a port. One that does not
/// make sense is passed over rather than taken as the whole answer being wrong.
fn dictionaries(peers: &mut Vec<SocketAddr>, seen: &mut HashSet<SocketAddr>, items: &[Node]) {
    for item in items {
        let (Some(address), Some(port)) = (
            item.get("ip").and_then(|node| node.text()),
            item.get("port").and_then(|node| node.int()),
        ) else {
            continue;
        };
        let (Ok(address), Ok(port)) = (address.parse::<IpAddr>(), u16::try_from(port)) else {
            continue;
        };
        collect(peers, seen, SocketAddr::new(address, port));
    }
}

/// How often to come back. Half an hour when the tracker does not say, and never more
/// than a day, however long a tracker claims it will be.
fn interval_of(seconds: Option<i64>) -> Duration {
    let seconds = match seconds {
        Some(seconds) if seconds > 0 => seconds.min(MAX_INTERVAL),
        _ => DEFAULT_INTERVAL,
    };
    Duration::from_secs(seconds as u64)
}

/// What BEP 15 makes of an event: a downloader that has begun, one that is finished, and
/// one that has stopped (so the tracker can let it go).
fn event_code(event: &str) -> u32 {
    match event {
        "started" => 2,
        "completed" => 1,
        "stopped" => 3,
        _ => 0,
    }
}

/// The host and port of a tracker address, without its scheme and without whatever a
/// path says: a UDP tracker is a host and a port, and the rest of the address is only
/// there for the trackers that answer over HTTP.
fn host_and_port(tracker: &str) -> Result<(String, u16), String> {
    let after_scheme = match tracker.find("://") {
        Some(at) => &tracker[at + 3..],
        None => tracker,
    };
    let rest = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let (host, port) = if let Some(end) = rest.rfind(']') {
        // An IPv6 address is written in brackets, so the port is the only thing after the
        // closing one.
        (
            rest[..end].trim_start_matches('[').to_owned(),
            rest[end + 1..].trim_start_matches(':'),
        )
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), port),
            None => return Err(format!("{tracker}: no port in the address")),
        }
    };
    if host.is_empty() {
        return Err(format!("{tracker}: no host in the address"));
    }
    let port = port
        .parse()
        .map_err(|_| format!("{tracker}: the port is not a number"))?;
    Ok((host, port))
}

/// Where a tracker's answers come from: the first address its name leads to, of the kind
/// the scheme asked for, since `udp4` will not do with an IPv6 address nor the reverse.
fn resolve(host: &str, port: u16, scheme: &str) -> Result<SocketAddr, String> {
    let wanted = |address: &SocketAddr| match scheme {
        "udp4" => address.is_ipv4(),
        "udp6" => address.is_ipv6(),
        _ => true,
    };
    (host, port)
        .to_socket_addrs()
        .map_err(|error| format!("{host}:{port}: {error}"))?
        .filter(|address| wanted(address))
        .next()
        .ok_or_else(|| format!("{host}:{port}: no address of the kind that was asked for"))
}

/// Where to open the socket from: an address of the same kind as the tracker's, so
/// nothing has to be translated between them.
fn bind_for(address: &SocketAddr) -> SocketAddr {
    if address.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 0], 0))
    }
}

/// One answer, or why there was none.
fn read(socket: &UdpSocket, tracker: &str) -> Result<Vec<u8>, String> {
    // A UDP answer cannot be longer than this, so nothing is ever cut short.
    let mut buffer = vec![0u8; 65_536];
    let count = socket
        .recv(&mut buffer)
        .map_err(|error| format!("{tracker}: no answer came: {error}"))?;
    buffer.truncate(count);
    Ok(buffer)
}

/// A random number to mark one request with, so an answer that is not for it (one that
/// came late, or one sent by something else) is not read as if it were. The clock is no
/// good as randomness, but it is better than a number that never changes.
fn transaction_id() -> u32 {
    let mut four = [0u8; 4];
    if getrandom::getrandom(&mut four).is_ok() {
        return u32::from_be_bytes(four);
    }
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (since.as_secs() as u32) ^ (since.subsec_nanos() << 8)
}

/// The four bytes at the front of `bytes` as a number, or zero when they are not there.
fn be32(bytes: &[u8]) -> u32 {
    let mut four = [0u8; 4];
    four.copy_from_slice(&bytes[..bytes.len().min(4)]);
    u32::from_be_bytes(four)
}

/// The eight bytes at the front of `bytes` as a number, or zero when they are not there.
fn be64(bytes: &[u8]) -> u64 {
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&bytes[..bytes.len().min(8)]);
    u64::from_be_bytes(eight)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, UdpSocket};
    use std::sync::mpsc::{channel, Receiver};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::*;

    const CONNECTION: u64 = 0x0102_0304_0506_0708;
    const INFO_HASH: [u8; 20] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef, 0x01, 0x23, 0x45, 0x67,
    ];
    const PEER_ID: [u8; 20] = *b"-ST0001-abcdefghijkl";

    fn hash() -> [u8; 20] {
        INFO_HASH
    }

    fn peer() -> [u8; 20] {
        PEER_ID
    }

    /// Just enough bencode to build a tracker's answer with.
    fn text(value: &str) -> Vec<u8> {
        byte_string(value.as_bytes())
    }

    fn byte_string(bytes: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", bytes.len()).into_bytes();
        out.extend_from_slice(bytes);
        out
    }

    fn integer(value: i64) -> Vec<u8> {
        format!("i{value}e").into_bytes()
    }

    fn list(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"l".to_vec();
        for item in items {
            out.extend_from_slice(item);
        }
        out.push(b'e');
        out
    }

    fn dictionary(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"d".to_vec();
        for (key, value) in entries {
            out.extend_from_slice(&text(key));
            out.extend_from_slice(value);
        }
        out.push(b'e');
        out
    }

    /// One peer as six bytes.
    fn compact_one(address: [u8; 4], port: u16) -> Vec<u8> {
        let mut out = address.to_vec();
        out.extend_from_slice(&port.to_be_bytes());
        out
    }

    /// A tracker on loopback that answers with `body` and hands back the request line of
    /// the first request it saw. It keeps answering for a while, and stops waiting after
    /// one, so a test that never gets there cannot hold the run up and nothing else on
    /// the loopback can take its place.
    fn http_tracker(body: Vec<u8>) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let (sender, seen) = channel();
        std::thread::spawn(move || {
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            let until = Instant::now() + Duration::from_secs(20);
            let mut said = false;
            while Instant::now() < until {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                let mut stream = stream;
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut head = Vec::new();
                let mut chunk = [0u8; 2048];
                while head.len() < 16 * 1024 && !head.windows(4).any(|at| at == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => head.extend_from_slice(&chunk[..count]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                if !said {
                    said = true;
                    let _ = sender.send(head.lines().next().unwrap_or_default().to_owned());
                }
                let mut answer = reply.clone();
                answer.extend_from_slice(&body);
                let _ = stream.write_all(&answer);
                let _ = stream.flush();
            }
        });
        (host, seen)
    }

    /// The line the fake HTTP tracker saw.
    fn line_of(seen: &Receiver<String>) -> String {
        seen.recv_timeout(Duration::from_secs(20)).unwrap()
    }

    /// Waits for a packet of exactly `length` bytes, passing over anything else that
    /// turns up: what the other tests send on the loopback is not this exchange's
    /// business, and a datagram that is the wrong length is not the one being waited for.
    fn wait_for(
        socket: &UdpSocket,
        buffer: &mut [u8],
        length: usize,
    ) -> Option<(SocketAddr, Vec<u8>)> {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            socket
                .set_read_timeout(Some(Duration::from_millis(200)))
                .ok()?;
            if let Ok((count, from)) = socket.recv_from(buffer) {
                if count == length {
                    return Some((from, buffer[..count].to_vec()));
                }
            }
        }
        None
    }

    /// A UDP tracker in a thread: it answers the connect request with `transaction` for
    /// the request's own id (or with a wrong one, to see that it is caught), and then
    /// hands the announce on to `answer`, which writes what the client should get back.
    fn udp_tracker<F>(connection: u64, wrong_id: bool, answer: F) -> (String, JoinHandle<()>)
    where
        F: FnOnce(&[u8], u32) -> Vec<u8> + Send + 'static,
    {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", server.local_addr().unwrap().port());
        let handle = std::thread::spawn(move || {
            let mut buffer = [0u8; 1024];
            let Some((from, packet)) = wait_for(&server, &mut buffer, 16) else {
                return;
            };
            assert_eq!(be64(&packet[0..8]), MAGIC, "the magic opens every exchange");
            assert_eq!(be32(&packet[8..12]), 0, "the first action is a connection");
            let transaction = be32(&packet[12..16]);
            // The answer to a connection keeps the request's shape: the action, then the
            // id of the request it answers, then the connection id (BEP 15).
            let mut reply = Vec::with_capacity(16);
            reply.extend_from_slice(&0u32.to_be_bytes());
            let answered = if wrong_id {
                transaction ^ 1
            } else {
                transaction
            };
            reply.extend_from_slice(&answered.to_be_bytes());
            reply.extend_from_slice(&connection.to_be_bytes());
            server.send_to(&reply, from).unwrap();

            let Some((from, packet)) = wait_for(&server, &mut buffer, 98) else {
                return;
            };
            assert_eq!(
                be64(&packet[0..8]),
                connection,
                "the connection id is carried on"
            );
            assert_eq!(be32(&packet[8..12]), 1, "the second action is an announce");
            let transaction = be32(&packet[12..16]);
            assert_eq!(
                &packet[16..36],
                &INFO_HASH[..],
                "the hash is where it belongs"
            );
            assert_eq!(
                &packet[36..56],
                &PEER_ID[..],
                "the peer id is where it belongs"
            );
            assert_eq!(be64(&packet[56..64]), 0, "nothing has been downloaded yet");
            assert_eq!(be64(&packet[64..72]), 0, "and nothing is left");
            assert_eq!(be64(&packet[72..80]), 0, "and nothing has been given");
            assert_eq!(be32(&packet[80..84]), 2, "the event is that it started");
            assert_eq!(
                &packet[84..88],
                &[0, 0, 0, 0],
                "the address is left to the tracker"
            );
            assert_eq!(
                &packet[92..96],
                &(-1i32 as u32).to_be_bytes(),
                "as many as it names"
            );
            assert_eq!(
                &packet[96..98],
                &51413u16.to_be_bytes(),
                "the port is the client's"
            );
            let mut reply = Vec::new();
            reply.extend_from_slice(&1u32.to_be_bytes());
            reply.extend_from_slice(&transaction.to_be_bytes());
            reply.extend_from_slice(&answer(&packet, transaction));
            server.send_to(&reply, from).unwrap();
        });
        (host, handle)
    }

    #[test]
    fn udp_announce_reads_the_peers_and_caps_the_interval() {
        let (host, server) = udp_tracker(CONNECTION, false, |_, _| {
            let mut reply = Vec::new();
            // An interval no sane tracker would send: a day is as much as is believed.
            reply.extend_from_slice(&999_999u32.to_be_bytes());
            reply.extend_from_slice(&3u32.to_be_bytes()); // leechers
            reply.extend_from_slice(&9u32.to_be_bytes()); // seeders
            reply.extend_from_slice(&compact_one([10, 0, 0, 1], 51414));
            reply.extend_from_slice(&compact_one([192, 168, 1, 7], 6881));
            reply
        });
        let tracker = format!("udp://{host}");
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "started",
            Duration::from_secs(10),
        );
        server.join().unwrap();
        let answer = answer.unwrap();
        assert_eq!(
            answer.peers,
            vec![
                SocketAddr::from(([10, 0, 0, 1], 51414)),
                SocketAddr::from(([192, 168, 1, 7], 6881)),
            ]
        );
        assert_eq!(answer.interval, Duration::from_secs(24 * 60 * 60));
    }

    #[test]
    fn an_answer_for_another_request_is_an_error() {
        let (host, server) = udp_tracker(CONNECTION, true, |_, _| Vec::new());
        let tracker = format!("udp://{host}");
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_secs(10),
        );
        server.join().unwrap();
        let error = answer.unwrap_err();
        assert!(
            error.contains(&tracker),
            "the error names the tracker: {error}"
        );
        assert!(error.contains("another request"), "{error}");
    }

    #[test]
    fn a_tracker_that_answers_nothing_is_given_up_on() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tracker = format!("udp://127.0.0.1:{}", server.local_addr().unwrap().port());
        // The tracker hears nothing and says nothing for longer than the client waits.
        let quiet = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            drop(server);
        });
        let asked = Instant::now();
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_millis(300),
        );
        let taken = asked.elapsed();
        quiet.join().unwrap();
        let error = answer.unwrap_err();
        assert!(
            error.contains(&tracker),
            "the error names the tracker: {error}"
        );
        assert!(
            taken < Duration::from_secs(10),
            "it did not wait: {taken:?}"
        );
    }

    #[test]
    fn http_announce_asks_compact_and_reads_the_peers() {
        let mut peers = compact_one([10, 1, 2, 3], 51414);
        peers.extend_from_slice(&compact_one([192, 168, 0, 9], 6881));
        let body = dictionary(&[("interval", integer(1800)), ("peers", byte_string(&peers))]);
        let (host, seen) = http_tracker(body);
        let tracker = format!("http://{host}/announce");
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_secs(10),
        )
        .unwrap();
        let line = line_of(&seen);
        assert!(line.starts_with("GET /announce?"), "{line}");
        assert!(line.contains("info_hash="), "{line}");
        assert!(line.contains("peer_id="), "{line}");
        assert!(line.contains("compact=1"), "{line}");
        assert!(line.contains(&format!("numwant={MAX_PEERS}")), "{line}");
        assert!(line.contains("port=51413"), "{line}");
        assert!(
            answer.peers
                == vec![
                    SocketAddr::from(([10, 1, 2, 3], 51414)),
                    SocketAddr::from(([192, 168, 0, 9], 6881)),
                ],
            "{:?}",
            answer.peers
        );
        assert_eq!(answer.interval, Duration::from_secs(1800));
    }

    #[test]
    fn http_reads_peers_named_as_dictionaries() {
        let mut one = b"d".to_vec();
        one.extend_from_slice(&text("ip"));
        one.extend_from_slice(&text("127.0.0.1"));
        one.extend_from_slice(&text("peer id"));
        one.extend_from_slice(&text("-ST0001-a"));
        one.extend_from_slice(&text("port"));
        one.extend_from_slice(&integer(6881));
        one.push(b'e');
        // A second peer that makes no sense, which is passed over rather than believed.
        let mut broken = b"d".to_vec();
        broken.extend_from_slice(&text("ip"));
        broken.extend_from_slice(&text("not an address"));
        broken.extend_from_slice(&text("port"));
        broken.extend_from_slice(&integer(1));
        broken.push(b'e');
        let body = dictionary(&[("interval", integer(900)), ("peers", list(&[one, broken]))]);
        let (host, seen) = http_tracker(body);
        let tracker = format!("http://{host}/announce");
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "started",
            Duration::from_secs(10),
        )
        .unwrap();
        let line = line_of(&seen);
        assert!(line.contains("event=started"), "{line}");
        assert_eq!(answer.peers, vec![SocketAddr::from(([127, 0, 0, 1], 6881))]);
        assert_eq!(answer.interval, Duration::from_secs(900));
    }

    #[test]
    fn a_failure_reason_is_an_error() {
        let body = dictionary(&[("failure reason", text("torrent not registered"))]);
        let (host, _seen) = http_tracker(body);
        let tracker = format!("http://{host}/announce");
        let error = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(
            error.contains(&tracker),
            "the error names the tracker: {error}"
        );
        assert!(error.contains("torrent not registered"), "{error}");
    }

    #[test]
    fn a_broken_tail_of_peers_does_not_lose_the_rest() {
        let mut peers = compact_one([10, 0, 0, 7], 51414);
        peers.extend_from_slice(&compact_one([10, 0, 0, 8], 51415));
        peers.extend_from_slice(&[1, 2, 3]); // a tail that is not a whole peer
        let body = dictionary(&[("interval", integer(60)), ("peers", byte_string(&peers))]);
        let (host, _seen) = http_tracker(body);
        let tracker = format!("http://{host}/announce");
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(
            answer.peers,
            vec![
                SocketAddr::from(([10, 0, 0, 7], 51414)),
                SocketAddr::from(([10, 0, 0, 8], 51415)),
            ]
        );
    }

    #[test]
    fn only_the_first_max_peers_peers_are_kept() {
        let mut peers = Vec::new();
        for count in 0..(MAX_PEERS as u16 + 50) {
            peers.extend_from_slice(&compact_one(
                [10, 0, (count / 256) as u8, count as u8],
                6881,
            ));
        }
        // The same peer twice, which is only counted once.
        peers.extend_from_slice(&compact_one([10, 0, 0, 0], 6881));
        let body = dictionary(&[("interval", integer(1800)), ("peers", byte_string(&peers))]);
        let (host, _seen) = http_tracker(body);
        let tracker = format!("http://{host}/announce");
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(answer.peers.len(), MAX_PEERS);
        let mut unique: Vec<SocketAddr> = Vec::new();
        for peer in answer.peers {
            assert!(!unique.contains(&peer), "no peer is named twice");
            unique.push(peer);
        }
    }

    #[test]
    fn ipv6_peers_are_read_from_peers6() {
        let mut six = Vec::new();
        six.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        six.extend_from_slice(&6881u16.to_be_bytes());
        six.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        six.extend_from_slice(&6969u16.to_be_bytes());
        let body = dictionary(&[("interval", integer(1800)), ("peers6", byte_string(&six))]);
        let (host, _seen) = http_tracker(body);
        let tracker = format!("http://{host}/announce");
        let answer = announce(
            &tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(
            answer.peers,
            vec![
                SocketAddr::from((Ipv6Addr::LOCALHOST, 6881)),
                SocketAddr::from(("2001:db8::1".parse::<Ipv6Addr>().unwrap(), 6969)),
            ]
        );
    }

    #[test]
    fn the_placeholders_in_an_address_are_filled_in() {
        // A peer id that is not text at all, so it has to survive the encoding.
        let mut binary = vec![0xff, 0xfe, 0x00];
        binary.extend_from_slice(b"ABC-ST0001-abcdef");
        let peer_id: [u8; 20] = binary[..20].try_into().unwrap();
        let body = dictionary(&[("interval", integer(1800)), ("peers", byte_string(&[]))]);
        let (host, seen) = http_tracker(body);
        let tracker = format!("http://{host}/announce?info_hash={{info_hash}}&peer_id={{peer_id}}");
        announce(
            &tracker,
            &hash(),
            &peer_id,
            51413,
            0,
            "",
            Duration::from_secs(10),
        )
        .unwrap();
        let line = line_of(&seen);
        assert!(
            line.contains("info_hash=0123456789abcdef0123456789abcdef01234567"),
            "{line}"
        );
        assert!(
            line.contains("peer_id=%FF%FE%00ABC-ST0001-abcdef"),
            "{line}"
        );
        // The two the address named are not added a second time.
        assert_eq!(line.matches("info_hash=").count(), 1, "{line}");
        assert_eq!(line.matches("peer_id=").count(), 1, "{line}");
    }

    #[test]
    fn an_address_asking_for_our_ip_is_an_error() {
        let error = announce(
            "http://tracker.example/announce?ip={ip}",
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.contains("tracker.example"), "{error}");
    }

    #[test]
    fn compact_peers_writes_an_address_and_a_port() {
        let peers = [
            SocketAddr::from(([10, 0, 0, 1], 51414)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, 6881)),
        ];
        let mut expected = Vec::new();
        expected.extend_from_slice(&[10, 0, 0, 1]);
        expected.extend_from_slice(&51414u16.to_be_bytes());
        expected.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        expected.extend_from_slice(&6881u16.to_be_bytes());
        assert_eq!(compact_peers(&peers), expected);
        assert!(compact_peers(&[]).is_empty());
    }

    #[test]
    fn an_address_that_is_not_a_tracker_is_an_error() {
        for tracker in [
            "wss://tracker.example/announce",
            "tracker.example/announce",
            "udp:/tracker.example:1337",
            "",
        ] {
            let error = announce(
                tracker,
                &hash(),
                &peer(),
                51413,
                0,
                "",
                Duration::from_millis(300),
            )
            .unwrap_err();
            assert!(error.contains("tracker address"), "{tracker}: {error}");
        }
    }

    #[test]
    fn a_tracker_that_cannot_be_reached_is_an_error() {
        // An address on the network reserved for writing examples (RFC 5737): no machine
        // this runs on has anything there, and nothing on the loopback can take it, which
        // a port that was let go of a moment ago cannot promise.
        let udp_tracker = "udp://192.0.2.1:451";
        let asked = Instant::now();
        let error = announce(
            udp_tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.contains(udp_tracker), "{error}");
        assert!(asked.elapsed() < Duration::from_secs(10), "it did not wait");

        let http_tracker = "http://192.0.2.1:8080/announce";
        let asked = Instant::now();
        let error = announce(
            http_tracker,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.contains(http_tracker), "{error}");
        assert!(asked.elapsed() < Duration::from_secs(10), "it did not wait");

        // A tracker on the loopback that is there but says nothing, which is what a
        // firewall looks like from here. The port is held for the whole call, so no other
        // test can be standing where this one expects nothing to be.
        let quiet = TcpListener::bind("127.0.0.1:0").unwrap();
        let hung = format!(
            "http://127.0.0.1:{}/announce",
            quiet.local_addr().unwrap().port()
        );
        let asked = Instant::now();
        let error = announce(
            &hung,
            &hash(),
            &peer(),
            51413,
            0,
            "",
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.contains(&hung), "{error}");
        assert!(asked.elapsed() < Duration::from_secs(10), "it did not wait");
        drop(quiet);
    }

    #[test]
    fn host_and_port_splits_the_usual_forms() {
        assert_eq!(
            host_and_port("udp://tracker.example:1337/announce").unwrap(),
            ("tracker.example".to_owned(), 1337)
        );
        assert_eq!(
            host_and_port("udp4://tracker.example:80").unwrap(),
            ("tracker.example".to_owned(), 80)
        );
        assert_eq!(
            host_and_port("udp://[::1]:451").unwrap(),
            ("::1".to_owned(), 451)
        );
        assert!(host_and_port("udp://tracker.example").is_err());
        assert!(host_and_port("udp://:1337").is_err());
    }
}
