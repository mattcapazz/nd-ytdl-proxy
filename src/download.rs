use std::collections::HashSet;
use std::sync::Mutex;

use tokio::process::Command;
use tracing::{info, warn};

use crate::utils::{http_client, music_dir, upstream_url};
use deunicode::deunicode;
use serde_json::Value as JsonValue;

static QUEUED_ARTISTS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn already_queued(artist: &str) -> bool {
    let mut guard = QUEUED_ARTISTS.lock().unwrap();
    let set = guard.get_or_insert_with(HashSet::new);
    let key = deunicode(artist).to_lowercase();
    !set.insert(key)
}

pub async fn download_and_scan(
    video_id: &str,
    artist: &str,
    title: &str,
    raw_query: &str,
    user: &str,
    yt_metadata: Option<JsonValue>,
) -> anyhow::Result<()> {
    let base = music_dir();
    let safe_artist = crate::utils::sanitize_filename(artist);

    // strip leading "Artist - " or "Artist: " prefix from title when it duplicates the artist name
    let title = crate::utils::strip_artist_prefix(artist, title);
    let title = title.as_str();

    let safe_title = crate::utils::sanitize_filename(title);

    let artist_dir = crate::utils::find_artist_dir(base, artist);
    let dest = format!("{}/{}.opus", artist_dir, safe_title);

    if std::path::Path::new(&dest).exists() {
        info!("yt {}: already on disk, skipping", video_id);
        // still fix artist/title tags if they were embedded incorrectly from YouTube metadata
        let (_, _, cur_artist) = crate::metadata::read_tags(&dest).await;
        let cur_title = crate::metadata::read_title(&dest).await;
        let artist_ok = cur_artist
            .as_deref()
            .map(|a| a.eq_ignore_ascii_case(artist))
            .unwrap_or(false);
        let title_ok = cur_title
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case(title))
            .unwrap_or(false);
        if !artist_ok || !title_ok {
            if !artist_ok {
                warn!(
                    "fixing artist tag in {}: was {:?}, should be {}",
                    dest, cur_artist, artist
                );
            }
            if !title_ok {
                warn!(
                    "fixing title tag in {}: was {:?}, should be {}",
                    dest, cur_title, title
                );
            }
            let _ = crate::metadata::write_tags(&dest, "", &[], artist, title, "", None).await;
        }
        trigger_scan().await;
        return Ok(());
    }

    tokio::fs::create_dir_all(&artist_dir).await?;

    info!("yt {}: downloading to {}", video_id, dest);

    let output_template = format!("{}/{}.%(ext)s", artist_dir, safe_title);
    let archive_path = format!("{}/archive.txt", artist_dir);

    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "--no-playlist",
        "--download-archive",
        &archive_path,
        "--no-overwrites",
        "--no-post-overwrites",
        "-f",
        "bestaudio/best",
        "--extract-audio",
        "--audio-format",
        "opus",
        "--audio-quality",
        "128K",
        "--embed-thumbnail",
        "-o",
        &output_template,
    ]);

    let status = cmd
        .arg(format!("https://youtu.be/{}", video_id))
        .status()
        .await?;

    anyhow::ensure!(status.success(), "yt-dlp exited with {}", status);

    if !std::path::Path::new(&dest).exists() {
        // yt-dlp skipped via archive but the file isn't at the expected path (e.g. renamed)
        warn!(
            "yt {}: archive skipped but {} not found, skipping metadata",
            video_id, dest
        );
        return Ok(());
    }

    info!("yt {}: download complete", video_id);

    // fetch lastfm album/genres (we won't rely on API releasedate)
    let (lfm_album, lfm_image, lfm_genres, _lfm_date, lfm_track_number) =
        crate::lastfm::lookup(artist, title).await;

    // read existing tags and date to avoid clobbering and to determine if we should try album lookups
    let (cur_album, cur_genre, cur_artist) = crate::metadata::read_tags(&dest).await;
    let cur_date = crate::metadata::read_date(&dest).await;

    // if Last.fm provided an image URL, embed it (overwrite youtube thumbnail when possible)
    if let Some(img) = lfm_image.as_deref() {
        if !img.is_empty() {
            info!(
                "yt {}: embedding lastfm cover from {} into {}",
                video_id, img, dest
            );
            match crate::metadata::embed_picture(&dest, img).await {
                Ok(()) => info!("yt {}: embedded lastfm cover for {}", video_id, dest),
                Err(e) => warn!(
                    "yt {}: failed embedding lastfm cover for {} from {}: {}",
                    video_id, dest, img, e
                ),
            }
        }
    }

    // parse YouTube metadata for possible album/date hints
    // reuse pre-fetched metadata from search when available
    let (yt_album, yt_date) = if let Some(ref meta) = yt_metadata {
        extract_album_and_date_from_json(meta)
    } else {
        match get_yt_album_and_date(video_id).await {
            Ok(v) => v,
            Err(e) => {
                warn!("yt {}: failed parsing youtube metadata: {}", video_id, e);
                (None, None)
            }
        }
    };

    // choose album/genre/artist to write (prefer Last.fm values, otherwise keep existing tags)
    let album_to_write = if let Some(a) = lfm_album.as_deref().filter(|a| !a.is_empty()) {
        a.to_string()
    } else if let Some(a) = yt_album.as_deref().filter(|a| !a.is_empty()) {
        a.to_string()
    } else {
        cur_album.as_deref().unwrap_or("").to_string()
    };
    let genres_to_write: Vec<String> = if !lfm_genres.is_empty() {
        lfm_genres
    } else if let Some(g) = cur_genre.filter(|g| !g.is_empty()) {
        vec![g]
    } else {
        vec![]
    };
    let artist_to_write = if !artist.is_empty() {
        artist.to_string()
    } else {
        cur_artist.unwrap_or_default()
    };

    // resolve release date: prefer Last.fm album scrape, then existing album->album.getInfo, then YouTube
    let mut date_to_write = String::new();
    if let Some(a) = lfm_album.as_deref().filter(|s| !s.is_empty()) {
        if let Some(d) = crate::lastfm::album_published(artist, a, Some(title)).await {
            date_to_write = d;
        }
    } else if let Some(existing_album) = cur_album.as_deref().filter(|s| !s.is_empty()) {
        // if we already have an album tag but no date, try to fetch the album page for a release date
        if cur_date.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
            if let Some(pubdate) =
                crate::lastfm::album_published(artist, existing_album, Some(title)).await
            {
                date_to_write = pubdate;
            }
        }
    }

    // fallback to YouTube description 'Released on: YYYY-MM-DD'
    if date_to_write.is_empty() {
        if let Some(d) = yt_date {
            date_to_write = d;
        }
    }

    if !album_to_write.is_empty()
        || !date_to_write.is_empty()
        || !genres_to_write.is_empty()
        || !title.is_empty()
    {
        match crate::metadata::write_tags(
            &dest,
            &album_to_write,
            &genres_to_write,
            &artist_to_write,
            title,
            date_to_write.as_str(),
            lfm_track_number,
        )
        .await
        {
            Ok(()) => {
                info!("yt {}: wrote tags for {}", video_id, dest);
            }
            Err(e) => {
                warn!("yt {}: failed writing tags to {}: {}", video_id, dest, e);
                // attempt to fix via metadata fixer
                let _ = crate::metadata::fix_file(&dest, &safe_artist, &safe_title).await;
            }
        }
    }

    // download top-10 for each individual artist, not the combined name
    let parts = crate::utils::split_artists(artist);
    let individual_artists = if parts.len() > 1 {
        parts
    } else {
        vec![safe_artist.clone()]
    };
    for part in individual_artists {
        if !already_queued(&part) {
            let rq = raw_query.to_string();
            let u = user.to_string();
            info!("queuing top 10 download: {}", part);
            let p = part.clone();
            tokio::spawn(async move {
                if let Err(e) = download_artist_top10(&p, &rq, &u).await {
                    warn!("artist top 10 download failed for {}: {}", p, e);
                }
            });
        }
    }

    trigger_scan().await;
    info!("yt {}: triggered navidrome scan", video_id);

    Ok(())
}

// extract release date from yt-dlp JSON (description "Released on:" or release_date field)
fn extract_release_date(v: &JsonValue) -> Option<String> {
    if let Some(desc) = v["description"].as_str() {
        let lower = desc.to_lowercase();
        if let Some(idx) = lower.find("released on:") {
            let after = &desc[idx + "released on:".len()..];
            for token in after.split_whitespace() {
                let tok = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '-');
                if tok.len() == 10
                    && tok.chars().nth(4) == Some('-')
                    && tok.chars().nth(7) == Some('-')
                {
                    if let Some(fmt) = format_yt_date(tok) {
                        return Some(fmt);
                    }
                }
            }
        }
    }

    if let Some(rd) = v["release_date"].as_str() {
        if !rd.trim().is_empty() {
            if let Some(fmt) = format_yt_date(rd) {
                return Some(fmt);
            }
        }
    }

    None
}

// extract album and date from a pre-fetched yt-dlp JSON object
pub(crate) fn extract_album_and_date_from_json(v: &JsonValue) -> (Option<String>, Option<String>) {
    if let Some(album_field) = v["album"].as_str() {
        if !album_field.trim().is_empty() {
            return (Some(album_field.to_string()), extract_release_date(v));
        }
    }

    if let Some(desc) = v["description"].as_str() {
        let lines: Vec<&str> = desc
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();

        for i in 0..lines.len() {
            if lines[i].contains('\u{00b7}') {
                if i + 1 < lines.len() {
                    let candidate = lines[i + 1];
                    let low = candidate.to_lowercase();
                    if !low.contains("released on")
                        && !low.contains("main artist")
                        && !low.contains('\u{00a9}')
                        && !low.contains('\u{2117}')
                    {
                        return (Some(candidate.to_string()), extract_release_date(v));
                    }
                }
            }
        }

        for line in &lines {
            let low = line.to_lowercase();
            if low.starts_with("album:") {
                let rest = line["album:".len()..].trim();
                if !rest.is_empty() {
                    return (Some(rest.to_string()), extract_release_date(v));
                }
            }
            if low.contains("from the album") {
                if let Some(idx) = low.find("from the album") {
                    let rest = line[idx + "from the album".len()..]
                        .trim()
                        .trim_matches(':')
                        .trim();
                    if !rest.is_empty() {
                        return (Some(rest.to_string()), extract_release_date(v));
                    }
                }
            }
        }
    }

    (None, extract_release_date(v))
}

// fetch yt-dlp JSON for a video and extract album/date
async fn get_yt_album_and_date(video_id: &str) -> anyhow::Result<(Option<String>, Option<String>)> {
    let output = Command::new("yt-dlp")
        .args(["-j", &format!("https://youtu.be/{}", video_id)])
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if stdout.trim().is_empty() {
        return Ok((None, None));
    }
    let v: JsonValue = serde_json::from_str(&stdout).map_err(|e| anyhow::anyhow!(e))?;
    Ok(extract_album_and_date_from_json(&v))
}

pub(crate) fn format_yt_date(s: &str) -> Option<String> {
    // expect YYYY-MM-DD
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() == 3 {
        let year = parts[0];
        let month = parts[1];
        let day = parts[2].trim_start_matches('0');
        if let Some(mname) = month_number_to_name(month) {
            return Some(format!("{} {} {}", day, mname, year));
        }
    }
    None
}

pub(crate) fn month_number_to_name(m: &str) -> Option<&'static str> {
    match m {
        "01" | "1" => Some("January"),
        "02" | "2" => Some("February"),
        "03" | "3" => Some("March"),
        "04" | "4" => Some("April"),
        "05" | "5" => Some("May"),
        "06" | "6" => Some("June"),
        "07" | "7" => Some("July"),
        "08" | "8" => Some("August"),
        "09" | "9" => Some("September"),
        "10" => Some("October"),
        "11" => Some("November"),
        "12" => Some("December"),
        _ => None,
    }
}

async fn download_artist_top10(artist: &str, _: &str, user: &str) -> anyhow::Result<()> {
    let base = music_dir();
    let artist_dir = crate::utils::find_artist_dir(base, artist);
    let archive_path = format!("{}/archive.txt", artist_dir);

    tokio::fs::create_dir_all(&artist_dir).await?;

    info!("fetching top 10 tracks from Last.fm for: {}", artist);
    let top = crate::lastfm::top_tracks(artist, 10).await;
    info!("top tracks resolved for {}: {} tracks", artist, top.len());

    // record all top-10 songs in this user's library
    if !user.is_empty() {
        let songs: Vec<(String, String)> = top
            .iter()
            .map(|(name, _)| (artist.to_string(), name.clone()))
            .collect();
        crate::db::add_songs(user, &songs);
    }

    for (track_name, _) in &top {
        let safe_title = crate::utils::sanitize_filename(track_name);
        let dest = format!("{}/{}.opus", artist_dir, safe_title);

        if std::path::Path::new(&dest).exists() {
            continue;
        }

        let search_query = format!("{} - {}", artist, track_name);
        let output_template = format!("{}/{}.%(ext)s", artist_dir, safe_title);

        let status = Command::new("yt-dlp")
            .args([
                &format!("ytsearch1:{}", search_query),
                "--download-archive",
                &archive_path,
                "--no-overwrites",
                "--no-post-overwrites",
                "--sleep-interval",
                "3",
                "-f",
                "bestaudio/best",
                "--extract-audio",
                "--audio-format",
                "opus",
                "--audio-quality",
                "128K",
                "--embed-thumbnail",
                "-o",
                &output_template,
            ])
            .status()
            .await?;

        if !status.success() {
            warn!("yt-dlp failed for '{}' by {}", track_name, artist);
            continue;
        }

        if std::path::Path::new(&dest).exists() {
            crate::metadata::fix_file(&dest, artist, &safe_title).await;
            trigger_scan().await;
            info!("top 10 {}: scanned after '{}'", artist, track_name);
        }
    }

    info!("top 10 download complete for: {}", artist);

    Ok(())
}

pub fn delete_song_file(artist: &str, title: &str) {
    let base = music_dir();
    let safe_artist = crate::utils::sanitize_filename(artist);
    let safe_title = crate::utils::sanitize_filename(title);
    let path = format!("{}/{}/{}.opus", base, safe_artist, safe_title);
    if std::path::Path::new(&path).exists() {
        std::fs::remove_file(&path).ok();
        info!("deleted song file: {}", path);
    }
}

pub async fn trigger_scan() {
    let auth = crate::utils::admin_auth_query();
    let url = format!("{}/rest/startScan.view?{}", upstream_url(), auth);
    let _ = http_client().get(&url).send().await;
}

// same as download_and_scan but without queuing artist top 10
pub async fn download_song(
    video_id: &str,
    artist: &str,
    title: &str,
    user: &str,
    yt_metadata: Option<JsonValue>,
) -> anyhow::Result<()> {
    let base = music_dir();
    let safe_artist = crate::utils::sanitize_filename(artist);

    let title = crate::utils::strip_artist_prefix(artist, title);
    let title = title.as_str();

    let safe_title = crate::utils::sanitize_filename(title);
    let artist_dir = crate::utils::find_artist_dir(base, artist);
    let dest = format!("{}/{}.opus", artist_dir, safe_title);

    if std::path::Path::new(&dest).exists() {
        info!("yt {}: already on disk, skipping", video_id);
        let (_, _, cur_artist) = crate::metadata::read_tags(&dest).await;
        let cur_title = crate::metadata::read_title(&dest).await;
        let artist_ok = cur_artist
            .as_deref()
            .map(|a| a.eq_ignore_ascii_case(artist))
            .unwrap_or(false);
        let title_ok = cur_title
            .as_deref()
            .map(|t| t.eq_ignore_ascii_case(title))
            .unwrap_or(false);
        if !artist_ok || !title_ok {
            if !artist_ok {
                warn!(
                    "fixing artist tag in {}: was {:?}, should be {}",
                    dest, cur_artist, artist
                );
            }
            if !title_ok {
                warn!(
                    "fixing title tag in {}: was {:?}, should be {}",
                    dest, cur_title, title
                );
            }
            let _ = crate::metadata::write_tags(&dest, "", &[], artist, title, "", None).await;
        }
        trigger_scan().await;
        return Ok(());
    }

    tokio::fs::create_dir_all(&artist_dir).await?;

    info!("yt {}: downloading to {}", video_id, dest);

    let output_template = format!("{}/{}.%(ext)s", artist_dir, safe_title);
    let archive_path = format!("{}/archive.txt", artist_dir);

    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "--no-playlist",
        "--download-archive",
        &archive_path,
        "--no-overwrites",
        "--no-post-overwrites",
        "-f",
        "bestaudio/best",
        "--extract-audio",
        "--audio-format",
        "opus",
        "--audio-quality",
        "128K",
        "--embed-thumbnail",
        "-o",
        &output_template,
    ]);

    let status = cmd
        .arg(format!("https://youtu.be/{}", video_id))
        .status()
        .await?;

    anyhow::ensure!(status.success(), "yt-dlp exited with {}", status);

    if !std::path::Path::new(&dest).exists() {
        warn!(
            "yt {}: archive skipped but {} not found, skipping metadata",
            video_id, dest
        );
        return Ok(());
    }

    info!("yt {}: download complete", video_id);

    let (lfm_album, lfm_image, lfm_genres, _lfm_date, lfm_track_number) =
        crate::lastfm::lookup(artist, title).await;

    let (cur_album, cur_genre, cur_artist) = crate::metadata::read_tags(&dest).await;
    let cur_date = crate::metadata::read_date(&dest).await;

    if let Some(img) = lfm_image.as_deref() {
        if !img.is_empty() {
            info!(
                "yt {}: embedding lastfm cover from {} into {}",
                video_id, img, dest
            );
            match crate::metadata::embed_picture(&dest, img).await {
                Ok(()) => info!("yt {}: embedded lastfm cover for {}", video_id, dest),
                Err(e) => warn!(
                    "yt {}: failed embedding lastfm cover for {} from {}: {}",
                    video_id, dest, img, e
                ),
            }
        }
    }

    let (yt_album, yt_date) = if let Some(ref meta) = yt_metadata {
        extract_album_and_date_from_json(meta)
    } else {
        match get_yt_album_and_date(video_id).await {
            Ok(v) => v,
            Err(e) => {
                warn!("yt {}: failed parsing youtube metadata: {}", video_id, e);
                (None, None)
            }
        }
    };

    let album_to_write = if let Some(a) = lfm_album.as_deref().filter(|a| !a.is_empty()) {
        a.to_string()
    } else if let Some(a) = yt_album.as_deref().filter(|a| !a.is_empty()) {
        a.to_string()
    } else {
        cur_album.as_deref().unwrap_or("").to_string()
    };
    let genres_to_write: Vec<String> = if !lfm_genres.is_empty() {
        lfm_genres
    } else if let Some(g) = cur_genre.filter(|g| !g.is_empty()) {
        vec![g]
    } else {
        vec![]
    };
    let artist_to_write = if !artist.is_empty() {
        artist.to_string()
    } else {
        cur_artist.unwrap_or_default()
    };

    let mut date_to_write = String::new();
    if let Some(a) = lfm_album.as_deref().filter(|s| !s.is_empty()) {
        if let Some(d) = crate::lastfm::album_published(artist, a, Some(title)).await {
            date_to_write = d;
        }
    } else if let Some(existing_album) = cur_album.as_deref().filter(|s| !s.is_empty()) {
        if cur_date.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
            if let Some(pubdate) =
                crate::lastfm::album_published(artist, existing_album, Some(title)).await
            {
                date_to_write = pubdate;
            }
        }
    }

    if date_to_write.is_empty() {
        if let Some(d) = yt_date {
            date_to_write = d;
        }
    }

    if !album_to_write.is_empty()
        || !date_to_write.is_empty()
        || !genres_to_write.is_empty()
        || !title.is_empty()
    {
        match crate::metadata::write_tags(
            &dest,
            &album_to_write,
            &genres_to_write,
            &artist_to_write,
            title,
            date_to_write.as_str(),
            lfm_track_number,
        )
        .await
        {
            Ok(()) => {
                info!("yt {}: wrote tags for {}", video_id, dest);
            }
            Err(e) => {
                warn!("yt {}: failed writing tags to {}: {}", video_id, dest, e);
                let _ = crate::metadata::fix_file(&dest, &safe_artist, &safe_title).await;
            }
        }
    }

    trigger_scan().await;
    info!("yt {}: triggered navidrome scan", video_id);

    // track in user library
    if !user.is_empty() {
        crate::db::add_song(user, artist, title);
    }

    Ok(())
}
