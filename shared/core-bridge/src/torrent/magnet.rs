//! What a `magnet:` link says about a torrent, and how one is written.

/// A magnet link, in the form a Stremio add-on gives one: a hash to find the torrent by,
/// the trackers to look for peers on, and (in Stremio's own use of the form) which of the
/// torrent's files the stream is for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Magnet {
    pub info_hash: [u8; 20],
    /// `tr`, in the order they were given, without repeats.
    pub trackers: Vec<String>,
    /// `xs` and `so`: places the torrent's own description can be fetched from over
    /// HTTPS, which is quicker than asking peers for it.
    pub web_seeds: Vec<String>,
    /// `dn`: what to call the torrent until its own description says.
    pub name: Option<String>,
    /// `xl`: how big the file is, when the link says.
    pub length: Option<u64>,
    /// `fileIdx`: which of the torrent's files to play.
    pub file_index: Option<usize>,
    /// `fileMustInclude`: the parts a file's name must have for it to be the right one.
    pub file_must_include: Vec<String>,
}

impl Magnet {
    /// Reads a link. Whitespace before it is ignored. Fails when it carries no usable hash:
    /// without one there is nothing to ask peers for.
    pub fn parse(uri: &str) -> Result<Self, String> {
        let Some(query) = query_of(uri.trim_start()) else {
            return Err("this is not a magnet link: it does not begin with magnet:".to_owned());
        };

        let mut hash: Option<[u8; 20]> = None;
        // The first hash that could not be read is kept, to say what was wrong with it.
        let mut refused: Option<String> = None;
        let mut trackers: Vec<String> = Vec::new();
        let mut web_seeds: Vec<String> = Vec::new();
        let mut name: Option<String> = None;
        let mut length: Option<u64> = None;
        let mut file_index: Option<usize> = None;
        let mut file_must_include: Vec<String> = Vec::new();
        for pair in query.split('&') {
            // A piece with no `=` in it says nothing, and a link with a stray `&` is still
            // a link.
            let Some((key, value)) = pair.split_once('=') else { continue };
            // A tracker's own query string may hold `&` and `=`, so each value is taken as
            // a whole first and only then unescaped.
            let value = percent_decode(value.as_bytes());
            match percent_decode(key.as_bytes()).as_str() {
                // A link may name more than one hash (the same torrent re-released); the
                // first one that can be read is the torrent to look for.
                "xt" if hash.is_none() => match info_hash_from_xt(&value) {
                    Some(found) => hash = Some(found),
                    None => refused = refused.or(Some(value)),
                },
                "tr" => add_unique(&mut trackers, value),
                "xs" | "so" => add_unique(&mut web_seeds, value),
                // The first name is the one the link set out with.
                "dn" if name.is_none() => name = Some(value),
                // A size or an index that is not one cannot be used, but it is no reason
                // to throw the link away: what it is really for is learned from the
                // torrent's own description, which arrives soon enough.
                "xl" => length = length.or_else(|| value.parse().ok()),
                "fileIdx" => file_index = file_index.or_else(|| value.parse().ok()),
                // Stremio's own parameter: one may be repeated, and one may carry several
                // parts separated by commas. Either way it is a list of parts.
                "fileMustInclude" => {
                    for part in value.split(',') {
                        if !part.is_empty() {
                            add_unique(&mut file_must_include, part.to_owned());
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(info_hash) = hash else {
            return Err(match refused {
                Some(value) => format!(
                    "the magnet link's hash {value:.40} is not one this can use: 40 hexadecimal \
                     characters, 32 base32 characters, or a sha1 or sha256 multihash"
                ),
                None => "the magnet link carries no torrent hash (xt)".to_owned(),
            });
        };
        Ok(Magnet { info_hash, trackers, web_seeds, name, length, file_index, file_must_include })
    }

    /// Writes a link, keeping `file_index` and `file_must_include` so that a stream
    /// saying which file it wants stays saying so. The web seeds are left out: they are
    /// of the torrent's own making, not something an add-on is told.
    pub fn build(
        info_hash: &[u8; 20],
        trackers: &[String],
        name: Option<&str>,
        length: Option<u64>,
        file_index: Option<usize>,
        file_must_include: &[String],
    ) -> String {
        let mut link = format!("magnet:?xt=urn:btih:{}", info_hash_to_hex(info_hash));
        for tracker in trackers {
            link.push_str("&tr=");
            link.push_str(&String::from_utf8_lossy(&percent_encode(tracker)));
        }
        if let Some(name) = name {
            link.push_str("&dn=");
            link.push_str(&String::from_utf8_lossy(&percent_encode(name)));
        }
        if let Some(length) = length {
            link.push_str(&format!("&xl={length}"));
        }
        if let Some(index) = file_index {
            link.push_str(&format!("&fileIdx={index}"));
        }
        for part in file_must_include {
            link.push_str("&fileMustInclude=");
            link.push_str(&String::from_utf8_lossy(&percent_encode(part)));
        }
        link
    }
}

/// The query of a link: what follows `magnet:` and, when it is there, the `?`. The scheme
/// is read whatever its case, since links are written by hand as often as by software.
fn query_of(uri: &str) -> Option<&str> {
    const SCHEME: &str = "magnet:";
    if !uri.get(..SCHEME.len())?.eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let query = uri.get(SCHEME.len()..)?;
    Some(query.strip_prefix('?').unwrap_or(query))
}

/// The 20 bytes a link's `xt` names, in any of the forms such a parameter is written in.
/// Its parts, and the hash itself, are read whichever way round they are cased (base32 is
/// usually written in capitals and hexadecimal in either case, so both are read in both).
fn info_hash_from_xt(value: &str) -> Option<[u8; 20]> {
    let value = value.trim();
    let (scheme, rest) = value.split_once(':')?;
    let (kind, hash) = rest.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("urn") {
        return None;
    }
    let hash = hash.trim();
    if kind.eq_ignore_ascii_case("btih") {
        info_hash_from_hex(hash)
            .ok()
            .or_else(|| info_hash_from_base32(&hash.to_ascii_uppercase()))
    } else if kind.eq_ignore_ascii_case("btmh") {
        info_hash_from_multihash(hash)
    } else {
        None
    }
}

/// The 20 bytes of a 32-character base32 hash: RFC 4648's alphabet, no padding, five bits
/// of every character and eight bits of every byte.
fn info_hash_from_base32(text: &str) -> Option<[u8; 20]> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    if text.len() != 32 {
        return None;
    }
    let mut info_hash = [0u8; 20];
    let mut held = 0u32;
    let mut bits = 0;
    let mut written = 0;
    for digit in text.bytes() {
        let value = ALPHABET.iter().position(|known| *known == digit)? as u32;
        held = held << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            *info_hash.get_mut(written)? = (held >> bits) as u8;
            written += 1;
        }
    }
    Some(info_hash)
}

/// The 20 bytes of a `btmh` hash: a multihash is what was hashed (a code), how long its
/// digest is, and the digest itself, all written as hexadecimal. A sha2-256 digest is
/// longer than the 20 bytes an info hash is made of, so the front of it is taken.
fn info_hash_from_multihash(hex: &str) -> Option<[u8; 20]> {
    let bytes = bytes_from_hex(hex)?;
    let digest = bytes.get(2..)?;
    if !matches!(bytes.first()?, 0x11 | 0x12) || digest.len() < 20 {
        return None;
    }
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(digest.get(..20)?);
    Some(info_hash)
}

/// The 40 hexadecimal characters of an info hash, as its 20 bytes. Upper and lower case
/// are both accepted.
pub fn info_hash_from_hex(hex: &str) -> Result<[u8; 20], String> {
    let mut info_hash = [0u8; 20];
    let bytes = bytes_from_hex(hex)
        .filter(|bytes| bytes.len() == 20)
        .ok_or_else(|| format!("{hex:.40} is not an info hash: it needs 40 hexadecimal characters"))?;
    info_hash.copy_from_slice(&bytes);
    Ok(info_hash)
}

/// The 40 lower-case hexadecimal characters of an info hash.
pub fn info_hash_to_hex(info_hash: &[u8; 20]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(40);
    for byte in info_hash {
        hex.push(DIGITS[usize::from(byte >> 4)] as char);
        hex.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    hex
}

/// The bytes of a run of hexadecimal digits, or None when it is not one (an odd number of
/// them, or one that is not a digit).
fn bytes_from_hex(hex: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = hex.bytes().map(hex_digit).collect::<Option<Vec<u8>>>()?;
    if digits.len() % 2 != 0 {
        return None;
    }
    Some(digits.chunks(2).map(|pair| pair[0] << 4 | pair[1]).collect())
}

/// What one hexadecimal digit is worth, in either case.
fn hex_digit(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Turns a magnet parameter's bytes into text: `%20` and `+` are spaces. Bytes that are
/// not text are kept as they are (a tracker's address is always text, a name is not
/// always).
pub fn percent_decode(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            // A space, written the way a query writes one.
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' if at + 2 < bytes.len() => {
                match (hex_digit(bytes[at + 1]), hex_digit(bytes[at + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high << 4 | low);
                        at += 3;
                    }
                    // A `%` that does not begin a byte is text, as it was written.
                    _ => {
                        out.push(b'%');
                        at += 1;
                    }
                }
            }
            other => {
                out.push(other);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The bytes of a magnet parameter's value as they go on a wire: a space is `%20`, and
/// anything that is not unreserved text is `%` and its byte in hex.
pub fn percent_encode(text: &str) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        // Unreserved text goes as it is; everything else, a space included, is written out
        // so that a value with a `&` in it cannot end the parameter early.
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte);
            continue;
        }
        out.push(b'%');
        out.push(DIGITS[usize::from(byte >> 4)]);
        out.push(DIGITS[usize::from(byte & 0x0f)]);
    }
    out
}

/// Adds a value to a list unless the list already has it: links often name the same
/// tracker twice, and announcing on it twice is work for nothing.
fn add_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";
    const HASH: [u8; 20] = [
        0xdd, 0x82, 0x55, 0xec, 0xdc, 0x7c, 0xa5, 0x5f, 0xb0, 0xbb, 0xf8, 0x13, 0x23, 0xd8, 0x70, 0x62,
        0xdb, 0x1f, 0x6d, 0x1c,
    ];
    /// The same hash in RFC 4648 base32, the way some trackers' links write it.
    const BASE32: &str = "3WBFL3G4PSSV7MF37AJSHWDQMLNR63I4";

    #[test]
    fn reads_the_hash_of_a_plain_link() {
        let magnet = Magnet::parse(&format!("magnet:?xt=urn:btih:{HEX}")).unwrap();
        assert_eq!(magnet.info_hash, HASH);
        assert_eq!(info_hash_to_hex(&magnet.info_hash), HEX);
        assert!(magnet.trackers.is_empty());
        assert!(magnet.web_seeds.is_empty());
        assert_eq!(magnet.name, None);
        assert_eq!(magnet.length, None);
        assert_eq!(magnet.file_index, None);
        assert!(magnet.file_must_include.is_empty());
    }

    #[test]
    fn every_form_of_the_hash_gives_the_same_torrent() {
        let forms = [
            // The scheme and the hexadecimal in capitals, and a stray space before it.
            format!("  MAGNET:?xt=urn:btih:{}", HEX.to_ascii_uppercase()),
            format!("magnet:?xt=urn:btih:{BASE32}"),
            format!("magnet:?xt=URN:BTIH:{}", BASE32.to_ascii_lowercase()),
            // A sha1 multihash, and a sha2-256 one whole (32 bytes) and one cut short.
            format!("magnet:?xt=urn:btmh:1120{HEX}"),
            format!("magnet:?xt=urn:btmh:1220{HEX}00112233445566778899aabbccddeeff"),
            format!("magnet:?xt=urn:btmh:1214{HEX}"),
            // The first usable one is the one taken, whatever the one before it was.
            format!("magnet:?xt=urn:sha1:{HEX}&xt=urn:btih:{HEX}&xt=urn:btih:{BASE32}"),
        ];
        for form in forms {
            assert_eq!(Magnet::parse(&form).map(|magnet| magnet.info_hash), Ok(HASH), "{form}");
        }
    }

    #[test]
    fn reads_everything_an_add_on_says_about_a_stream() {
        let magnet = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{HEX}&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce\
             &tr=udp%3A%2F%2Ftracker%3A6969%2Fannounce%3Ffoo%3D1\
             &tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce\
             &xs=https%3A%2F%2Fexample.com%2Fdescribe&so=https%3A%2F%2Fmirror.example%2Fdescribe\
             &xs=https%3A%2F%2Fexample.com%2Fdescribe&dn=Some+Film+2020&dn=ignored\
             &xl=8589934592&xl=nonsense&fileIdx=3&fileIdx=-1\
             &fileMustInclude=Sample&fileMustInclude=1080p,Sample&fileMustInclude="
        ))
        .unwrap();
        assert_eq!(
            magnet.trackers,
            [
                "udp://tracker.opentrackr.org:1337/announce".to_owned(),
                "udp://tracker:6969/announce?foo=1".to_owned(),
            ]
        );
        assert_eq!(
            magnet.web_seeds,
            [
                "https://example.com/describe".to_owned(),
                "https://mirror.example/describe".to_owned(),
            ]
        );
        assert_eq!(magnet.name.as_deref(), Some("Some Film 2020"));
        assert_eq!(magnet.length, Some(8589934592));
        assert_eq!(magnet.file_index, Some(3));
        assert_eq!(magnet.file_must_include, ["Sample".to_owned(), "1080p".to_owned()]);

        // A name with spaces written out in full, and the brackets around the year left in
        // the value rather than escaped by whoever wrote the link.
        let escaped = Magnet::parse(&format!("magnet:?xt=urn:btih:{HEX}&dn=A%20Film%20(2026)")).unwrap();
        assert_eq!(escaped.name.as_deref(), Some("A Film (2026)"));
    }

    #[test]
    fn a_link_with_no_usable_hash_is_refused() {
        assert!(Magnet::parse(&format!("magnet:?tr=udp%3A%2F%2Ftracker&dn=nothing")).is_err());
        assert!(Magnet::parse("magnet:").is_err());
        assert!(Magnet::parse("").is_err());
        assert!(Magnet::parse("?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c").is_err());
        assert!(Magnet::parse(&format!("http://example.com/?xt=urn:btih:{HEX}")).is_err());
        assert!(Magnet::parse("magnet:?xt=urn:btih:not-a-hash").is_err());
        assert!(Magnet::parse("magnet:?xt=urn:sha1:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c").is_err());
        // A multihash of something this cannot take 20 bytes from.
        assert!(Magnet::parse("magnet:?xt=urn:btmh:1220ddaa").is_err());
    }

    #[test]
    fn a_built_link_reads_back_as_the_same_magnet() {
        let wanted = Magnet {
            info_hash: HASH,
            trackers: vec![
                "udp://tracker.opentrackr.org:1337/announce".to_owned(),
                "udp://tracker:6969/announce?passkey=abc&x=1".to_owned(),
            ],
            web_seeds: vec![],
            name: Some("Some Film (2026) 1080p".to_owned()),
            length: Some(4_294_967_296),
            file_index: Some(7),
            file_must_include: vec!["1080p".to_owned(), "BluRay".to_owned()],
        };
        let link = Magnet::build(
            &wanted.info_hash,
            &wanted.trackers,
            wanted.name.as_deref(),
            wanted.length,
            wanted.file_index,
            &wanted.file_must_include,
        );
        assert_eq!(
            link,
            format!(
                "magnet:?xt=urn:btih:{HEX}&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce\
                 &tr=udp%3A%2F%2Ftracker%3A6969%2Fannounce%3Fpasskey%3Dabc%26x%3D1\
                 &dn=Some%20Film%20%282026%29%201080p&xl=4294967296&fileIdx=7\
                 &fileMustInclude=1080p&fileMustInclude=BluRay"
            )
        );
        assert_eq!(Magnet::parse(&link).unwrap(), wanted);
    }

    #[test]
    fn a_bare_link_is_the_smallest_one_there_is() {
        let link = Magnet::build(&HASH, &[], None, None, None, &[]);
        assert_eq!(link, format!("magnet:?xt=urn:btih:{HEX}"));
        assert_eq!(Magnet::parse(&link).unwrap(), Magnet { info_hash: HASH, ..Magnet::default() });
    }

    #[test]
    fn values_survive_the_escaping() {
        assert_eq!(percent_decode(b"Some%20Film+%282026%29"), "Some Film (2026)");
        // A `%` that does not begin a byte is text, as it was written.
        assert_eq!(percent_decode(b"100%25 sure%2 not %zz"), "100% sure%2 not %zz");
        // Unreserved text goes as it is; a space, and everything else, is written out.
        assert_eq!(percent_encode("A-Z a-z 0-9 -._~"), b"A-Z%20a-z%200-9%20-._~");
        assert_eq!(percent_encode(" "), b"%20");
        assert_eq!(String::from_utf8_lossy(&percent_encode("/?&=")), "%2F%3F%26%3D");
        // A name is not always plain text, and a tracker carries its own query string.
        for value in ["Película número 1", "udp://tracker.example:1337/announce?a=1&b=2"] {
            assert_eq!(percent_decode(&percent_encode(value)), value);
        }
    }

    #[test]
    fn an_upper_case_hash_comes_back_in_lower_case() {
        assert_eq!(info_hash_from_hex(&HEX.to_ascii_uppercase()), Ok(HASH));
        assert_eq!(info_hash_from_hex(HEX), Ok(HASH));
        assert_eq!(info_hash_to_hex(&HASH), HEX);
        assert!(info_hash_from_hex("dd8255ecdc7ca55fb0bbf81323d87062db1f6d1").is_err());
        assert!(info_hash_from_hex(&format!("{HEX}0")).is_err());
        assert!(info_hash_from_hex(&HEX.replace('d', "z")).is_err());
        assert!(info_hash_from_hex("").is_err());

        let parsed = Magnet::parse(&format!("magnet:?xt=urn:btih:{}", HEX.to_ascii_uppercase())).unwrap();
        assert_eq!(
            Magnet::build(&parsed.info_hash, &[], None, None, None, &[]),
            format!("magnet:?xt=urn:btih:{HEX}")
        );
    }
}