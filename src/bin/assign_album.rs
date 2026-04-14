use std::path::Path;

use nd_ytdl_proxy::{lastfm, metadata};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    let song_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("song:").map(|s| s.trim().replace('\\', "/")));
    let album_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("album:").map(|s| s.trim().to_string()));

    let (song_path, album_name) = match (song_arg, album_arg) {
        (Some(s), Some(a)) => (s, a),
        _ => {
            eprintln!("usage: assign-album \"song:<artist>/<title>\" \"album:<album name>\"");
            std::process::exit(1);
        }
    };

    let music = std::env::var("MUSIC_DIR").unwrap_or_else(|_| "music".to_string());
    let full_path = format!("{}/{}.opus", music, song_path);

    if !Path::new(&full_path).exists() {
        eprintln!("file not found: {}", full_path);
        std::process::exit(1);
    }

    let parts: Vec<&str> = song_path.splitn(2, '/').collect();
    let artist = parts.first().copied().unwrap_or("");
    let title = parts.get(1).copied().unwrap_or("");

    println!("song  : {}/{}", artist, title);
    println!("album : {}", album_name);
    println!();

    // look up album-level data from last.fm
    let image_url = lastfm::album_image(artist, &album_name, Some(title)).await;
    let release_date = lastfm::album_published(artist, &album_name, Some(title)).await;
    let mut genres = lastfm::album_genres(artist, &album_name).await;

    // fall back to existing genres on disk if the album has none
    if genres.is_empty() {
        let (_, existing_genre, _) = metadata::read_tags(&full_path).await;
        if let Some(g) =
            existing_genre.filter(|g| !g.is_empty() && !g.eq_ignore_ascii_case("music"))
        {
            genres = vec![g];
        }
    }

    println!("image   : {}", image_url.as_deref().unwrap_or("(none)"));
    println!("date    : {}", release_date.as_deref().unwrap_or("(none)"));
    println!("genres  : {:?}", genres);
    println!();

    // read the existing artist tag to preserve it
    let (_, _, existing_artist) = metadata::read_tags(&full_path).await;
    let artist_to_write = existing_artist.as_deref().unwrap_or(artist);

    print!("writing tags... ");
    match metadata::write_tags(
        &full_path,
        &album_name,
        &genres,
        artist_to_write,
        "",
        release_date.as_deref().unwrap_or(""),
        None,
    )
    .await
    {
        Ok(()) => println!("done"),
        Err(e) => {
            eprintln!("failed: {}", e);
            std::process::exit(1);
        }
    }

    if let Some(url) = image_url.as_deref() {
        print!("embedding cover art... ");
        match metadata::embed_picture(&full_path, url).await {
            Ok(()) => println!("done"),
            Err(e) => eprintln!("failed: {}", e),
        }
    } else {
        println!("no cover art found on last.fm for this album");
    }

    println!();
    println!("done");
}
