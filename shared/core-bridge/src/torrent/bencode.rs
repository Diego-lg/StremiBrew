//! Reading and writing bencode, the form torrent files, tracker answers and a peer's
//! extended handshake are all in.
//!
//! Everything read here arrived over a network, so reading is strict: what cannot be
//! read is refused with a reason rather than guessed at, and nesting is limited so a
//! value cannot use up the stack before it is found to be wrong. Writing is the other
//! way about, and must come out the same every time, because a torrent's identity is the
//! SHA-1 of exactly the bytes of its `info` dictionary.
//!
//! Every node keeps where in the bytes it was read from ([`Node::start`] and [`Node::end`]),
//! which is how the bytes a hash was taken over are found again without writing the
//! dictionary out and hoping it comes out the same.

/// How deep lists and dictionaries may nest. Anything deeper is refused: reading it would
/// take the stack a peer could otherwise fill before the value was found to be wrong.
const MAX_DEPTH: usize = 64;

/// A value, with the bytes it was read from.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub value: Value,
    /// Where the value starts in the bytes it was read from, and where it ends.
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Node>),
    /// The dictionary's entries, in the order they were read.
    Dict(Vec<(Vec<u8>, Node)>),
}

impl Node {
    /// The value of `key` when this is a dictionary that has one. The key is matched by
    /// its bytes, the way bencode both compares and orders them.
    pub fn get(&self, key: &str) -> Option<&Node> {
        let Value::Dict(entries) = &self.value else { return None };
        entries
            .iter()
            .find(|(name, _)| name.as_slice() == key.as_bytes())
            .map(|(_, value)| value)
    }

    pub fn int(&self) -> Option<i64> {
        match self.value {
            Value::Int(number) => Some(number),
            _ => None,
        }
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match &self.value {
            Value::Bytes(bytes) => Some(bytes.as_slice()),
            _ => None,
        }
    }

    /// The value as text, when it is a byte string that is valid UTF-8. Names and
    /// addresses are, but nothing forces them to be: the bytes stay either way and only
    /// this says no.
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(self.bytes()?).ok()
    }

    pub fn list(&self) -> Option<&[Node]> {
        match &self.value {
            Value::List(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    pub fn dict(&self) -> Option<&[(Vec<u8>, Node)]> {
        match &self.value {
            Value::Dict(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    /// An integer as a yes or no, for the flags in a tracker's answer.
    pub fn flag(&self) -> Option<bool> {
        // Senders use 0 and 1, but nothing says they must: only a value that is not a
        // number at all is an unknown answer.
        self.int().map(|number| number != 0)
    }
}

/// Reads one value from the front of `bytes`, which may go on after it (a peer's extended
/// message has a header before its bencoded part). `limit` is how many bytes the value may
/// use at most; pass `usize::MAX` when there is no reason to cap it.
pub fn parse(bytes: &[u8], limit: usize) -> Result<Node, String> {
    read(bytes, 0, limit.min(bytes.len()), 0)
}

/// Reads the value starting at `at`. `cap` is the first byte the value may not reach, which
/// is what `limit` (and the end of the bytes) come to; `depth` is how many lists and
/// dictionaries are already open around it.
fn read(bytes: &[u8], at: usize, cap: usize, depth: usize) -> Result<Node, String> {
    let Some(&first) = bytes.get(at..cap).and_then(|rest| rest.first()) else {
        return Err("a value is expected here".to_owned());
    };
    match first {
        b'i' => integer(bytes, at, cap),
        b'l' => list(bytes, at, cap, depth),
        b'd' => dictionary(bytes, at, cap, depth),
        b'0'..=b'9' => string(bytes, at, cap),
        _ => Err(format!("{} starts no value", first as char)),
    }
}

/// Reads `i<digits>e`. The digits are gathered as a magnitude first, so that the one value
/// `i64` cannot hold the other way round (`i64::MIN`) still reads.
fn integer(bytes: &[u8], at: usize, cap: usize) -> Result<Node, String> {
    let stop = match terminator(bytes, at + 1, cap, b'e') {
        Some(stop) => stop,
        None => return Err("an integer never ends".to_owned()),
    };
    let mut first = at + 1;
    let negative = bytes[first] == b'-';
    if negative {
        first += 1;
    }
    if first == stop {
        return Err("an integer holds no digits".to_owned());
    }
    let mut magnitude: u64 = 0;
    for &digit in &bytes[first..stop] {
        if !matches!(digit, b'0'..=b'9') {
            return Err("an integer may hold only digits".to_owned());
        }
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add((digit - b'0') as u64))
            .ok_or_else(|| "an integer is too large to read".to_owned())?;
    }
    let number = if negative {
        match magnitude {
            too_big if too_big > i64::MAX as u64 + 1 => {
                return Err("an integer is too small to read".to_owned())
            }
            exactly_min if exactly_min == i64::MAX as u64 + 1 => i64::MIN,
            other => -(other as i64),
        }
    } else {
        match magnitude {
            too_big if too_big > i64::MAX as u64 => {
                return Err("an integer is too large to read".to_owned())
            }
            other => other as i64,
        }
    };
    Ok(Node { value: Value::Int(number), start: at, end: stop + 1 })
}

/// Reads `<length>:<length bytes>`. The length must fit inside what is there and inside
/// `cap`, so a length claiming far more than the buffer holds is refused rather than
/// waited on.
fn string(bytes: &[u8], at: usize, cap: usize) -> Result<Node, String> {
    let stop = match terminator(bytes, at, cap, b':') {
        Some(stop) => stop,
        None => return Err("a string's length never ends".to_owned()),
    };
    let mut length: usize = 0;
    for &digit in &bytes[at..stop] {
        if !matches!(digit, b'0'..=b'9') {
            return Err("a string's length may hold only digits".to_owned());
        }
        length = length
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add((digit - b'0') as usize))
            .ok_or_else(|| "a string's length is too large to read".to_owned())?;
    }
    let first = stop + 1;
    let end = first
        .checked_add(length)
        .filter(|end| *end <= cap)
        .ok_or_else(|| "a string is longer than the bytes it has".to_owned())?;
    Ok(Node { value: Value::Bytes(bytes[first..end].to_vec()), start: at, end })
}

/// Reads `l...e`. What comes back is what was read, in the order it was read.
fn list(bytes: &[u8], at: usize, cap: usize, depth: usize) -> Result<Node, String> {
    if depth >= MAX_DEPTH {
        return Err("a list nests too deeply".to_owned());
    }
    let mut items: Vec<Node> = Vec::new();
    let mut next = at + 1;
    loop {
        match bytes.get(next..cap).and_then(|rest| rest.first()) {
            None => return Err("a list never ends".to_owned()),
            Some(&b'e') => return Ok(Node { value: Value::List(items), start: at, end: next + 1 }),
            Some(_) => items.push(read(bytes, next, cap, depth + 1)?),
        }
        next = items.last().map_or(next + 1, |item| item.end);
    }
}

/// Reads `d...e`, where every key is a byte string. A key that is anything else is refused:
/// there is no way to say what it meant, and a torrent's keys are what it is looked up by.
fn dictionary(bytes: &[u8], at: usize, cap: usize, depth: usize) -> Result<Node, String> {
    if depth >= MAX_DEPTH {
        return Err("a dictionary nests too deeply".to_owned());
    }
    let mut entries: Vec<(Vec<u8>, Node)> = Vec::new();
    let mut next = at + 1;
    loop {
        match bytes.get(next..cap).and_then(|rest| rest.first()) {
            None => return Err("a dictionary never ends".to_owned()),
            Some(&b'e') => return Ok(Node { value: Value::Dict(entries), start: at, end: next + 1 }),
            Some(&first) if !(b'0'..=b'9').contains(&first) => {
                return Err("a dictionary's key is not a string".to_owned())
            }
            Some(_) => {}
        }
        let key = read(bytes, next, cap, depth + 1)?;
        let Value::Bytes(name) = key.value else {
            return Err("a dictionary's key is not a string".to_owned());
        };
        let value = read(bytes, key.end, cap, depth + 1)?;
        next = value.end;
        entries.push((name, value));
    }
}

/// Where `mark` next is in `bytes[from..cap]`, as an offset from the front of `bytes`.
fn terminator(bytes: &[u8], from: usize, cap: usize, mark: u8) -> Option<usize> {
    bytes
        .get(from..cap)?
        .iter()
        .position(|byte| *byte == mark)
        .map(|offset| from + offset)
}

/// Writes `value` out again, with a dictionary's keys in the order bencode requires (by
/// their bytes, not as text).
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(&mut out, value);
    out
}

fn write(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Int(number) => {
            out.push(b'i');
            out.extend_from_slice(number.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                write(out, &item.value);
            }
            out.push(b'e');
        }
        Value::Dict(entries) => {
            // By the bytes, so that a key that is not text at all still lands in a fixed
            // place and two writers of one torrent agree.
            let mut sorted: Vec<&(Vec<u8>, Node)> = entries.iter().collect();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            out.push(b'd');
            for (key, item) in sorted {
                out.extend_from_slice(key.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(key);
                write(out, &item.value);
            }
            out.push(b'e');
        }
    }
}

/// Where the value of `key` sits within the bencoded dictionary at the front of `bytes`:
/// its first byte and one past its last. This is how the SHA-1 of a torrent's `info`
/// dictionary is taken over exactly the bytes the torrent's hash was made from.
pub fn dictionary_span(bytes: &[u8], key: &str) -> Result<(usize, usize), String> {
    let front = parse(bytes, usize::MAX)?;
    let Value::Dict(entries) = &front.value else {
        return Err("what is here is not a dictionary".to_owned());
    };
    let wanted = key.as_bytes();
    for (name, value) in entries {
        if name.as_slice() == wanted {
            return Ok((value.start, value.end));
        }
    }
    Err(format!("the dictionary has no {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A torrent, hand-written the way a torrent file is: an announce address and an
    /// `info` dictionary holding the length and the name.
    const TORRENT: &[u8] = b"d8:announce18:http://tracker/ann4:infod6:lengthi16384e4:name3:abcee";

    /// A tracker's answer: what it says about progress and the peers it names.
    const ANSWER: &[u8] =
        b"d8:completei0e8:intervali1800e10:incompletei7e5:peers6:\x01\x02\x03\x04\x05\x06e";

    /// A value with all four kinds in it, twice over.
    const NESTED: &[u8] = b"li1e3:abcd1:kli2eeee";

    /// The value alone, with every span dropped: spans are checked on their own.
    fn plain(node: &Node) -> Value {
        let value = match &node.value {
            Value::Int(_) | Value::Bytes(_) => node.value.clone(),
            Value::List(items) => Value::List(items.iter().map(|item| unplaced(plain(item))).collect()),
            Value::Dict(entries) => Value::Dict(
                entries.iter().map(|(key, item)| (key.clone(), unplaced(plain(item)))).collect(),
            ),
        };
        value
    }

    /// A node with no span, for building what is expected of a read one.
    fn unplaced(value: Value) -> Node {
        Node { value, start: 0, end: 0 }
    }

    fn value_of(bytes: &[u8]) -> Value {
        plain(&parse(bytes, usize::MAX).unwrap())
    }

    #[test]
    fn reads_each_kind_of_value() {
        assert_eq!(value_of(b"i42e"), Value::Int(42));
        assert_eq!(value_of(b"i0e"), Value::Int(0));
        assert_eq!(value_of(b"i-7e"), Value::Int(-7));
        assert_eq!(value_of(b"i-0e"), Value::Int(0));
        assert_eq!(value_of(b"i9223372036854775807e"), Value::Int(i64::MAX));
        assert_eq!(value_of(b"i-9223372036854775808e"), Value::Int(i64::MIN));

        assert_eq!(value_of(b"0:"), Value::Bytes(vec![]));
        assert_eq!(value_of(b"4:test"), Value::Bytes(b"test".to_vec()));
        // A length is the only thing that says how long a string is: nothing in the bytes
        // themselves ends it.
        assert_eq!(value_of(b"4:abcd"), Value::Bytes(b"abcd".to_vec()));
        assert_eq!(value_of(b"23:http://tracker/announce"), Value::Bytes(b"http://tracker/announce".to_vec()));
        // A string is a length and then exactly that many bytes, so a NUL, a `:` and the
        // `e` that would otherwise end something are only text inside it.
        assert_eq!(value_of(b"5:a\0b:e"), Value::Bytes(b"a\0b:e".to_vec()));

        assert_eq!(value_of(b"le"), Value::List(vec![]));
        assert_eq!(value_of(b"de"), Value::Dict(vec![]));
        assert_eq!(
            value_of(NESTED),
            Value::List(vec![
                unplaced(Value::Int(1)),
                unplaced(Value::Bytes(b"abc".to_vec())),
                unplaced(Value::Dict(vec![(
                    b"k".to_vec(),
                    unplaced(Value::List(vec![unplaced(Value::Int(2))])),
                )])),
            ])
        );
    }

    #[test]
    fn a_node_says_what_it_holds() {
        let answer = parse(ANSWER, usize::MAX).unwrap();
        assert_eq!(answer.dict().map(<[_]>::len), Some(4));
        assert_eq!(answer.get("interval").and_then(Node::int), Some(1800));
        assert_eq!(answer.get("peers").and_then(Node::bytes), Some(&[1, 2, 3, 4, 5, 6][..]));
        assert_eq!(answer.get("nowhere"), None);
        // A tracker sends these as bytes, so nothing here is text.
        assert_eq!(answer.text(), None);
        assert_eq!(answer.int(), None);
        assert_eq!(answer.list(), None);
        // Nor are the peers' addresses turned into anything: they are text, oddly enough,
        // because a control character is as good UTF-8 as any other.
        assert_eq!(answer.get("peers").and_then(Node::text), Some("\u{1}\u{2}\u{3}\u{4}\u{5}\u{6}"));

        let list = parse(b"l3:onei-7ee", usize::MAX).unwrap();
        assert_eq!(list.list().map(<[_]>::len), Some(2));
        assert_eq!(list.dict(), None);
        assert_eq!(list.get("one"), None, "a list is not looked up by key");
        assert_eq!(list.bytes(), None);

        let bytes = parse(b"5:caf\xc3\xa9", usize::MAX).unwrap();
        assert_eq!(bytes.text(), Some("café"));
        let broken = parse(b"2:\xff\xfe", usize::MAX).unwrap();
        assert_eq!(broken.text(), None, "text that is not UTF-8 is refused, not guessed");
        assert_eq!(broken.bytes(), Some(&[0xff, 0xfe][..]));

        let dict = parse(b"d1:k3:onee", usize::MAX).unwrap();
        assert_eq!(dict.dict().map(<[_]>::len), Some(1));
        assert_eq!(dict.get("k").and_then(Node::text), Some("one"));
    }

    #[test]
    fn a_tracker_answer_reads_as_yes_and_no() {
        let answer = parse(ANSWER, usize::MAX).unwrap();
        assert_eq!(answer.get("complete").and_then(Node::flag), Some(false));
        assert_eq!(answer.get("incomplete").and_then(Node::flag), Some(true));
        // Anything but zero counts as yes, since some senders answer with a number.
        assert_eq!(answer.get("interval").and_then(Node::flag), Some(true));
        assert_eq!(answer.get("peers").and_then(Node::flag), None, "not a number");
        assert_eq!(parse(b"i0e", usize::MAX).unwrap().flag(), Some(false));
        assert_eq!(parse(b"i-1e", usize::MAX).unwrap().flag(), Some(true));
    }

    #[test]
    fn what_is_left_out_is_refused() {
        for broken in [
            &b""[..],
            b"i42",
            b"ie",
            b"i12x3e",
            b"i-e",
            b"3:ab",
            b"l",
            b"d",
            b"l3:ab",
            b"d3:one",
            b"d3:onei1e",
            b"di1e1:ae",
            b"dle1:ae",
            b"-3:abc",
            b"-1:a",
            b"x",
            b"i9223372036854775808e",
            b"i-9223372036854775809e",
            b"i99999999999999999999e",
            b"99999999999999999999999:a",
        ] {
            assert!(parse(broken, usize::MAX).is_err(), "{broken:?} should not read");
        }
    }

    #[test]
    fn a_value_cut_short_is_refused() {
        // Every prefix of a whole value is a value cut short, and none of them reads.
        for whole in [TORRENT, ANSWER, NESTED] {
            assert!(parse(whole, usize::MAX).is_ok());
            for length in 0..whole.len() {
                assert!(
                    parse(&whole[..length], usize::MAX).is_err(),
                    "{whole:?} cut to {length} should not read"
                );
            }
        }
    }

    #[test]
    fn nesting_too_deep_is_refused() {
        let mut deep = Vec::new();
        deep.extend(std::iter::repeat(b'l').take(MAX_DEPTH));
        deep.extend(std::iter::repeat(b'e').take(MAX_DEPTH));
        assert!(parse(&deep, usize::MAX).is_ok());
        let mut deeper = Vec::new();
        deeper.extend(std::iter::repeat(b'l').take(MAX_DEPTH + 1));
        deeper.extend(std::iter::repeat(b'e').take(MAX_DEPTH + 1));
        let error = parse(&deeper, usize::MAX).unwrap_err();
        assert!(error.contains("deeply"), "{error}");
        // A dictionary around it is refused at the same depth, not one later.
        let mut mixed = Vec::new();
        for _ in 0..MAX_DEPTH + 1 {
            mixed.extend_from_slice(b"ld1:k");
        }
        mixed.extend(std::iter::repeat(b'e').take(MAX_DEPTH + 1));
        assert!(parse(&mixed, usize::MAX).is_err());
    }

    #[test]
    fn a_limit_caps_what_a_value_may_use() {
        assert!(parse(NESTED, usize::MAX).is_ok());
        assert!(parse(NESTED, NESTED.len()).is_ok(), "a value that exactly fills the cap fits");
        assert!(parse(NESTED, NESTED.len() - 1).is_err(), "one byte short is one byte too few");
        assert!(parse(NESTED, 0).is_err());
        assert!(parse(ANSWER, 20).is_err());
        // A length reaching past the cap is refused however many bytes are there.
        assert!(parse(b"9999999999:x", 12).is_err());
        // And a cap larger than the bytes is no cap at all.
        assert!(parse(ANSWER, usize::MAX - 1).is_ok());
    }

    #[test]
    fn spans_sit_exactly_where_the_bytes_are() {
        let torrent = TORRENT;
        let whole = parse(torrent, usize::MAX).unwrap();
        assert_eq!(whole.start, 0);
        assert_eq!(whole.end, torrent.len(), "the torrent is one value, all of it");
        for (key, child) in whole.dict().unwrap() {
            let key = std::str::from_utf8(key).unwrap();
            let (start, end) = dictionary_span(torrent, key).unwrap();
            assert_eq!((child.start, child.end), (start, end), "the span of {key}");
            // And the bytes over that span read back as the same value. This is what a
            // torrent's hash is taken over, so it may not drift by a byte.
            assert_eq!(plain(&parse(&torrent[start..end], usize::MAX).unwrap()), plain(child));
            // The name and the length sit inside the `info` dictionary's own span, and
            // outside each other's.
            let (inner, _) = dictionary_span(torrent, "info").unwrap();
            if key != "announce" {
                assert!(child.start >= inner && child.end <= dictionary_span(torrent, "info").unwrap().1);
            }
        }
    }

    #[test]
    fn a_torrents_info_dictionary_is_measured_exactly() {
        // The announce address sits after its own name, and the `info` dictionary fills
        // everything from there to the last byte but one.
        assert_eq!(dictionary_span(TORRENT, "announce"), Ok((11, 32)));
        assert_eq!(dictionary_span(TORRENT, "info"), Ok((38, TORRENT.len() - 1)));

        let info = parse(&TORRENT[38..TORRENT.len() - 1], usize::MAX).unwrap();
        assert_eq!(info.start, 0, "read on its own, the span starts at its own first byte");
        assert_eq!(info.end, TORRENT.len() - 1 - 38);
        assert_eq!(info.get("length").and_then(Node::int), Some(16384));
        assert_eq!(info.get("name").and_then(Node::text), Some("abc"));
        assert_eq!(info.dict().map(<[_]>::len), Some(2));

        assert!(dictionary_span(TORRENT, "pieces").is_err(), "there is no such key");
        assert!(dictionary_span(b"i16384e", "info").is_err(), "not a dictionary");
        assert!(dictionary_span(b"d4:inf", "info").is_err(), "cut short");
        assert!(dictionary_span(b"", "info").is_err());
    }

    #[test]
    fn what_comes_after_a_value_is_left_alone() {
        // A peer sends a message with a header in front of its value and its own length
        // behind, so the value in the middle has to read without either end in the way.
        let mut message = b"d1:ai1ee".to_vec();
        message.extend_from_slice(b"\x13\x00\x00\x04payload");
        let node = parse(&message, usize::MAX).unwrap();
        assert_eq!((node.start, node.end), (0, 8), "the value is what was read, and no more");
        assert_eq!(node.get("a").and_then(Node::int), Some(1));

        let mut short = b"3:abc".to_vec();
        short.extend_from_slice(&[0u8; 32]);
        let node = parse(&short, 16).unwrap();
        assert_eq!(node.end, 5);
        assert_eq!(node.bytes(), Some(&b"abc"[..]));

        // What follows a value is not part of it: the dictionary here never ends, so it is
        // refused however much comes after it.
        let mut unended = b"d1:ai1e".to_vec();
        unended.extend_from_slice(b"\x00\x00\x00\x05trailing");
        assert!(parse(&unended, usize::MAX).is_err());
    }

    #[test]
    fn a_value_written_out_reads_back_the_same() {
        let value = Value::Dict(vec![
            (b"pieces".to_vec(), unplaced(Value::Bytes(vec![0xaa; 20]))),
            (b"piece length".to_vec(), unplaced(Value::Int(32768))),
            (b"announce".to_vec(), unplaced(Value::Bytes(b"http://tracker/announce".to_vec()))),
            (b"name".to_vec(), unplaced(Value::Bytes(b"example".to_vec()))),
            (vec![0xff, 0x00], unplaced(Value::List(vec![unplaced(Value::Int(0)), unplaced(Value::Int(-1))]))),
            (b"".to_vec(), unplaced(Value::Dict(vec![]))),
        ]);
        let written = encode(&value);
        // What was written reads back as what was put in, key for key: reading keeps the
        // order it found, so this also says every key came out where it belongs.
        let read = parse(&written, usize::MAX).unwrap();
        let Value::Dict(mine) = &value else { unreachable!() };
        for (key, item) in read.dict().unwrap() {
            let wanted = mine
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, item)| plain(item))
                .unwrap();
            assert_eq!(&plain(item), &wanted, "the value of {key:?}");
        }
        assert_eq!(read.dict().map(<[_]>::len), Some(6));
        // And writing what was read changes nothing further: one value, one form.
        assert_eq!(encode(&read.value), written);

        // The keys came out by their bytes: `piece length` before `pieces` (the space is
        // worth less than the `s`), and the key that is not text at all last of all, which
        // is the order nothing but a byte comparison gives.
        let keys: Vec<Vec<u8>> = read.dict().unwrap().iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(
            keys,
            vec![
                b"".to_vec(),
                b"announce".to_vec(),
                b"name".to_vec(),
                b"piece length".to_vec(),
                b"pieces".to_vec(),
                vec![0xff, 0x00],
            ]
        );
    }

    #[test]
    fn a_torrent_written_out_is_the_torrent_again() {
        let read = parse(TORRENT, usize::MAX).unwrap();
        assert_eq!(encode(&read.value), TORRENT, "a torrent file is already in order");
        // Even when its dictionaries are handed over out of order, which is how one built
        // by hand or by a magnet's peer may arrive.
        let mut shuffled = read.value.clone();
        if let Value::Dict(entries) = &mut shuffled {
            entries.reverse();
        }
        assert_eq!(encode(&shuffled), TORRENT);
        assert_eq!(dictionary_span(&encode(&shuffled), "info"), Ok((38, TORRENT.len() - 1)));
    }
}