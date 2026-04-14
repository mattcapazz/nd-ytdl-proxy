use std::collections::HashMap;
use std::path::Path;

use nd_ytdl_proxy::{download, lastfm, metadata};

// filter mode parsed from the optional positional argument
enum Filter {
    None,
    Artist(String),
    Song(String), // relative path like "artist/song name"
}

impl Filter {
    fn parse(args: &[String]) -> Self {
        for arg in args {
            if let Some(rest) = arg.strip_prefix("artist:") {
                return Filter::Artist(rest.trim().to_lowercase());
            }
            if let Some(rest) = arg.strip_prefix("song:") {
                return Filter::Song(rest.trim().replace('\\', "/").to_lowercase());
            }
        }
        Filter::None
    }

    fn matches(&self, artist_name: &str, title: &str) -> bool {
        match self {
            Filter::None => true,
            Filter::Artist(a) => artist_name.to_lowercase() == *a,
            Filter::Song(s) => {
                let candidate = format!("{}/{}", artist_name.to_lowercase(), title.to_lowercase());
                candidate == *s
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let filter = Filter::parse(&args);

    let music = std::env::var("MUSIC_DIR").unwrap_or_else(|_| "music".to_string());

    let scope_label = match &filter {
        Filter::None => format!("scanning {} for files with bad metadata...", music),
        Filter::Artist(a) => format!("scanning {} - artist filter: {}", music, a),
        Filter::Song(s) => format!("scanning {} - song filter: {}", music, s),
    };
    println!("{}", scope_label);

    let mut scanned = 0u64;
    let mut fixed = 0u64;
    let mut art_fixed = 0u64;
    let mut failures: HashMap<String, String> = HashMap::new();

    let base = Path::new(&music);
    if !base.is_dir() {
        eprintln!("error: {} is not a directory", music);
        std::process::exit(1);
    }

    let mut files = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("opus"))
                .unwrap_or(false)
            {
                files.push(p);
            }
        }
    }

    files.sort();

    for path in &files {
        let path_str = path.to_string_lossy().to_string();
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let artist_name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        if !filter.matches(&artist_name, &title) {
            continue;
        }

        scanned += 1;

        // log lastfm lookup (album/genres/date) for visibility
        /* let (_lfm_album, _lfm_image, _lfm_genres, _lfm_date) =
            lastfm::lookup(&artist_name, &title).await;
        println!(
            "lastfm lookup for '{} / {}': album={:?}, genres={:?}, date={:?}",
            artist_name, title, lfm_album, lfm_genres, lfm_date
        ); */

        // fix missing tags || date
        let (album, genre, artist) = metadata::read_tags(&path_str).await;
        let mut attempted_fix = false;
        if metadata::needs_fix(&album, &artist, &genre) {
            println!(
                "  fixing tags: {}/{} (album={:?}, artist={:?}, genre={:?})",
                artist_name, title, album, artist, genre
            );
            attempted_fix = true;
            metadata::fix_file(&path_str, &artist_name, &title).await;
        } else {
            // if tags look okay but DATE is missing or not in YYYY/YYYY-MM-DD format, fix it
            let date = metadata::read_date(&path_str).await;
            let date_invalid = date
                .as_deref()
                .map(|d| {
                    let trimmed = d.trim();
                    trimmed.len() < 4 || !trimmed[..4].chars().all(|c| c.is_ascii_digit())
                })
                .unwrap_or(true);
            if date_invalid {
                if date.is_none() {
                    println!("  missing DATE: attempting fix for {}/{}", artist_name, title);
                } else {
                    println!(
                        "  bad DATE format {:?}: attempting fix for {}/{}",
                        date, artist_name, title
                    );
                }
                attempted_fix = true;
                metadata::fix_file(&path_str, &artist_name, &title).await;
            }
        }

        if attempted_fix {
            let (new_album, new_genre, new_artist) = metadata::read_tags(&path_str).await;
            if metadata::needs_fix(&new_album, &new_artist, &new_genre) {
                let key = format!("{}/{}", artist_name, title);
                failures.insert(key, "no lastfm data for tags".to_string());
                println!("    -> still incomplete (no lastfm data available)");
            } else {
                fixed += 1;
                println!(
                    "    -> fixed (album={:?}, artist={:?}, genre={:?})",
                    new_album, new_artist, new_genre
                );
            }
        }

        // fix missing track number (only if tags are otherwise ok)
        if !attempted_fix && metadata::read_track_number(&path_str).is_none() {
            println!("  missing TRACKNUMBER: attempting fix for {}/{}", artist_name, title);
            metadata::fix_file(&path_str, &artist_name, &title).await;
            if let Some(n) = metadata::read_track_number(&path_str) {
                fixed += 1;
                println!("    -> track number set to {}", n);
            } else {
                println!("    -> no track number found on last.fm");
            }
        }

        // fix missing cover art
        if !metadata::has_picture(&path_str) {
            let (_, image_url, _, _lfm_date, _) = lastfm::lookup(&artist_name, &title).await;
            /* println!(
                "  lastfm lookup for art {}/{}: date={:?}",
                artist_name, title, lfm_date
            ); */
            let key = format!("{}/{}", artist_name, title);
            if let Some(url) = image_url {
                print!("  fixing art: {}/{} ... ", artist_name, title);
                match metadata::embed_picture(&path_str, &url).await {
                    Ok(()) => {
                        art_fixed += 1;
                        println!("done");
                    }
                    Err(e) => {
                        failures.insert(key, format!("art embed failed: {}", e));
                        println!("failed: {}", e);
                    }
                }
            } else {
                failures.insert(key, "no cover art on lastfm".to_string());
            }
        }
    }

    let failures_path = format!("{}/failures.json", music);
    if failures.is_empty() {
        let _ = std::fs::remove_file(&failures_path);
    } else if let Ok(json) = serde_json::to_string_pretty(&failures) {
        let _ = std::fs::write(&failures_path, json);
    }

    println!();
    println!(
        "done - scanned: {}, tags fixed: {}, art fixed: {}, failures: {}",
        scanned,
        fixed,
        art_fixed,
        failures.len()
    );

    if fixed > 0 || art_fixed > 0 {
        println!("triggering navidrome scan...");
        download::trigger_scan().await;
        println!("scan triggered");
    }
}
