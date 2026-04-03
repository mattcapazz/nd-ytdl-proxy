use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use rusqlite::Connection;
use tracing::info;

static DB: LazyLock<Mutex<Connection>> = LazyLock::new(|| {
    let path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/library.db".to_string());
    let conn = Connection::open(&path).expect("failed to open database");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS user_songs (
            user TEXT NOT NULL,
            artist TEXT NOT NULL COLLATE NOCASE,
            title TEXT NOT NULL COLLATE NOCASE,
            trashed INTEGER NOT NULL DEFAULT 0,
            added_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (user, artist, title)
        )",
    )
    .expect("failed to create table");
    info!("database ready at {}", path);
    Mutex::new(conn)
});

// add a song to a user's library (skips if already exists)
pub fn add_song(user: &str, artist: &str, title: &str) {
    if user.is_empty() || artist.is_empty() || title.is_empty() {
        return;
    }
    let db = DB.lock().unwrap();
    db.execute(
        "INSERT OR IGNORE INTO user_songs (user, artist, title) VALUES (?1, ?2, ?3)",
        rusqlite::params![user, artist, title],
    )
    .ok();
}

pub fn add_songs(user: &str, songs: &[(String, String)]) {
    if user.is_empty() {
        return;
    }
    let db = DB.lock().unwrap();
    for (artist, title) in songs {
        if artist.is_empty() || title.is_empty() {
            continue;
        }
        db.execute(
            "INSERT OR IGNORE INTO user_songs (user, artist, title) VALUES (?1, ?2, ?3)",
            rusqlite::params![user, artist, title],
        )
        .ok();
    }
}

// mark a song as trashed so it stops appearing for this user
pub fn trash_song(user: &str, artist: &str, title: &str) {
    let db = DB.lock().unwrap();
    db.execute(
        "UPDATE user_songs SET trashed = 1 WHERE user = ?1 AND artist = ?2 AND title = ?3",
        rusqlite::params![user, artist, title],
    )
    .ok();
}

// get set of artist names that have at least one non-trashed song
pub fn get_artists(user: &str) -> HashSet<String> {
    let db = DB.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT DISTINCT artist FROM user_songs WHERE user = ?1 AND trashed = 0")
        .unwrap();
    stmt.query_map(rusqlite::params![user], |row| row.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
}

// get set of (artist, title) pairs that are not trashed
pub fn get_songs(user: &str) -> HashSet<(String, String)> {
    let db = DB.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT artist, title FROM user_songs WHERE user = ?1 AND trashed = 0")
        .unwrap();
    stmt.query_map(rusqlite::params![user], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

// check if a specific song is trashed for this user
pub fn is_trashed(user: &str, artist: &str, title: &str) -> bool {
    let db = DB.lock().unwrap();
    db.query_row(
        "SELECT trashed FROM user_songs WHERE user = ?1 AND artist = ?2 AND title = ?3",
        rusqlite::params![user, artist, title],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v == 1)
    .unwrap_or(false)
}

// check if any other user (besides the given one) owns this song non-trashed
pub fn song_owned_by_others(user: &str, artist: &str, title: &str) -> bool {
    let db = DB.lock().unwrap();
    db.query_row(
        "SELECT COUNT(*) FROM user_songs WHERE user != ?1 AND artist = ?2 AND title = ?3 AND trashed = 0",
        rusqlite::params![user, artist, title],
        |row| row.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

// get set of (artist, title) pairs that are trashed
pub fn get_trashed_songs(user: &str) -> HashSet<(String, String)> {
    let db = DB.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT artist, title FROM user_songs WHERE user = ?1 AND trashed = 1")
        .unwrap();
    stmt.query_map(rusqlite::params![user], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

pub fn has_any(user: &str) -> bool {
    let db = DB.lock().unwrap();
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM user_songs WHERE user = ?1 AND trashed = 0",
            rusqlite::params![user],
            |row| row.get(0),
        )
        .unwrap_or(0);
    count > 0
}

// remove songs from navidrome's media_file table so they don't linger as missing
pub fn navidrome_delete_songs(ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let nd_path = std::env::var("ND_DB_PATH").unwrap_or_else(|_| "data/navidrome.db".to_string());
    let conn = match Connection::open(&nd_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to open navidrome db: {}", e);
            return;
        }
    };
    // match navidrome's WAL mode so we don't corrupt its journal
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .ok();
    for id in ids {
        conn.execute(
            "DELETE FROM media_file WHERE id = ?1",
            rusqlite::params![id],
        )
        .ok();
        conn.execute(
            "DELETE FROM media_file_artists WHERE media_file_id = ?1",
            rusqlite::params![id],
        )
        .ok();
    }
    info!("removed {} entries from navidrome media_file", ids.len());
}
