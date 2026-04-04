# nd-ytdl-proxy

Proxy for [Navidrome](https://www.navidrome.org/) that lets you search and download music on the fly. You search for a song, it finds matches via Last.fm, downloads the audio from YouTube with [yt-dlp](https://github.com/yt-dlp/yt-dlp), and saves it to disk

This is a portfolio/family project. I was looking for an open-source alternative to Spotify and also wanted to store music for preservation purposes, found Navidrome, then discovered [Navic](https://github.com/paigely/Navic) through their client apps page. Any Subsonic client like [Substreamer](https://github.com/ghenry22/substreamer) or [Yuzic](https://github.com/eftpmc/yuzic) *should* work fine too

*[Funkwhale](https://funkwhale.audio/) was considered later (as a Navidrome replacement) but at that point was already too far in*

### How it works

Proxy sits in front of Navidrome and intercepts Subsonic API calls:

- **search** - hits Navidrome first, then mixes in Last.fm results
- **stream** - streams audio from YouTube via yt-dlp, downloads the mp3 in the background with metadata (album art, genre)
- **cover art** - fetched from Last.fm and cached locally
- ...
- everything else gets forwarded straight to Navidrome

The proxy also adds a few features on top:

- **Auto-populate** - first time you stream a song from an artist, it grabs their top 10 tracks from Last.fm too
- **Trash Queue** - each user has a "Trash Queue" playlist; adding a song hides it from their library and deletes it from the server if no other users have it

Both containers share the same `/music` volume so the proxy can save downloads where Navidrome reads from

### Setup

1. `git clone https://github.com/mattcapazz/nd-ytdl-proxy.git && cd nd-ytdl-proxy`
2. `cp .env.example .env`
3. `docker compose up -d`
4. Visit http://localhost:4533 and create your Navidrome admin account
5. Fill in your [Last.fm API key](https://www.last.fm/api/account/create) *(free)* and admin credentials in `.env`
6. `docker compose up -d`

Proxy runs on port **4532**, Navidrome on **4533**. **Point your Subsonic client at the proxy**