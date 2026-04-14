use std::collections::HashMap;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::ogg::{OggPictureStorage, OpusFile};
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};

use crate::utils::music_dir;
use deunicode::deunicode;
use serde_json::Value as JsonValue;
use tokio::process::Command;
use tracing::{info, warn};

fn unresolvable_path() -> String {
    format!("{}/unresolvable.json", music_dir())
}

async fn load_unresolvable() -> HashMap<String, String> {
    match tokio::fs::read_to_string(unresolvable_path()).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

async fn save_unresolvable(map: &HashMap<String, String>) {
    if let Ok(s) = serde_json::to_string_pretty(map) {
        let _ = tokio::fs::write(unresolvable_path(), s).await;
    }
}

fn read_tags_sync(path: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None, None),
    };
    let opus = match OpusFile::read_from(&mut file, ParseOptions::default()) {
        Ok(f) => f,
        Err(_) => return (None, None, None),
    };
    let vc = opus.vorbis_comments();

    let album = vc.get("ALBUM").map(str::to_string);
    let genre = vc.get("GENRE").map(str::to_string);
    let artist = vc.get("ARTIST").map(str::to_string);

    (album, genre, artist)
}

fn read_date_sync(path: &str) -> Option<String> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return None,
    };
    let opus = match OpusFile::read_from(&mut file, ParseOptions::default()) {
        Ok(f) => f,
        Err(_) => return None,
    };
    let vc = opus.vorbis_comments();
    vc.get("DATE").map(str::to_string)
}

pub async fn read_date(path: &str) -> Option<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_date_sync(&path))
        .await
        .unwrap_or(None)
}

pub async fn read_title(path: &str) -> Option<String> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path).ok()?;
        let opus = OpusFile::read_from(&mut file, ParseOptions::default()).ok()?;
        opus.vorbis_comments().get("TITLE").map(str::to_string)
    })
    .await
    .unwrap_or(None)
}

pub fn read_track_number(path: &str) -> Option<u32> {
    let mut file = std::fs::File::open(path).ok()?;
    let opus = OpusFile::read_from(&mut file, ParseOptions::default()).ok()?;
    let vc = opus.vorbis_comments();
    vc.get("TRACKNUMBER")
        .and_then(|s| s.trim().split('/').next())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
}

pub fn has_picture(path: &str) -> bool {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let opus = match OpusFile::read_from(&mut file, ParseOptions::default()) {
        Ok(f) => f,
        Err(_) => return false,
    };
    !opus.vorbis_comments().pictures().is_empty()
}

pub async fn read_tags(path: &str) -> (Option<String>, Option<String>, Option<String>) {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_tags_sync(&path))
        .await
        .unwrap_or((None, None, None))
}

// writes tags with multi-valued ARTISTS for Navidrome; release date optional
fn write_tags_sync(
    path: &str,
    album: &str,
    genres: &[String],
    artist: &str,
    title: &str,
    date: &str,
    track_number: Option<u32>,
) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut opus = OpusFile::read_from(&mut file, ParseOptions::default())?;
    let vc = opus.vorbis_comments_mut();

    if !title.is_empty() {
        vc.remove("TITLE").count();
        vc.push("TITLE".to_owned(), title.to_owned());
    }
    if !album.is_empty() {
        vc.remove("ALBUM").count();
        vc.push("ALBUM".to_owned(), album.to_owned());
    }
    if !genres.is_empty() {
        vc.remove("GENRE").count();
        for g in genres {
            if !g.is_empty() {
                vc.push("GENRE".to_owned(), g.clone());
            }
        }
    }

    let display = crate::utils::artist_display_name(artist);
    vc.remove("ARTIST").count();
    vc.push("ARTIST".to_owned(), display);

    // multi-valued ARTISTS so Navidrome identifies each artist separately
    let parts = crate::utils::split_artists(artist);
    vc.remove("ARTISTS").count();
    for part in &parts {
        vc.push("ARTISTS".to_owned(), part.clone());
    }

    vc.remove("ALBUMARTIST").count();
    if let Some(primary) = parts.first() {
        vc.push("ALBUMARTIST".to_owned(), primary.clone());
    }

    if !date.is_empty() {
        vc.remove("DATE").count();
        vc.push("DATE".to_owned(), date.to_owned());
    }

    if let Some(n) = track_number.filter(|&n| n > 0) {
        vc.remove("TRACKNUMBER").count();
        vc.push("TRACKNUMBER".to_owned(), n.to_string());
    }

    opus.save_to(&mut file, WriteOptions::default())?;
    Ok(())
}

// embeds cover art from a URL into an opus file
pub async fn embed_picture(path: &str, image_url: &str) -> anyhow::Result<()> {
    info!("embedding cover from '{}' into '{}'", image_url, path);
    let bytes = crate::utils::http_client()
        .get(image_url)
        .send()
        .await?
        .bytes()
        .await?;

    let mime = if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        MimeType::Png
    } else if bytes.starts_with(b"GIF8") {
        MimeType::Gif
    } else {
        MimeType::Jpeg
    };

    let bytes_vec = bytes.to_vec();

    let pic = Picture::unchecked(bytes_vec.clone())
        .pic_type(PictureType::CoverFront)
        .mime_type(mime)
        .build();

    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        let mut opus = OpusFile::read_from(&mut file, ParseOptions::default())?;
        let vc = opus.vorbis_comments_mut();

        // if the primary picture already matches the bytes, nothing to do
        if let Some(pair) = vc.pictures().first() {
            let first_pic = &pair.0;
            if first_pic.data() == bytes_vec.as_slice() {
                return Ok::<_, anyhow::Error>(());
            }
        }

        // remove any existing pictures and set the new one as primary
        vc.remove_pictures();

        let pic_info = match PictureInformation::from_picture(&pic) {
            Ok(info) => info,
            Err(_) => PictureInformation::default(),
        };

        vc.set_picture(0, pic, pic_info);

        opus.save_to(&mut file, WriteOptions::default())?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

pub async fn write_tags(
    path: &str,
    album: &str,
    genres: &[String],
    artist: &str,
    title: &str,
    date: &str,
    track_number: Option<u32>,
) -> anyhow::Result<()> {
    let path = path.to_owned();
    let album = album.to_owned();
    let genres = genres.to_vec();
    let artist = artist.to_owned();
    let title = title.to_owned();
    let date = date.to_owned();
    tokio::task::spawn_blocking(move || {
        write_tags_sync(&path, &album, &genres, &artist, &title, &date, track_number)
    })
    .await?
}

pub fn needs_fix(album: &Option<String>, artist: &Option<String>, genre: &Option<String>) -> bool {
    let bad_album = album
        .as_deref()
        .map(|a| {
            a.is_empty()
                || a.eq_ignore_ascii_case("youtube")
                || a.eq_ignore_ascii_case("youtube4")
                || a.eq_ignore_ascii_case("music")
        })
        .unwrap_or(true);

    let bad_artist = artist
        .as_deref()
        .map(|a| a.is_empty() || a.eq_ignore_ascii_case("na"))
        .unwrap_or(true);

    let bad_genre = genre
        .as_deref()
        .map(|g| g.eq_ignore_ascii_case("music"))
        .unwrap_or(false);

    bad_album || bad_artist || bad_genre
}

// fixes tags on a single opus file if they look like placeholder values
pub async fn fix_file(path: &str, artist: &str, title: &str) {
    let clean_title = crate::title::strip_tags(title);

    let mut unresolvable = load_unresolvable().await;
    if unresolvable.contains_key(path) {
        return;
    }

    let (album, genre, file_artist) = read_tags(path).await;
    let cur_date = read_date(path).await;
    info!(
        "fix_file: path='{}' artist='{}' album={:?} genre={:?} date={:?}",
        path, artist, album, genre, cur_date
    );

    // determine whether tags look ok; don't skip yet because we may still want to embed album art
    let tags_ok = !needs_fix(&album, &file_artist, &genre);

    let (lfm_album, lfm_image, lfm_genres, mut lfm_date, lfm_track_number) =
        crate::lastfm::lookup(artist, &clean_title).await;
    info!(
        "fix_file: lastfm for '{} - {}': album={:?} genres={:?}",
        artist, clean_title, lfm_album, lfm_genres
    );

    // if Last.fm lookup didn't provide a date but we already have an album tag,
    // try a direct album.getInfo lookup for the album's published/release date
    if lfm_date.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
        if let Some(existing_album) = album.as_deref() {
            if let Some(pubdate) =
                crate::lastfm::album_published(artist, existing_album, Some(clean_title.as_str()))
                    .await
            {
                info!(
                    "fix_file: album published date for '{} - {}': {}",
                    artist, existing_album, pubdate
                );
                lfm_date = Some(pubdate);
            }
        }
    }

    // if we still don't have an album from Last.fm, try a YouTube search
    let mut yt_album: Option<String> = None;
    let mut yt_date: Option<String> = None;
    if lfm_album.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
        let search_arg = format!("ytsearch5:{} - {}", artist, clean_title);
        if let Ok(output) = Command::new("yt-dlp")
            .args([&search_arg, "--dump-json", "--no-playlist"])
            .output()
            .await
        {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let mut candidates: Vec<JsonValue> = Vec::new();
            for line in stdout.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<JsonValue>(line) {
                    candidates.push(val);
                }
            }

            let norm_artist = deunicode(artist).to_lowercase();

            let mut chosen: Option<&JsonValue> = None;
            for c in &candidates {
                let uploader = c["uploader"].as_str().unwrap_or("");
                let norm_uploader = deunicode(uploader).to_lowercase();
                if norm_uploader == norm_artist
                    || norm_uploader.contains(&norm_artist)
                    || norm_artist.contains(&norm_uploader)
                {
                    chosen = Some(c);
                    break;
                }
            }
            if chosen.is_none() {
                for c in &candidates {
                    if let Some(desc) = c["description"].as_str() {
                        if desc.to_lowercase().contains("released on:") {
                            chosen = Some(c);
                            break;
                        }
                    }
                }
            }
            if chosen.is_none() {
                chosen = candidates.first();
            }

            if let Some(v) = chosen {
                let (album_res, date_res) = crate::download::extract_album_and_date_from_json(v);
                yt_album = album_res;
                yt_date = date_res;
            }
        }
    }

    let new_album = if let Some(a) = lfm_album.as_deref().filter(|a| !a.is_empty()) {
        a
    } else if let Some(a) = yt_album.as_deref().filter(|a| !a.is_empty()) {
        a
    } else {
        album
            .as_deref()
            .filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case("youtube"))
            .unwrap_or("")
    };
    let new_genres: Vec<String> = if !lfm_genres.is_empty() {
        lfm_genres
    } else if let Some(g) = genre.filter(|g| !g.is_empty() && !g.eq_ignore_ascii_case("music")) {
        vec![g]
    } else {
        vec![]
    };

    let artist_to_write = if !artist.is_empty() {
        artist
    } else {
        file_artist
            .as_deref()
            .filter(|a| !a.eq_ignore_ascii_case("na"))
            .unwrap_or("")
    };

    if new_album.is_empty() && artist_to_write.is_empty() {
        info!(
            "fix_file: no album or artist for '{}', marking unresolvable",
            clean_title
        );
        unresolvable.insert(path.to_string(), "no lastfm data".to_string());
        save_unresolvable(&unresolvable).await;
        return;
    }

    let album_to_write = if new_album.is_empty() {
        album.as_deref().unwrap_or("")
    } else {
        new_album
    };
    // genres_to_write is the final list (already resolved above)

    let date_to_write = if let Some(d) = lfm_date.as_deref() {
        d
    } else if let Some(d) = yt_date.as_deref() {
        d
    } else {
        ""
    };
    info!(
        "fix_file: writing tags for '{}' -> album='{}' genres={:?} artist='{}'",
        path, album_to_write, new_genres, artist_to_write
    );

    // decide on an image to embed: prefer track-level Last.fm image, else try album.getInfo
    let mut image_to_embed: Option<String> = None;
    if let Some(img) = lfm_image.as_deref() {
        if !img.is_empty() {
            image_to_embed = Some(img.to_string());
        }
    }
    if image_to_embed.is_none() && !album_to_write.is_empty() {
        if let Some(ai) =
            crate::lastfm::album_image(artist, album_to_write, Some(&clean_title)).await
        {
            image_to_embed = Some(ai);
        }
    }

    // if tags look ok and a DATE already exists, only embed cover if Last.fm has one; otherwise skip
    if tags_ok && cur_date.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        if let Some(imgurl) = image_to_embed.as_deref() {
            match embed_picture(path, imgurl).await {
                Ok(()) => info!("fix_file: embedded cover for '{}'", title),
                Err(e) => warn!("fix_file: embed failed for '{}': {}", title, e),
            }
        } else {
            info!(
                "fix_file: skipping, tags ok and date present for '{}'",
                path
            );
            return;
        }
    }
    match write_tags(
        path,
        album_to_write,
        &new_genres,
        artist_to_write,
        clean_title.as_str(),
        date_to_write,
        lfm_track_number,
    )
    .await
    {
        Ok(()) => {
            info!("fix_file: updated tags for '{}'", title);
            if let Some(imgurl) = image_to_embed.as_deref() {
                if !imgurl.is_empty() {
                    match embed_picture(path, imgurl).await {
                        Ok(()) => info!("fix_file: embedded cover for '{}'", title),
                        Err(e) => warn!("fix_file: embed failed for '{}': {}", title, e),
                    }
                }
            }
        }
        Err(e) => warn!("fix_file: write failed for '{}': {}", title, e),
    }
}
