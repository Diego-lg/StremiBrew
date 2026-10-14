<p align="center">
  <img src="docs/images/stremibrew-logo.png" alt="StremiBrew" width="460">
</p>

<p align="center">
  <a href="https://github.com/stupiditytries/StremiBrew/actions/workflows/build.yml"><img src="https://github.com/stupiditytries/StremiBrew/actions/workflows/build.yml/badge.svg?branch=main" alt="Build status"></a>&nbsp;
  <img src="https://img.shields.io/badge/Tokens%20Wasted-9%20Kazillion%2B-blueviolet" alt="Tokens Wasted: 9 Kazillion+">&nbsp;
  <a href="LICENSE.md"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT"></a>&nbsp;
  <img src="https://img.shields.io/badge/built%20with-C%2B%2B20%20%7C%20Rust-8B5A2B" alt="Built with C++20 and Rust">
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/images/stremibrew-dark.png">
    <img src="docs/images/stremibrew-light.png" alt="StremiBrew on a TV and a Nintendo Switch" width="100%">
  </picture>
</p>

<hr>

<h3 align="center">Cross-Platform Unofficial Stremio Client</h3>

<hr>

StremiBrew is a cross-platform, third-party Stremio client that aims to bring the native Stremio TV experience to consoles and builds on it with features only console hardware can handle.

**To get it running on your console, see the [installation guide](INSTALLATION.md).** To build it yourself, see the [build guide](BUILD.md).

The UI is inspired by my favourite elements of Stremio's official web-based TV interface and its Android TV app.

It currently supports the **PS5** and **Switch**, with features including:

- Stremio account sign-in with a link code, with your library, add-ons and watch progress synced through the official `stremio-core`
- Trailers that play automatically on the home screen as you move between titles (PS5)
- Home board, Discover, Library, Calendar, Add-ons and search with the console's on-screen keyboard
- Title pages with seasons, episodes and stream lists, for HTTP streams, and for torrents on the Switch (see [torrents](#torrents))
- Player with seeking, scrubber previews, audio track selection, embedded and add-on subtitles with delay and styling, and resume
- An animated UI, with transitions between screens and optional sound effects
- 4K output and HDR-to-SDR tone mapping (PS5)
- Experimental subtitle auto-calibration, which times add-on subtitles to the dialogue using on-device speech recognition (PS5)
- A handheld UI that switches on automatically when undocked, and a stream quality limit (Switch)

PS4 and Wii U support is planned in the near future.

## Torrents

On the Switch, a stream that carries a torrent plays instead of being marked unplayable. The client is in `shared/core-bridge/src/torrent`: it finds peers through the link's trackers (HTTP and UDP), asks them for the torrent's own description when the link is a magnet, fetches the pieces around where the player is reading, checks each piece against the hash the torrent gives, and keeps everything on the card in `sdmc:/switch/StremiBrew/torrents` — which is also what a link with no trackers plays from, when it was played before.

**Downloaded films** in Settings → Playback says how much of the card that folder may take: nothing, 2 GB, 4 GB, 8 GB (the default), 16 GB or 32 GB. What is over it goes, the least recently watched film first, and never the one being watched. It is trimmed whenever the setting is lowered and whenever a film is closed, so the card gives the room back without anything being asked of it.

What it does not do: seed (nothing is ever uploaded, so a leecher's ratio is not spoiled by this one and no port is opened), find peers without trackers (there is no DHT), or play a private torrent (BEP 27). The scrubbing pictures, which need a reader of their own, are not made for a torrent, so the scrub bar has no thumbnails on one. The PS5 port has no player for them yet, and says so rather than offering a stream it cannot open.

## Demonstration

<p align="center">
  <img src="docs/images/home.gif" alt="The home screen, with a trailer starting behind the featured title" width="49%">
  <img src="docs/images/episodes.gif" alt="A series' page: seasons, episodes and streams" width="49%">
</p>

<p align="center">
  <img src="docs/images/menu.gif" alt="The player: seeking with scrub previews, and the subtitles menu" width="49%">
  <img src="docs/images/discover.png" alt="Discover, with a film chosen and its details beside the grid" width="49%">
</p>

<p align="center">
  <img src="docs/images/calendar.png" alt="The calendar of upcoming episodes" width="49%">
  <img src="docs/images/settings.gif" alt="Settings: account, playback, subtitles, auto-calibrate and interface" width="49%">
</p>

## Credits and disclaimers

### Credits

- **[Stremio](https://www.stremio.com/)**: StremiBrew is a client for Stremio and would not exist without it. Accounts, the library, add-ons, catalogs and watch progress are all handled by Stremio's own open-source [`stremio-core`](https://github.com/Stremio/stremio-core), used under its MIT license. The interface takes after Stremio's web-based TV interface and its Android TV app.
- **[borealis](https://github.com/natinusala/borealis)** by natinusala, and **[xfangfang's fork of borealis](https://github.com/xfangfang/borealis)**: StremiBrew's interface is drawn with the NanoVG renderer kept in that fork. It is what lets the UI render natively on each console, smoothly and responsively, with no web view in between.
- **[NanoVG](https://github.com/memononen/nanovg)** by Mikko Mononen, and **[FFmpeg](https://ffmpeg.org/)**, which plays the video.
- **[しの (@shino_illust)](https://www.instagram.com/shino_illust)**: the artwork behind StremiBrew on the PS5 home screen is their [Stremio Art Contest illustration](https://ibispaint.com/art/995888552/).
- The artwork in the pictures on this page comes from [Blender Studio's open movies](https://studio.blender.org/films/) (© Blender Foundation, CC BY), NASA/ESA/CSA/STScI imagery and public-domain films. These, and the 3D models in the picture at the top, are credited in [docs/images/CREDITS.md](docs/images/CREDITS.md).

### Support Stremio

All contributions and support should go to the Stremio team, whose work this is built on. I do not, and will never accept donations for this project.

- [Become a Stremio supporter](https://www.stremio.com/plans)
- [Contribute to `stremio-core`](https://github.com/Stremio/stremio-core)

### Disclaimers

- **Unofficial**: StremiBrew is a third-party project. It is not affiliated with, endorsed by or supported by Stremio, Sony Interactive Entertainment or Nintendo. All names, logos and trademarks belong to their owners. Please do not ask the Stremio team for help with it.
- **No content**: StremiBrew does not host, provide, index or link to any film, series or other media, and includes no third-party add-ons. It only shows what the add-ons on your own Stremio account return. What you install and what you watch are your responsibility, and so is following the law where you live.
- **Torrents (Switch only)**: the Switch port has a torrent client, so a stream that carries one plays: the film is fetched piece by piece and playback starts on the pieces already there, carrying on as the rest arrives, and what has been played is kept on the card for next time. It downloads and does not seed, so it opens no port and no socket is ever opened to answer one; a private torrent is not played, its hashes being a secret between peers of one account. There is no DHT, so a link that arrives with no tracker, no web seed and nothing kept from a run before cannot find peers and says so. The PS5 port has no player for them yet, so its details list still marks them as it cannot play.
- **Artwork**: The posters, logos and stills in the pictures above are freely licensed or in the public domain (see [the credits](docs/images/CREDITS.md)). The titles, ratings and release dates shown are sample data.
- **Homebrew**: StremiBrew runs only on consoles that can already run homebrew. It contains no code from Sony or Nintendo and does nothing to enable piracy of games. Use it at your own risk: it comes with no warranty, as set out in the [license](LICENSE.md).

I am not responsible or liable for any use of this app, for account or console bans, or for any other risk that comes with using it.
