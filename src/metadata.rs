use std::collections::HashSet;

use serde_json::Value;
use tokio::process::Command;
use tracing::{info, warn};

use crate::utils::music_dir;

fn unresolvable_path() -> String {
    format!("{}/unresolvable.json", music_dir())
}

async fn load_unresolvable() -> HashSet<String> {
    match tokio::fs::read_to_string(unresolvable_path()).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => HashSet::new(),
    }
}

async fn save_unresolvable(set: &HashSet<String>) {
    if let Ok(s) = serde_json::to_string_pretty(set) {
        let _ = tokio::fs::write(unresolvable_path(), s).await;
    }
}

async fn read_tags(path: &str) -> (Option<String>, Option<String>, Option<String>) {
    let output = match Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_entries",
            "format_tags",
            path,
        ])
        .output()
        .await
    {
        Ok(o) => o,
        Err(_) => return (None, None, None),
    };

    let v: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let tags = &v["format"]["tags"];

    // ID3 writers vary on case
    let album = tags["album"]
        .as_str()
        .or_else(|| tags["ALBUM"].as_str())
        .map(str::to_string);
    let genre = tags["genre"]
        .as_str()
        .or_else(|| tags["GENRE"].as_str())
        .map(str::to_string);
    let artist = tags["artist"]
        .as_str()
        .or_else(|| tags["ARTIST"].as_str())
        .map(str::to_string);

    (album, genre, artist)
}

async fn write_tags(path: &str, album: &str, genre: &str, artist: &str) -> anyhow::Result<()> {
    let tmp = format!("{}.fixing.mp3", path);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            path,
            "-c",
            "copy",
            "-metadata",
            &format!("album={}", album),
            "-metadata",
            &format!("genre={}", genre),
            "-metadata",
            &format!("artist={}", artist),
            "-id3v2_version",
            "3",
            &tmp,
        ])
        .status()
        .await?;
    anyhow::ensure!(status.success(), "ffmpeg tag write failed for {}", path);
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

fn needs_fix(album: &Option<String>, artist: &Option<String>, genre: &Option<String>) -> bool {
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

// fixes tags on a single mp3 file if they look like placeholder values
pub async fn fix_file(path: &str, artist: &str, title: &str) {
    let clean_title = crate::title::strip_tags(title);

    let mut unresolvable = load_unresolvable().await;
    if unresolvable.contains(path) {
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
        unresolvable.insert(path.to_string());
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
