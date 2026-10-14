//! What two peers say to each other once they have greeted: the core protocol (BEP 3),
//! the port message (BEP 11's cousin, BEP 5), the fast extension (BEP 6), and the two
//! extensions this client uses — `ut_metadata` for a torrent's description (BEP 9) and
//! `ut_pex` for peers a peer already knows (BEP 11).

use std::io::{ErrorKind, Read, Write};

use super::bencode::{self, Node, Value};

/// The greeting: 19 bytes of protocol name, eight reserved, the torrent's hash, and the
/// peer's id.
pub const HANDSHAKE_LEN: usize = 68;

/// The most a message may be. A piece message holds one block, so this is generous.
pub const MAX_MESSAGE: usize = 1 << 20;

/// How large a piece of a torrent's description is asked for and sent in (BEP 9).
pub const METADATA_CHUNK: usize = 16 * 1024;

/// What every greeting opens with (BEP 3). A peer that says anything else is not one.
const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";

/// The greeting, kept for its two hashes.
#[derive(Clone, Debug, PartialEq)]
pub struct Handshake {
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

/// The bit in the greeting's reserved bytes that says the peer sends extended messages
/// (BEP 10). Without it a peer is free to answer as if it had none.
const EXTENDED: u8 = 0x10;

impl Handshake {
    /// A greeting for this client: the peer extension marked (BEP 10, which is how a peer
    /// is told to send its extensions and its torrent's description), and the torrent's
    /// hash and this client's id.
    pub fn new(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Self {
        let mut reserved = [0u8; 8];
        reserved[5] = EXTENDED;
        Self {
            reserved,
            info_hash: *info_hash,
            peer_id: *peer_id,
        }
    }

    /// The 68 bytes to send.
    pub fn bytes(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        out[..PROTOCOL.len()].copy_from_slice(PROTOCOL);
        out[19..27].copy_from_slice(&self.reserved);
        out[27..47].copy_from_slice(&self.info_hash);
        out[47..67].copy_from_slice(&self.peer_id);
        out
    }

    /// Reads a greeting. Fails when it is not one, which is how a peer that speaks
    /// something else (or nothing) is caught. Anything after the 68 bytes is left to
    /// whoever reads next, and the reserved bytes are kept as they arrived: other
    /// extensions (BEP 6's fast set, the DHT's) put their marks there, and refusing a
    /// greeting over them would refuse a peer that can still serve us.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < HANDSHAKE_LEN {
            return Err(format!(
                "a greeting is {HANDSHAKE_LEN} bytes, and only {} arrived",
                bytes.len()
            ));
        }
        if &bytes[..PROTOCOL.len()] != PROTOCOL {
            return Err("the peer did not start with the BitTorrent protocol name".to_owned());
        }
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&bytes[19..27]);
        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&bytes[27..47]);
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&bytes[47..67]);
        Ok(Self {
            reserved,
            info_hash,
            peer_id,
        })
    }
}

/// One message from a peer. A peer may send anything, so what is not known is kept rather
/// than refused.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request { piece: u32, begin: u32, length: u32 },
    Cancel { piece: u32, begin: u32, length: u32 },
    Piece { piece: u32, begin: u32, data: Vec<u8> },
    /// A peer's listening port (BEP 5), which shares its id with an extended message
    /// and is told apart from one by the two bytes that follow it.
    Port(u16),
    /// An extended message: its extension's id and the extension's own bytes.
    Extended { id: u8, payload: Vec<u8> },
    Unknown { id: u8, payload: Vec<u8> },
}

impl Message {
    /// The message's bytes, without the length that goes in front of them. A keep-alive
    /// is that length and nothing else: it has no id byte of its own.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Message::KeepAlive => {}
            Message::Choke => out.push(1),
            Message::Unchoke => out.push(2),
            Message::Interested => out.push(3),
            Message::NotInterested => out.push(4),
            Message::Have(piece) => {
                out.push(5);
                out.extend_from_slice(&piece.to_be_bytes());
            }
            Message::Bitfield(bits) => {
                out.push(6);
                out.extend_from_slice(bits);
            }
            Message::Request { piece, begin, length } => {
                out.push(7);
                out.extend_from_slice(&piece.to_be_bytes());
                out.extend_from_slice(&begin.to_be_bytes());
                out.extend_from_slice(&length.to_be_bytes());
            }
            Message::Piece { piece, begin, data } => {
                out.push(8);
                out.extend_from_slice(&piece.to_be_bytes());
                out.extend_from_slice(&begin.to_be_bytes());
                out.extend_from_slice(data);
            }
            Message::Cancel { piece, begin, length } => {
                out.push(9);
                out.extend_from_slice(&piece.to_be_bytes());
                out.extend_from_slice(&begin.to_be_bytes());
                out.extend_from_slice(&length.to_be_bytes());
            }
            Message::Port(port) => {
                out.push(20);
                out.extend_from_slice(&port.to_be_bytes());
            }
            // The extension's id and then the extension's own message (BEP 10).
            Message::Extended { id, payload } => {
                out.push(20);
                out.push(*id);
                out.extend_from_slice(payload);
            }
            Message::Unknown { id, payload } => {
                out.push(*id);
                out.extend_from_slice(payload);
            }
        }
        out
    }
}

/// Turns what arrived into a message. An id this does not know, and a known id whose
/// payload is too short to hold what it should, are both kept as `Unknown`: the framing
/// said how long the message was, so what is in it is what the peer meant to say, and
/// guessing at it would be worse than handing it over.
fn decode(body: &[u8]) -> Message {
    let Some((id, payload)) = body.split_first() else {
        return Message::KeepAlive;
    };
    let (id, payload) = (*id, payload);
    match id {
        0 => Message::KeepAlive,
        1 if payload.is_empty() => Message::Choke,
        2 if payload.is_empty() => Message::Unchoke,
        3 if payload.is_empty() => Message::Interested,
        4 if payload.is_empty() => Message::NotInterested,
        5 => match read_u32(payload) {
            Some(piece) => Message::Have(piece),
            None => Message::Unknown { id, payload: payload.to_vec() },
        },
        6 => Message::Bitfield(payload.to_vec()),
        7 if payload.len() >= 12 => Message::Request {
            piece: read_u32(payload).unwrap_or(0),
            begin: read_u32(&payload[4..8]).unwrap_or(0),
            length: read_u32(&payload[8..12]).unwrap_or(0),
        },
        8 if payload.len() >= 8 => Message::Piece {
            piece: read_u32(payload).unwrap_or(0),
            begin: read_u32(&payload[4..8]).unwrap_or(0),
            data: payload[8..].to_vec(),
        },
        9 if payload.len() >= 12 => Message::Cancel {
            piece: read_u32(payload).unwrap_or(0),
            begin: read_u32(&payload[4..8]).unwrap_or(0),
            length: read_u32(&payload[8..12]).unwrap_or(0),
        },
        20 => match payload {
            // A port (BEP 5) and an extension (BEP 10) share this id, and the wire says
            // nothing more about which is which: two bytes behind it are a port, and
            // anything else is an extension, whose own id comes first.
            [high, low] => Message::Port(u16::from_be_bytes([*high, *low])),
            _ => match payload.split_first() {
                Some((extension, body)) => Message::Extended {
                    id: *extension,
                    payload: body.to_vec(),
                },
                None => Message::Unknown { id, payload: Vec::new() },
            },
        },
        _ => Message::Unknown { id, payload: payload.to_vec() },
    }
}

/// A big-endian number out of the first four bytes, or nothing when there are not four
/// there. Anything after them is left alone, so this reads a piece header out of the
/// front of a piece's data as well.
fn read_u32(bytes: &[u8]) -> Option<u32> {
    let [a, b, c, d] = bytes.get(..4)? else {
        return None;
    };
    Some(u32::from_be_bytes([*a, *b, *c, *d]))
}

/// Writes one message, its length first (BEP 3).
pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> std::io::Result<()> {
    let bytes = message.encode();
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)
}

/// Reads one message, waiting for as much of it as it needs. `Ok(None)` at the end of the
/// connection, and an error when what arrived is not a message this can read.
pub fn read_message<R: Read>(reader: &mut R) -> std::io::Result<Option<Message>> {
    let mut prefix = [0u8; 4];
    let mut read = 0;
    while read < prefix.len() {
        match reader.read(&mut prefix[read..]) {
            // Nothing at all where a length should be: the peer has closed the connection.
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => return Err(ended("a message's length")),
            Ok(count) => read += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 {
        return Ok(Some(Message::KeepAlive));
    }
    // The length is the peer's claim, so it is checked before anything is set aside for
    // it: a peer must not be able to make this client hold a buffer it never fills.
    if length > MAX_MESSAGE {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("a peer sent {length} bytes in one message, over the {MAX_MESSAGE} byte limit"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(decode(&body)))
}

/// The connection ended in the middle of something that had said it would arrive.
fn ended(what: &str) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::UnexpectedEof,
        format!("the connection ended inside {what}"),
    )
}

/// Somewhere a bit per piece is kept. The session may know how many pieces there are and
/// hold a buffer of exactly that many bytes, or hold a vector that grows as pieces are
/// heard of, so both are taken rather than leaving the caller to resize by hand.
pub trait Bitfield {
    /// The byte holding the bit `index`, if there is one: a vector makes room for it,
    /// a fixed buffer has none past its end and says so.
    fn slot(&mut self, index: usize) -> Option<&mut u8>;
}

impl Bitfield for Vec<u8> {
    /// Pieces arrive in no order, so a bit may be set well past what has been said before.
    /// Only the end moves, and only outwards: room already given is kept.
    fn slot(&mut self, index: usize) -> Option<&mut u8> {
        if index >= self.len() {
            self.resize(index + 1, 0);
        }
        self.get_mut(index)
    }
}

impl Bitfield for [u8] {
    fn slot(&mut self, index: usize) -> Option<&mut u8> {
        self.get_mut(index)
    }
}

impl<const N: usize> Bitfield for [u8; N] {
    fn slot(&mut self, index: usize) -> Option<&mut u8> {
        self.get_mut(index)
    }
}

/// Whether a peer's bitfield message says it holds a piece. A piece past the end of the
/// bitfield is one the peer has not claimed, not a reason to stop reading.
pub fn bitfield_has(bitfield: &[u8], piece: u32) -> bool {
    let byte = (piece / 8) as usize;
    match bitfield.get(byte) {
        Some(byte) => byte & (0x80 >> (piece % 8)) != 0,
        None => false,
    }
}

/// Says a piece is held, in a peer's bitfield. Piece `p` is byte `p / 8`, and bit `p % 8`
/// counted from that byte's most significant end (BEP 3).
pub fn bitfield_set<B: Bitfield>(bitfield: &mut B, piece: u32) {
    if let Some(byte) = bitfield.slot((piece / 8) as usize) {
        *byte |= 0x80 >> (piece % 8);
    }
}

/// The extended handshake (BEP 10): which extensions a peer has, and, when it has this
/// torrent's description, how large it is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtendedHandshake {
    pub ut_metadata: Option<u8>,
    pub ut_pex: Option<u8>,
    pub metadata_size: Option<u64>,
    pub client: Option<String>,
}

impl ExtendedHandshake {
    /// Reads the bencoded part of an extended handshake (the bytes after the extension's
    /// id). An empty payload means an old peer that sent nothing: all its extensions are
    /// then unknown.
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        if payload.is_empty() {
            return Ok(Self::default());
        }
        let node = bencode::parse(payload, payload.len())?;
        if node.dict().is_none() {
            return Err("the extended handshake is not a dictionary".to_owned());
        }
        // What a peer says it has is what matters, so a "m" that is not a dictionary of
        // names is left alone rather than taken as the end of the handshake.
        let mut handshake = Self::default();
        if let Some(extensions) = node.get("m").and_then(Node::dict) {
            for (name, id) in extensions {
                let Some(id) = id.int().and_then(|id| u8::try_from(id).ok()) else {
                    continue;
                };
                if name == b"ut_metadata" {
                    handshake.ut_metadata = Some(id);
                } else if name == b"ut_pex" {
                    handshake.ut_pex = Some(id);
                }
            }
        }
        // The size is only about something that can be fetched: a peer that does not
        // have `ut_metadata` has no description of its own to measure.
        if handshake.ut_metadata.is_some() {
            handshake.metadata_size = node
                .get("metadata_size")
                .and_then(Node::int)
                .and_then(|size| u64::try_from(size).ok());
        }
        handshake.client = node.get("v").and_then(Node::text).map(str::to_owned);
        Ok(handshake)
    }

    /// The bencoded part to send.
    pub fn payload(&self) -> Vec<u8> {
        let mut extensions = Vec::new();
        if let Some(id) = self.ut_metadata {
            extensions.push((b"ut_metadata".to_vec(), node(Value::Int(id.into()))));
        }
        if let Some(id) = self.ut_pex {
            extensions.push((b"ut_pex".to_vec(), node(Value::Int(id.into()))));
        }
        let mut dictionary = vec![(b"m".to_vec(), node(Value::Dict(extensions)))];
        if let Some(size) = self.metadata_size {
            dictionary.push((b"metadata_size".to_vec(), node(Value::Int(size as i64))));
        }
        if let Some(client) = &self.client {
            dictionary.push((b"v".to_vec(), node(Value::Bytes(client.as_bytes().to_vec()))));
        }
        bencode::encode(&Value::Dict(dictionary))
    }

    /// The whole extended message, with `id` as the extension's id.
    pub fn message(&self, id: u8) -> Message {
        Message::Extended {
            id,
            payload: self.payload(),
        }
    }
}

/// A value for an encoded dictionary. Where it sits in the bytes is left at nothing,
/// which is only ever asked of a value that was read, never of one being written.
fn node(value: Value) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

/// The three `ut_metadata` messages (BEP 9).
#[derive(Clone, Debug, PartialEq)]
pub enum MetadataMessage {
    Request { piece: u32 },
    Data { piece: u32, /// How large the whole description is, which the first part says.
           total: Option<u64>, data: Vec<u8> },
    Reject { piece: u32 },
}

impl MetadataMessage {
    /// Reads the bytes after the extension's id. BEP 9 marks an extension's own message
    /// with an `e`, which is read here; a peer that leaves it out is read as well, since
    /// the dictionary is what follows either way.
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        let payload = payload.strip_prefix(b"e").unwrap_or(payload);
        let node = bencode::parse(payload, payload.len())?;
        if node.dict().is_none() {
            return Err("a ut_metadata message is not a dictionary".to_owned());
        }
        let kind = node
            .get("msg_type")
            .and_then(Node::int)
            .ok_or("a ut_metadata message says nothing about what it is")?;
        // Every one of the three needs the piece, and none of them is worth acting on
        // without it: it is what says which part of the description is being talked about.
        let piece = |node: &Node| {
            node.get("piece")
                .and_then(Node::int)
                .and_then(|piece| u32::try_from(piece).ok())
                .ok_or_else(|| "a ut_metadata message names no piece".to_owned())
        };
        match kind {
            0 => Ok(Self::Request { piece: piece(&node)? }),
            1 => {
                let data = node
                    .get("data")
                    .and_then(Node::bytes)
                    .ok_or("a ut_metadata data message carries no data")?;
                Ok(Self::Data {
                    piece: piece(&node)?,
                    // Only the first part says how large the whole description is; the
                    // rest leave it out, and one that sends it anyway is taken at its word.
                    total: node
                        .get("total_size")
                        .and_then(Node::int)
                        .and_then(|total| u64::try_from(total).ok()),
                    data: data.to_vec(),
                })
            }
            2 => Ok(Self::Reject { piece: piece(&node)? }),
            other => Err(format!("a ut_metadata message of type {other} is not one of the three")),
        }
    }

    /// The whole extended message, with `id` as the extension's id. The `e` in front is
    /// BEP 9's mark of an extension's own message, so a peer can tell it from the rest.
    pub fn message(&self, id: u8) -> Message {
        let mut dictionary: Vec<(Vec<u8>, Node)> = Vec::new();
        let mut entry = |key: &str, value: Value| {
            dictionary.push((key.as_bytes().to_vec(), node(value)))
        };
        match self {
            Self::Request { piece } => {
                entry("msg_type", Value::Int(0));
                entry("piece", Value::Int(*piece as i64));
            }
            Self::Data { piece, total, data } => {
                entry("msg_type", Value::Int(1));
                entry("piece", Value::Int(*piece as i64));
                // Every part but the first is sent without the size: only the first one
                // carries it, and repeating it would say the same thing over and over.
                if let Some(total) = total {
                    entry("total_size", Value::Int(*total as i64));
                }
                entry("data", Value::Bytes(data.clone()));
            }
            Self::Reject { piece } => {
                entry("msg_type", Value::Int(2));
                entry("piece", Value::Int(*piece as i64));
            }
        }
        let mut payload = b"e".to_vec();
        payload.extend(bencode::encode(&Value::Dict(dictionary)));
        Message::Extended { id, payload }
    }
}

/// The peers a `ut_pex` "added" message carries (BEP 11): each as an address and a port,
/// in the compact form trackers use.
pub fn parse_added_peers(payload: &[u8]) -> Result<Vec<[u8; 6]>, String> {
    let node = bencode::parse(payload, payload.len())?;
    // "added.f" (the flags of each peer) is not needed here, and a peer that leaves it
    // out is the same as one that sends it.
    let added = node
        .get("added")
        .and_then(Node::bytes)
        .ok_or("a ut_pex added message names no peers")?;
    if added.len() % 6 != 0 {
        return Err(format!(
            "the compact peers are {} bytes, which is not a whole number of them",
            added.len()
        ));
    }
    Ok(added
        .chunks_exact(6)
        .map(|peer| {
            let mut one = [0u8; 6];
            one.copy_from_slice(peer);
            one
        })
        .collect())
}

/// The compact form of peers: four bytes of address and two of port each, which is how
/// trackers and `ut_pex` both send them (BEP 23).
pub fn compact_peers(peers: &[std::net::SocketAddr]) -> Vec<u8> {
    let mut out = Vec::with_capacity(peers.len() * 6);
    for peer in peers {
        match peer {
            std::net::SocketAddr::V4(peer) => {
                out.extend_from_slice(&peer.ip().octets());
                out.extend_from_slice(&peer.port().to_be_bytes());
            }
            std::net::SocketAddr::V6(peer) => {
                out.extend_from_slice(&peer.ip().octets());
                out.extend_from_slice(&peer.port().to_be_bytes());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// A bencoded dictionary written out by hand rather than through the bencode module,
    /// so that a test says what it expects to be able to read.
    fn dict(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"d".to_vec();
        for (key, value) in entries {
            out.extend_from_slice(format!("{}:", key.len()).as_bytes());
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(value);
        }
        out.push(b'e');
        out
    }

    fn integer(value: i64) -> Vec<u8> {
        format!("i{value}e").into_bytes()
    }

    fn string(bytes: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", bytes.len()).into_bytes();
        out.extend_from_slice(bytes);
        out
    }

    /// A message through the wire and back, which is the only way a peer ever shows it.
    fn round_trip(message: Message) -> Message {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &message).unwrap();
        let mut cursor = Cursor::new(buffer);
        read_message(&mut cursor).unwrap().expect("a message should be there")
    }

    /// The bytes an extended message carries for its extension.
    fn payload_of(message: &Message) -> Vec<u8> {
        match message {
            Message::Extended { payload, .. } => payload.clone(),
            other => panic!("not an extended message: {other:?}"),
        }
    }

    #[test]
    fn a_greeting_is_the_protocol_name_and_two_hashes() {
        let handshake = Handshake::new(&[7; 20], &[9; 20]);
        let bytes = handshake.bytes();
        assert_eq!(bytes.len(), HANDSHAKE_LEN);
        assert_eq!(bytes[0..19], *b"BitTorrent protocol");
        // The peer extension is marked, and nothing else: a mark for a fast set this
        // client does not speak would be a promise it cannot keep (BEP 10).
        let mut reserved = [0u8; 8];
        reserved[5] = 0x10;
        assert_eq!(bytes[19..27], reserved);
        assert_eq!(bytes[27..47], [7u8; 20]);
        assert_eq!(bytes[47..67], [9u8; 20]);
        assert_eq!(Handshake::parse(&bytes), Ok(handshake));
    }

    #[test]
    fn a_greeting_in_another_protocol_is_refused() {
        let mut bytes = Handshake::new(&[1; 20], &[2; 20]).bytes();
        bytes[0] = b'X';
        assert!(Handshake::parse(&bytes).is_err());
        assert!(Handshake::parse(&[0u8; HANDSHAKE_LEN - 1]).is_err());
        assert!(Handshake::parse(&[]).is_err());
    }

    #[test]
    fn the_reserved_bytes_are_left_as_they_arrived() {
        // Another extension (BEP 6's fast set, the DHT's) puts its mark here, and a peer
        // that has one is still a peer that can send us the file.
        let mut bytes = Handshake::new(&[3; 20], &[4; 20]).bytes();
        bytes[19..27].copy_from_slice(&[0x80, 0, 0, 0, 0, 0x10, 0, 0x01]);
        let handshake = Handshake::parse(&bytes).unwrap();
        assert_eq!(handshake.reserved, [0x80, 0, 0, 0, 0, 0x10, 0, 0x01]);
        assert_eq!(handshake.info_hash, [3; 20]);
        assert_eq!(handshake.peer_id, [4; 20]);
        assert_eq!(Handshake::parse(&handshake.bytes()), Ok(handshake));
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        let messages = [
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(0),
            Message::Have(u32::MAX),
            Message::Bitfield(vec![0b1010_0101, 0xFF, 0x00]),
            Message::Request { piece: 3, begin: 1, length: 16 * 1024 },
            Message::Cancel { piece: 3, begin: 1, length: 16 * 1024 },
            Message::Piece { piece: 2, begin: 32, data: vec![9; 1024] },
            Message::Port(6881),
            Message::Port(0),
            Message::Extended { id: 1, payload: b"de".to_vec() },
            Message::Extended { id: 200, payload: Vec::new() },
            Message::Unknown { id: 99, payload: vec![1, 2, 3] },
            Message::Unknown { id: 10, payload: Vec::new() },
        ];
        for message in messages {
            assert_eq!(round_trip(message.clone()), message);
        }
    }

    #[test]
    fn a_message_is_its_id_and_its_payload() {
        assert_eq!(Message::KeepAlive.encode(), Vec::<u8>::new());
        assert_eq!(Message::Choke.encode(), vec![1]);
        assert_eq!(Message::NotInterested.encode(), vec![4]);
        assert_eq!(Message::Have(0x0102_0304).encode(), vec![5, 1, 2, 3, 4]);
        assert_eq!(Message::Bitfield(vec![0xAA]).encode(), vec![6, 0xAA]);
        assert_eq!(
            Message::Request { piece: 1, begin: 2, length: 3 }.encode(),
            vec![7, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]
        );
        assert_eq!(
            Message::Piece { piece: 1, begin: 0, data: vec![9, 9] }.encode(),
            vec![8, 0, 0, 0, 1, 0, 0, 0, 0, 9, 9]
        );
        assert_eq!(
            Message::Cancel { piece: 1, begin: 2, length: 3 }.encode(),
            vec![9, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]
        );
        assert_eq!(Message::Port(6881).encode(), vec![20, 0x1a, 0xe1]);
        assert_eq!(Message::Extended { id: 2, payload: vec![b'e'] }.encode(), vec![20, 2, b'e']);
        assert_eq!(Message::Unknown { id: 99, payload: vec![1] }.encode(), vec![99, 1]);
    }

    #[test]
    fn a_keep_alive_is_four_bytes_and_nothing_else() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &Message::KeepAlive).unwrap();
        assert_eq!(buffer, vec![0, 0, 0, 0]);
        let mut cursor = Cursor::new(buffer);
        assert_eq!(read_message(&mut cursor).unwrap(), Some(Message::KeepAlive));
    }

    #[test]
    fn a_message_over_the_limit_is_refused_before_it_is_held() {
        let mut cursor = Cursor::new(vec![0xFF, 0xFF, 0xFF, 0xFF, 1, 2, 3]);
        assert!(read_message(&mut cursor).is_err());
        let mut cursor = Cursor::new(vec![0, 0, 0x20, 0, 1, 2, 3]);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn an_id_this_does_not_know_is_kept_as_it_arrived() {
        let message = Message::Unknown { id: 123, payload: vec![4, 5] };
        assert_eq!(round_trip(message.clone()), message);
        // A known id whose payload is too short for what it should hold is kept the same
        // way: the framing said how long it was, so what is in it is what was meant.
        let mut cursor = Cursor::new(vec![0, 0, 0, 3, 5, 1, 2]);
        assert_eq!(
            read_message(&mut cursor).unwrap(),
            Some(Message::Unknown { id: 5, payload: vec![1, 2] })
        );
    }

    #[test]
    fn an_id_with_nothing_after_it_still_names_its_message() {
        let mut cursor = Cursor::new(vec![0, 0, 0, 1, 1]);
        assert_eq!(read_message(&mut cursor).unwrap(), Some(Message::Choke));
        let mut cursor = Cursor::new(vec![0, 0, 0, 1, 20]);
        assert_eq!(
            read_message(&mut cursor).unwrap(),
            Some(Message::Unknown { id: 20, payload: Vec::new() })
        );
    }

    #[test]
    fn a_port_and_an_extension_share_one_id() {
        // Nothing on the wire says which of the two id 20 is, so two bytes behind it are
        // a port: an extension whose whole body is that short reads back as a port, which
        // is a thing no extension this client uses is.
        let mut buffer = Vec::new();
        write_message(&mut buffer, &Message::Port(6881)).unwrap();
        let mut cursor = Cursor::new(buffer);
        assert_eq!(read_message(&mut cursor).unwrap(), Some(Message::Port(6881)));

        let mut buffer = Vec::new();
        write_message(&mut buffer, &Message::Extended { id: 2, payload: vec![b'e'] }).unwrap();
        let mut cursor = Cursor::new(buffer);
        assert_eq!(
            read_message(&mut cursor).unwrap(),
            Some(Message::Port(u16::from_be_bytes([2, b'e'])))
        );
    }

    #[test]
    fn the_end_of_the_connection_is_not_a_message() {
        let mut cursor = Cursor::new(Vec::new());
        assert_eq!(read_message(&mut cursor).unwrap(), None);
    }

    #[test]
    fn a_message_cut_short_is_an_error_and_not_a_panic() {
        // The length says ten bytes, six of them arrive.
        let mut cursor = Cursor::new(vec![0, 0, 0, 10, 6, 1, 2, 3, 4, 5]);
        assert!(read_message(&mut cursor).is_err());
        // The length itself is cut short.
        let mut cursor = Cursor::new(vec![0, 0]);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn messages_are_read_one_after_another() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &Message::Unchoke).unwrap();
        write_message(&mut buffer, &Message::Have(4)).unwrap();
        write_message(&mut buffer, &Message::KeepAlive).unwrap();
        let mut cursor = Cursor::new(buffer);
        assert_eq!(read_message(&mut cursor).unwrap(), Some(Message::Unchoke));
        assert_eq!(read_message(&mut cursor).unwrap(), Some(Message::Have(4)));
        assert_eq!(read_message(&mut cursor).unwrap(), Some(Message::KeepAlive));
        assert_eq!(read_message(&mut cursor).unwrap(), None);
    }

    #[test]
    fn a_bitfield_grows_as_far_as_the_pieces_ask_for() {
        let mut bits = Vec::new();
        for piece in 0..20u32 {
            bitfield_set(&mut bits, piece);
        }
        // Twenty pieces are three bytes, and the last one is in the third of them.
        assert_eq!(bits.len(), 3);
        assert_eq!(bits, vec![0xFF, 0xFF, 0xF0]);
        for piece in 0..20u32 {
            assert!(bitfield_has(&bits, piece), "piece {piece} should be there");
        }
        assert!(!bitfield_has(&bits, 20));
        assert!(!bitfield_has(&bits, u32::MAX));
        assert!(!bitfield_has(&[], 0));
    }

    #[test]
    fn a_bitfield_of_a_known_size_keeps_to_its_end() {
        let mut bits = [0u8; 2];
        for piece in 0..20u32 {
            bitfield_set(&mut bits, piece);
        }
        assert!(bitfield_has(&bits, 0));
        assert!(bitfield_has(&bits, 15));
        assert!(!bitfield_has(&bits, 16));
        assert!(!bitfield_has(&bits, 100_000));
    }

    #[test]
    fn an_extended_handshake_survives_a_round_trip() {
        let handshake = ExtendedHandshake {
            ut_metadata: Some(1),
            ut_pex: Some(2),
            metadata_size: Some(65_536),
            client: Some("stremio-ps5".to_owned()),
        };
        let payload = handshake.payload();
        assert_eq!(ExtendedHandshake::parse(&payload), Ok(handshake.clone()));
        assert_eq!(payload_of(&handshake.message(0)), payload);
    }

    #[test]
    fn an_extended_handshake_with_nothing_in_it_knows_nothing() {
        let handshake = ExtendedHandshake::parse(b"").unwrap();
        assert_eq!(handshake, ExtendedHandshake::default());
        assert_eq!(handshake.ut_metadata, None);
        assert_eq!(handshake.ut_pex, None);
        assert_eq!(handshake.metadata_size, None);
        assert_eq!(handshake.client, None);
        // An empty dictionary is read the same way, and still answers as one.
        let empty = ExtendedHandshake::default();
        assert_eq!(ExtendedHandshake::parse(&empty.payload()), Ok(empty));
    }

    #[test]
    fn an_extended_handshake_that_is_not_a_dictionary_is_refused() {
        assert!(ExtendedHandshake::parse(b"i5e").is_err());
        assert!(ExtendedHandshake::parse(b"5:hello").is_err());
        assert!(ExtendedHandshake::parse(b"le").is_err());
        assert!(ExtendedHandshake::parse(b"d1:me").is_err());
    }

    #[test]
    fn the_description_size_is_only_read_from_a_peer_that_has_the_extension() {
        let payload = dict(&[
            ("m", dict(&[("ut_pex", integer(2))])),
            ("metadata_size", integer(4096)),
            ("v", string(b"someone else")),
        ]);
        let handshake = ExtendedHandshake::parse(&payload).unwrap();
        assert_eq!(handshake.ut_pex, Some(2));
        assert_eq!(handshake.ut_metadata, None);
        assert_eq!(handshake.metadata_size, None);
        assert_eq!(handshake.client.as_deref(), Some("someone else"));

        // The same payload with `ut_metadata` in it does carry the size.
        let payload = dict(&[
            ("m", dict(&[("ut_metadata", integer(1)), ("ut_pex", integer(2))])),
            ("metadata_size", integer(4096)),
        ]);
        let handshake = ExtendedHandshake::parse(&payload).unwrap();
        assert_eq!(handshake.ut_metadata, Some(1));
        assert_eq!(handshake.metadata_size, Some(4096));
    }

    #[test]
    fn every_ut_metadata_message_survives_a_round_trip() {
        let messages = [
            MetadataMessage::Request { piece: 0 },
            MetadataMessage::Request { piece: 3 },
            MetadataMessage::Data { piece: 0, total: Some(32 * 1024), data: vec![4; 1024] },
            MetadataMessage::Data { piece: 1, total: None, data: vec![4; METADATA_CHUNK] },
            MetadataMessage::Reject { piece: 7 },
        ];
        for message in messages {
            let payload = payload_of(&message.message(3));
            assert_eq!(MetadataMessage::parse(&payload), Ok(message));
        }
    }

    #[test]
    fn a_part_after_the_first_says_nothing_about_the_whole_size() {
        let message = MetadataMessage::Data { piece: 1, total: None, data: vec![1, 2, 3] };
        let payload = payload_of(&message.message(1));
        assert_eq!(payload.first(), Some(&b'e'));
        assert!(!payload.windows(10).any(|window| window == b"total_size"));
        // A peer that sends it anyway is taken at its word.
        let sent = dict(&[
            ("data", string(&[1, 2, 3])),
            ("msg_type", integer(1)),
            ("piece", integer(1)),
            ("total_size", integer(3000)),
        ]);
        let mut marked = b"e".to_vec();
        marked.extend(sent);
        assert_eq!(
            MetadataMessage::parse(&marked),
            Ok(MetadataMessage::Data { piece: 1, total: Some(3000), data: vec![1, 2, 3] })
        );
    }

    #[test]
    fn a_ut_metadata_message_is_read_with_or_without_its_mark() {
        let payload = dict(&[("msg_type", integer(0)), ("piece", integer(4))]);
        let wanted = Ok(MetadataMessage::Request { piece: 4 });
        assert_eq!(MetadataMessage::parse(&payload), wanted);
        let mut marked = b"e".to_vec();
        marked.extend_from_slice(&payload);
        assert_eq!(MetadataMessage::parse(&marked), wanted);
    }

    #[test]
    fn a_ut_metadata_message_that_is_not_one_of_the_three_is_refused() {
        let payload = dict(&[("msg_type", integer(9)), ("piece", integer(0))]);
        assert!(MetadataMessage::parse(&payload).is_err());
        let payload = dict(&[("piece", integer(0))]);
        assert!(MetadataMessage::parse(&payload).is_err());
        let payload = dict(&[("msg_type", integer(1)), ("piece", integer(0))]);
        assert!(MetadataMessage::parse(&payload).is_err());
        assert!(MetadataMessage::parse(b"e").is_err());
    }

    #[test]
    fn a_ut_metadata_message_naming_no_piece_is_refused() {
        assert!(MetadataMessage::parse(&dict(&[("msg_type", integer(0))])).is_err());
        assert!(MetadataMessage::parse(&dict(&[("msg_type", integer(2))])).is_err());
    }

    #[test]
    fn added_peers_are_read_with_and_without_their_flags() {
        let peers = compact_peers(&[
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)), 6881),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 30)), 51413),
        ]);
        let wanted = Ok(vec![[10, 0, 0, 7, 0x1a, 0xe1], [192, 168, 1, 30, 0xc8, 0xd5]]);
        let payload = dict(&[("added", string(&peers))]);
        assert_eq!(parse_added_peers(&payload), wanted);
        let payload = dict(&[("added", string(&peers)), ("added.f", integer(0))]);
        assert_eq!(parse_added_peers(&payload), wanted);
    }

    #[test]
    fn added_peers_that_are_not_whole_are_refused() {
        let payload = dict(&[("added", string(&[0u8; 7]))]);
        assert!(parse_added_peers(&payload).is_err());
        let payload = dict(&[("added.f", integer(0))]);
        assert!(parse_added_peers(&payload).is_err());
        assert!(parse_added_peers(b"i7e").is_err());
    }

    #[test]
    fn compact_peers_hold_an_address_and_a_port() {
        let four = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 6881);
        let six = SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 1, 2, 3, 4, 5, 6)),
            80,
        );
        let bytes = compact_peers(&[four, six]);
        assert_eq!(bytes.len(), 6 + 18);
        assert_eq!(bytes[0..4], [1, 2, 3, 4]);
        assert_eq!(bytes[4..6], [0x1a, 0xe1]);
        assert_eq!(bytes[6..22], [0x20, 0x01, 0x0d, 0xb8, 0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6]);
        assert_eq!(bytes[22..24], [0x00, 80]);
        assert!(compact_peers(&[]).is_empty());
        // What one peer thread writes is what another reads.
        let payload = dict(&[("added", string(&bytes[0..6]))]);
        assert_eq!(parse_added_peers(&payload), Ok(vec![[1, 2, 3, 4, 0x1a, 0xe1]]));
    }
}