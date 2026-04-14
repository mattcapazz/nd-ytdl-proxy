use std::path::Path;

use nd_ytdl_proxy::{lastfm, metadata};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let song_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("song:").map(|s| s.trim().replace('\\', "/")));

    let song_path = match song_arg {
        Some(s) => s,
        None => {
            eprintln!("usage: inspect-song \"song:<artist>/<title>\"");
            std::process::exit(1);
        }
    };

    let music = std::env::var("MUSIC_DIR").unwrap_or_else(|_| "music".to_string());
    let full_path = format!("{}/{}.opus", music, song_path);

    if !Path::new(&full_path).exists() {
        eprintln!("file not found: {}", full_path);
        std::process::exit(1);
    }

    // derive artist/title from path components
    let parts: Vec<&str> = song_path.splitn(2, '/').collect();
    let artist_name = parts.first().copied().unwrap_or("");
    let title = parts.get(1).copied().unwrap_or("");

    println!("file: {}", full_path);
    println!();

    // on-disk tags
    let (album, genre, artist) = metadata::read_tags(&full_path).await;
    let date = metadata::read_date(&full_path).await;
    let has_art = metadata::has_picture(&full_path);

    println!("-- on-disk tags --");
    println!("  artist : {}", artist.as_deref().unwrap_or("(none)"));
    println!("  album  : {}", album.as_deref().unwrap_or("(none)"));
    println!("  genre  : {}", genre.as_deref().unwrap_or("(none)"));
    println!("  date   : {}", date.as_deref().unwrap_or("(none)"));
    println!("  art    : {}", if has_art { "yes" } else { "no" });
    println!();

    // last.fm track info
    let (lfm_album, lfm_image, lfm_genres, lfm_track_number) =
        lastfm::lookup(artist_name, title).await;

    println!("-- last.fm track --");
    println!("  album  : {}", lfm_album.as_deref().unwrap_or("(none)"));
    println!("  image  : {}", lfm_image.as_deref().unwrap_or("(none)"));
    println!(
        "  genres : {}",
        if lfm_genres.is_empty() {
            "(none)".to_string()
        } else {
            lfm_genres.join(", ")
        }
    );

    println!(
        "  track# : {}",
        lfm_track_number
            .map(|n| n.to_string())
            .as_deref()
            .unwrap_or("(none)")
    );
    println!();

    // last.fm album info (if we have an album name)
    let album_name = lfm_album.as_deref().or(album.as_deref()).unwrap_or("");
    if !album_name.is_empty() {
        let release_date = lastfm::album_published(artist_name, album_name, Some(title)).await;
        let image = lastfm::album_image(artist_name, album_name, Some(title)).await;

        println!("-- last.fm album: {} --", album_name);
        println!(
            "  released : {}",
            release_date.as_deref().unwrap_or("(none)")
        );
        println!("  image    : {}", image.as_deref().unwrap_or("(none)"));
    } else {
        println!("-- last.fm album: (no album found) --");
    }
}
