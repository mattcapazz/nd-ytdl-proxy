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
