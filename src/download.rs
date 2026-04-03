use std::collections::HashSet;
use std::sync::Mutex;

use tokio::process::Command;
use tracing::{info, warn};

use crate::utils::{http_client, music_dir, upstream_url};

static QUEUED_ARTISTS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn already_queued(artist: &str) -> bool {
    let mut guard = QUEUED_ARTISTS.lock().unwrap();
    let set = guard.get_or_insert_with(HashSet::new);
    !set.insert(artist.to_lowercase())
}

pub fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub async fn download_and_scan(
    video_id: &str,
    artist: &str,
    title: &str,
    raw_query: &str,
    user: &str,
) -> anyhow::Result<()> {
    let base = music_dir();
    let safe_artist = sanitize_filename(artist);
    let safe_title = sanitize_filename(title);

    let artist_dir = format!("{}/{}", base, safe_artist);
    let dest = format!("{}/{}.mp3", artist_dir, safe_title);

    if std::path::Path::new(&dest).exists() {
        info!("yt {}: already on disk, skipping", video_id);
        return Ok(());
    }

    let (_, _, genres) = crate::lastfm::lookup(&safe_artist, &safe_title).await;
    let genre_tag = genres.first().cloned().unwrap_or_default();

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
        "mp3",
        "--audio-quality",
        "128K",
        "--embed-metadata",
        "--embed-thumbnail",
        "--parse-metadata",
        &format!("{}:%(meta_artist)s", safe_artist),
    ]);

    cmd.arg("--ppa").arg(format!(
        "EmbedMetadata+ffmpeg_i:-metadata artist={}",
        safe_artist
    ));

    if !genre_tag.is_empty() {
        cmd.arg("--ppa").arg(format!(
            "EmbedMetadata+ffmpeg_i:-metadata genre={}",
            genre_tag
        ));
    }

    let status = cmd
        .arg("-o")
        .arg(&output_template)
        .arg(format!("https://youtu.be/{}", video_id))
        .status()
        .await?;

    anyhow::ensure!(status.success(), "yt-dlp exited with {}", status);
    info!("yt {}: download complete", video_id);

    crate::metadata::fix_file(&dest, &safe_artist, &safe_title).await;

    if !already_queued(&safe_artist) {
        let ar = safe_artist.clone();
        let rq = raw_query.to_string();
        let u = user.to_string();
        info!("queuing top 10 download: {}", ar);
        tokio::spawn(async move {
            if let Err(e) = download_artist_top10(&ar, &rq, &u).await {
                warn!("artist top 10 download failed for {}: {}", ar, e);
            }
        });
    }

    trigger_scan().await;
    info!("yt {}: triggered navidrome scan", video_id);

    Ok(())
}

async fn download_artist_top10(artist: &str, raw_query: &str, user: &str) -> anyhow::Result<()> {
    let base = music_dir();
    let artist_dir = format!("{}/{}", base, artist);
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
        let safe_title = sanitize_filename(track_name);
        let dest = format!("{}/{}.mp3", artist_dir, safe_title);

        if std::path::Path::new(&dest).exists() {
            continue;
        }

        let search_query = format!("{} - {}", artist, track_name);
        let output_template = format!("{}/{}.%(ext)s", artist_dir, safe_title);

        let ppa_artist = format!("EmbedMetadata+ffmpeg_i:-metadata artist={}", artist);

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
                "mp3",
                "--audio-quality",
                "128K",
                "--embed-metadata",
                "--embed-thumbnail",
                "--parse-metadata",
                &format!("{}:%(meta_artist)s", artist),
                "--ppa",
                &ppa_artist,
                "-o",
                &output_template,
            ])
            .status()
            .await?;

        if !status.success() {
            warn!("yt-dlp failed for '{}' by {}", track_name, artist);
        }
    }

    if let Ok(mut entries) = tokio::fs::read_dir(&artist_dir).await {
        while let Some(entry) = entries.next_entry().await.ok().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("mp3") {
                continue;
            }
            let path_str = path.to_string_lossy().to_string();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            crate::metadata::fix_file(&path_str, artist, &stem).await;
        }
    }

    info!("top 10 download complete for: {}", artist);
    trigger_scan().await;
    info!("top 10 {}: triggered navidrome scan", artist);

    Ok(())
}

pub fn delete_song_file(artist: &str, title: &str) {
    let base = music_dir();
    let safe_artist = sanitize_filename(artist);
    let safe_title = sanitize_filename(title);
    let path = format!("{}/{}/{}.mp3", base, safe_artist, safe_title);
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
