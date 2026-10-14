//! A title's details for the UI: what the details screen shows about a film or series,
//! its episodes, and the streams add-ons offer for it.

use serde::Serialize;
use std::borrow::Cow;

use stremio_core::constants::{GENRES_LINK_CATEGORY, IMDB_LINK_CATEGORY};
use stremio_core::models::common::Loadable;
use stremio_core::models::meta_details::Selected;
use stremio_core::runtime::msg::{Action, ActionLoad};
use stremio_core::runtime::RuntimeAction;
use stremio_core::types::addon::ResourcePath;
use stremio_core::types::resource::{MetaItem, StreamSource};

use crate::model::{Ps5Model, Ps5ModelField};
use crate::torrent::{magnet::Magnet, PLAYABLE};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Details<'a> {
    /// "loading", "ready" or "error" (no add-on could describe the title).
    state: &'static str,
    id: &'a str,
    r#type: &'a str,
    name: &'a str,
    description: Option<&'a str>,
    background: Option<&'a str>,
    logo: Option<&'a str>,
    poster: Option<&'a str>,
    release_info: Option<&'a str>,
    runtime: Option<&'a str>,
    imdb_rating: Option<&'a str>,
    genres: Vec<&'a str>,
    cast: Vec<&'a str>,
    directors: Vec<&'a str>,
    videos: Vec<Video<'a>>,
    /// The video whose streams are loaded: an episode's id, or the title's own for a film.
    stream_video: Option<&'a str>,
    stream_groups: Vec<StreamGroup<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Video<'a> {
    id: &'a str,
    title: &'a str,
    season: Option<u32>,
    episode: Option<u32>,
    released: Option<String>,
    thumbnail: Option<&'a str>,
    overview: Option<&'a str>,
    /// Whether the account has watched it, and for the one left part-way how far, in
    /// thousandths.
    watched: bool,
    progress: Option<u32>,
}

/// One add-on's answer for the selected video.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamGroup<'a> {
    addon: &'a str,
    state: &'static str,
    streams: Vec<Stream<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Stream<'a> {
    name: Option<&'a str>,
    description: Option<&'a str>,
    /// The address to play, for streams this app can play: direct web links, and a torrent
    /// as the `magnet:` link its reader opens where the player has one. YouTube ids and
    /// external links have none. A torrent's link is written for the stream, so it is
    /// owned, while a web link is the add-on's own and is only borrowed.
    url: Option<Cow<'a, str>>,
    /// Why a stream without an address cannot be played here.
    unsupported: Option<&'static str>,
    /// The file's size in bytes, when the add-on gives it.
    size: Option<u64>,
}

/// Loads a title's details. `video` selects the video to load streams for: an episode's
/// id for a series, or null to use the title's own id for a film and none for a series.
pub fn load(
    runtime: &stremio_core::runtime::Runtime<crate::Ps5Env, Ps5Model>,
    r#type: &str,
    id: &str,
    video: Option<&str>,
) {
    let stream_id = video.or(if r#type == "series" { None } else { Some(id) });
    runtime.dispatch(RuntimeAction {
        field: Some(Ps5ModelField::MetaDetails),
        action: Action::Load(ActionLoad::MetaDetails(Selected {
            meta_path: ResourcePath::without_extra("meta", r#type, id),
            stream_path: stream_id.map(|video| ResourcePath::without_extra("stream", r#type, video)),
            guess_stream: false,
        })),
    });
}

fn links<'a>(item: &'a MetaItem, category: &str) -> Vec<&'a str> {
    item.preview
        .links
        .iter()
        .filter(|link| link.category == category)
        .map(|link| link.name.as_str())
        .collect()
}

/// What a torrent stream offers: the `magnet:` address a player with a torrent reader is
/// given, and nothing against it. Only the hash is needed for the link to name a torrent,
/// so every part an add-on may leave out is simply left out of it: no trackers, no file,
/// no name and no size still make an address worth offering.
fn torrent_stream(
    info_hash: &[u8; 20],
    announce: &[String],
    file_idx: Option<u16>,
    file_must_include: &[String],
    name: Option<&str>,
    video_size: Option<u64>,
) -> (Option<String>, Option<&'static str>) {
    let url = Magnet::build(
        info_hash,
        announce,
        name,
        video_size,
        file_idx.map(usize::from),
        file_must_include,
    );
    (Some(url), None)
}

/// The details as the UI draws them, or `None` when no title is selected.
pub fn details(model: &Ps5Model) -> Option<Details<'_>> {
    let meta = &model.meta_details;
    let selected = meta.selected.as_ref()?;
    // The first add-on that could describe the title wins, as in Stremio's other apps.
    let ready = meta.meta_items.iter().find_map(|item| match &item.content {
        Some(Loadable::Ready(item)) => Some(item),
        _ => None,
    });
    let failed = !meta.meta_items.is_empty()
        && meta
            .meta_items
            .iter()
            .all(|item| matches!(item.content, Some(Loadable::Err(_))));

    let stream_groups = meta
        .streams
        .iter()
        .filter_map(|group| {
            let addon = model
                .ctx
                .profile
                .addons
                .iter()
                .find(|addon| addon.transport_url == group.request.base)?;
            let (state, streams) = match &group.content {
                Some(Loadable::Ready(streams)) => (
                    "ready",
                    streams
                        .iter()
                        .map(|stream| {
                            // A torrent is played from the address its reader opens, which is
                            // written here, before the match, so that it can be moved into the
                            // stream rather than borrowed from something that goes away.
                            let torrent = match &stream.source {
                                StreamSource::Torrent {
                                    info_hash,
                                    file_idx,
                                    announce,
                                    file_must_include,
                                } if PLAYABLE => Some(torrent_stream(
                                    info_hash,
                                    announce,
                                    *file_idx,
                                    file_must_include,
                                    stream.name.as_deref(),
                                    stream.behavior_hints.video_size,
                                )),
                                _ => None,
                            };
                            let (url, unsupported) = match &stream.source {
                                StreamSource::Url { url }
                                    if matches!(url.scheme(), "http" | "https") =>
                                {
                                    (Some(Cow::Borrowed(url.as_str())), None)
                                }
                                StreamSource::Url { .. } => (None, Some("Unsupported link")),
                                StreamSource::YouTube { .. } => (None, Some("YouTube")),
                                // The address the player's torrent reader is given, or the
                                // refusal an app without one still shows.
                                StreamSource::Torrent { .. } => match torrent {
                                    Some((url, unsupported)) => (url.map(Cow::Owned), unsupported),
                                    None => (None, Some("Torrent")),
                                },
                                StreamSource::External { .. } => (None, Some("Opens elsewhere")),
                                _ => (None, Some("Unsupported")),
                            };
                            Stream {
                                name: stream.name.as_deref(),
                                description: stream.description.as_deref(),
                                url,
                                unsupported,
                                size: stream.behavior_hints.video_size,
                            }
                        })
                        .collect(),
                ),
                Some(Loadable::Err(_)) => ("error", vec![]),
                Some(Loadable::Loading) | None => ("loading", vec![]),
            };
            Some(StreamGroup {
                addon: &addon.manifest.name,
                state,
                streams,
            })
        })
        .collect();

    let path = &selected.meta_path;
    let stream_video = selected.stream_path.as_ref().map(|path| path.id.as_str());
    Some(match ready {
        Some(item) => Details {
            state: "ready",
            id: &item.preview.id,
            r#type: &item.preview.r#type,
            name: &item.preview.name,
            description: item.preview.description.as_deref(),
            background: item.preview.background.as_ref().map(|url| url.as_str()),
            logo: item.preview.logo.as_ref().map(|url| url.as_str()),
            poster: item.preview.poster.as_ref().map(|url| url.as_str()),
            release_info: item.preview.release_info.as_deref(),
            runtime: item.preview.runtime.as_deref(),
            imdb_rating: links(item, IMDB_LINK_CATEGORY).first().copied(),
            genres: links(item, GENRES_LINK_CATEGORY),
            cast: links(item, "Cast"),
            directors: links(item, "Directors"),
            videos: item
                .videos
                .iter()
                .map(|video| Video {
                    id: &video.id,
                    title: &video.title,
                    season: video.series_info.as_ref().map(|info| info.season),
                    episode: video.series_info.as_ref().map(|info| info.episode),
                    released: video.released.map(|date| date.format("%Y-%m-%d").to_string()),
                    thumbnail: video.thumbnail.as_deref(),
                    overview: video.overview.as_deref(),
                    watched: model
                        .meta_details
                        .watched
                        .as_ref()
                        .is_some_and(|watched| watched.get_video(&video.id)),
                    progress: model
                        .meta_details
                        .library_item
                        .as_ref()
                        .filter(|library| {
                            library.state.video_id.as_deref() == Some(video.id.as_str())
                                && library.state.duration > 0
                        })
                        .map(|library| {
                            (library.state.time_offset.saturating_mul(1000) / library.state.duration)
                                .min(1000) as u32
                        }),
                })
                .collect(),
            stream_video,
            stream_groups,
        },
        None => Details {
            state: if failed { "error" } else { "loading" },
            id: &path.id,
            r#type: &path.r#type,
            name: "",
            description: None,
            background: None,
            logo: None,
            poster: None,
            release_info: None,
            runtime: None,
            imdb_rating: None,
            genres: vec![],
            cast: vec![],
            directors: vec![],
            videos: vec![],
            stream_video,
            stream_groups,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real info hash and the same forty characters of it in writing.
    const HEX: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";
    const HASH: [u8; 20] = [
        0xdd, 0x82, 0x55, 0xec, 0xdc, 0x7c, 0xa5, 0x5f, 0xb0, 0xbb, 0xf8, 0x13, 0x23, 0xd8, 0x70,
        0x62, 0xdb, 0x1f, 0x6d, 0x1c,
    ];

    /// A magnet parameter's value as it goes on a wire, which is how it is looked for in
    /// a written link.
    fn written(value: &str) -> String {
        String::from_utf8(crate::torrent::magnet::percent_encode(value)).expect("as it was written")
    }

    #[test]
    fn a_torrent_stream_is_given_the_address_its_reader_opens() {
        let trackers = vec![
            "udp://tracker.opentrackr.org:1337/announce".to_owned(),
            "https://tracker.example.org:8443/announce".to_owned(),
        ];
        let parts = vec!["Sample".to_owned(), "1080p".to_owned()];
        let (url, unsupported) = torrent_stream(
            &HASH,
            &trackers,
            Some(3),
            &parts,
            Some("Some Film"),
            Some(8_589_934_592),
        );
        assert_eq!(unsupported, None);

        let url = url.expect("a torrent is given an address to play");
        assert!(url.starts_with("magnet:?xt=urn:btih:"), "{url}");
        assert!(url.contains(HEX), "{url} does not name the torrent");
        for tracker in &trackers {
            assert!(
                url.contains(&format!("tr={}", written(tracker))),
                "{url} has no tracker for {tracker}"
            );
        }
        assert!(
            url.contains(&format!("dn={}", written("Some Film"))),
            "{url}"
        );
        assert!(url.contains("fileIdx=3"), "{url}");
        assert!(url.contains("fileMustInclude=Sample"), "{url}");
        assert!(url.contains("fileMustInclude=1080p"), "{url}");
    }

    #[test]
    fn a_torrent_with_nothing_but_its_hash_is_still_a_magnet() {
        let (url, unsupported) = torrent_stream(&HASH, &[], None, &[], None, None);
        assert_eq!(unsupported, None);
        assert_eq!(
            url.as_deref(),
            Some(format!("magnet:?xt=urn:btih:{HEX}").as_str())
        );
    }

    #[test]
    fn a_torrent_without_a_file_or_a_size_leaves_both_out() {
        let trackers = vec!["udp://tracker.example.org:1337/announce".to_owned()];
        let (url, unsupported) = torrent_stream(&HASH, &trackers, None, &[], None, None);
        assert_eq!(unsupported, None);

        let url = url.expect("a torrent is given an address to play");
        assert_eq!(url.matches("tr=").count(), trackers.len(), "{url}");
        assert!(!url.contains("fileIdx="), "{url} names a file");
        assert!(
            !url.contains("fileMustInclude="),
            "{url} names a part of a file"
        );
        assert!(!url.contains("dn="), "{url} carries a name");
        assert!(!url.contains("xl="), "{url} has a size");
    }

    #[test]
    fn every_tracker_an_add_on_gives_is_written() {
        let trackers = vec![
            "udp://tracker.one.example:1337/announce".to_owned(),
            "udp://tracker.two.example:1337/announce".to_owned(),
            "https://tracker.three.example/announce".to_owned(),
        ];
        let (url, _) = torrent_stream(&HASH, &trackers, Some(0), &["Part".to_owned()], None, None);
        let url = url.expect("a torrent is given an address to play");
        assert_eq!(url.matches("tr=").count(), 3, "{url}");
    }
}
