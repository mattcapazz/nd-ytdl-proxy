use std::collections::HashMap;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::ogg::{OggPictureStorage, OpusFile};
use lofty::picture::{MimeType, Picture, PictureType};
use tracing::{info, warn};

use crate::utils::music_dir;

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

// writes tags with multi-valued ARTISTS for Navidrome
fn write_tags_sync(path: &str, album: &str, genre: &str, artist: &str) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut opus = OpusFile::read_from(&mut file, ParseOptions::default())?;
    let vc = opus.vorbis_comments_mut();

    if !album.is_empty() {
        vc.remove("ALBUM").count();
        vc.push("ALBUM".to_owned(), album.to_owned());
    }
    if !genre.is_empty() {
        vc.remove("GENRE").count();
        vc.push("GENRE".to_owned(), genre.to_owned());
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

    opus.save_to(&mut file, WriteOptions::default())?;
    Ok(())
}

// embeds cover art from a URL into an opus file
pub async fn embed_picture(path: &str, image_url: &str) -> anyhow::Result<()> {
    let bytes = crate::utils::http_client()
        .get(image_url)
        .send()
        .await?
        .bytes()
        .await?;

    let mime = if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        MimeType::Png
    } else {
        MimeType::Jpeg
    };

    let pic = Picture::unchecked(bytes.to_vec())
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
        opus.vorbis_comments_mut().insert_picture(pic, None)?;
        opus.save_to(&mut file, WriteOptions::default())?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn write_tags(path: &str, album: &str, genre: &str, artist: &str) -> anyhow::Result<()> {
    let path = path.to_owned();
    let album = album.to_owned();
    let genre = genre.to_owned();
    let artist = artist.to_owned();
    tokio::task::spawn_blocking(move || write_tags_sync(&path, &album, &genre, &artist)).await?
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
    if !needs_fix(&album, &file_artist, &genre) {
        return;
    }

    let (lfm_album, _, lfm_genres) = crate::lastfm::lookup(artist, &clean_title).await;

    let new_album = lfm_album
        .as_deref()
        .filter(|a| !a.is_empty())
        .or_else(|| {
            album
                .as_deref()
                .filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case("youtube"))
        })
        .unwrap_or("");
    let new_genre = lfm_genres
        .first()
        .map(String::as_str)
        .or_else(|| {
            genre
                .as_deref()
                .filter(|g| !g.is_empty() && !g.eq_ignore_ascii_case("music"))
        })
        .unwrap_or("");

    let artist_to_write = if !artist.is_empty() {
        artist
    } else {
        file_artist
            .as_deref()
            .filter(|a| !a.eq_ignore_ascii_case("na"))
            .unwrap_or("")
    };

    if new_album.is_empty() && artist_to_write.is_empty() {
        info!("metadata fixer: no album or artist for '{}'", clean_title);
        unresolvable.insert(path.to_string(), "no lastfm data".to_string());
        save_unresolvable(&unresolvable).await;
        return;
    }

    let album_to_write = if new_album.is_empty() {
        album.as_deref().unwrap_or("")
    } else {
        new_album
    };
    let genre_to_write = if new_genre.is_empty() {
        genre
            .as_deref()
            .filter(|g| !g.eq_ignore_ascii_case("music"))
            .unwrap_or("")
    } else {
        new_genre
    };

    match write_tags(path, album_to_write, genre_to_write, artist_to_write).await {
        Ok(()) => info!("metadata fixer: updated tags for '{}'", title),
        Err(e) => warn!("metadata fixer: write failed for '{}': {}", title, e),
    }
}
