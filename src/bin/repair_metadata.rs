use std::collections::HashMap;
use std::path::Path;

use nd_ytdl_proxy::{download, lastfm, metadata};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let music = std::env::var("MUSIC_DIR").unwrap_or_else(|_| "/music".to_string());
    println!("scanning {} for files with bad metadata...", music);

    let mut scanned = 0u64;
    let mut fixed = 0u64;
    let mut art_fixed = 0u64;
    let mut failures: HashMap<String, String> = HashMap::new();

    let base = Path::new(&music);
    if !base.is_dir() {
        eprintln!("error: {} is not a directory", music);
        std::process::exit(1);
    }

    let mut artist_dirs: Vec<_> = std::fs::read_dir(base)
        .expect("cannot read music dir")
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    artist_dirs.sort_by_key(|e| e.file_name());

    for artist_entry in &artist_dirs {
        let artist_name = artist_entry.file_name().to_string_lossy().to_string();
        let artist_path = artist_entry.path();

        let files: Vec<_> = std::fs::read_dir(&artist_path)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("opus"))
                    .unwrap_or(false)
            })
            .collect();

        if files.is_empty() {
            continue;
        }

        for entry in &files {
            let path = entry.path();
            let path_str = path.to_string_lossy().to_string();
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            scanned += 1;

            // fix missing tags
            let (album, genre, artist) = metadata::read_tags(&path_str).await;
            if metadata::needs_fix(&album, &artist, &genre) {
                println!(
                    "  fixing tags: {}/{} (album={:?}, artist={:?}, genre={:?})",
                    artist_name, title, album, artist, genre
                );

                metadata::fix_file(&path_str, &artist_name, &title).await;

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

            // fix missing cover art
            if !metadata::has_picture(&path_str) {
                let (_, image_url, _) = lastfm::lookup(&artist_name, &title).await;
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
