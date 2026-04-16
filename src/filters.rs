use std::collections::HashSet;

use serde_json::Value;
use tracing::info;

use crate::{db, utils};

pub fn artist_allowed(artist: &str, allowed: &HashSet<String>) -> bool {
    if allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist)) {
        return true;
    }
    // check if any individual part of a multi-artist name is allowed
    let parts = utils::split_artists(artist);
    if parts.len() > 1 {
        return parts
            .iter()
            .any(|p| allowed.iter().any(|lib| lib.eq_ignore_ascii_case(p)));
    }
    false
}

pub fn filter_get_album(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    let trashed = db::get_trashed_songs(user);
    let hidden_albums = db::get_fully_trashed_album_ids(user);
    let library_user = !allowed.is_empty();
    // if the entire album is trashed for this user, wipe all songs
    if let Some(album_id) = data
        .get("subsonic-response")
        .and_then(|r| r.get("album"))
        .and_then(|a| a.get("id"))
        .and_then(|id| id.as_str())
    {
        if hidden_albums.contains(album_id) {
            if let Some(songs) = data
                .get_mut("subsonic-response")
                .and_then(|r| r.get_mut("album"))
                .and_then(|a| a.get_mut("song"))
                .and_then(|s| s.as_array_mut())
            {
                songs.clear();
            }
            return;
        }
    }
    let songs = data
        .get_mut("subsonic-response")
        .and_then(|r| r.get_mut("album"))
        .and_then(|a| a.get_mut("song"))
        .and_then(|s| s.as_array_mut());
    if let Some(song_list) = songs {
        song_list.retain(|s| {
            let artist = s["artist"].as_str().unwrap_or("");
            let title = s["title"].as_str().unwrap_or("");
            // library users: hide songs from artists not in their library
            if library_user && !artist_allowed(artist, &allowed) {
                return false;
            }
            !trashed
                .iter()
                .any(|(a, t)| a.eq_ignore_ascii_case(artist) && t.eq_ignore_ascii_case(title))
        });
    }
}

pub fn filter_get_artists(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    let trashed_only = if allowed.is_empty() {
        db::get_trashed_only_artists(user)
    } else {
        HashSet::new()
    };
    let library_user = !allowed.is_empty();
    let indexes = data
        .get_mut("subsonic-response")
        .and_then(|r| r.get_mut("artists"))
        .and_then(|a| a.get_mut("index"))
        .and_then(|i| i.as_array_mut());
    if let Some(indexes) = indexes {
        for index in indexes.iter_mut() {
            if let Some(artists) = index.get_mut("artist").and_then(|a| a.as_array_mut()) {
                artists.retain(|a| {
                    let name = a["name"].as_str().unwrap_or("");
                    if library_user {
                        return artist_allowed(name, &allowed);
                    }
                    !trashed_only.iter().any(|ta| ta.eq_ignore_ascii_case(name))
                });
            }
        }
        indexes.retain(|idx| {
            idx["artist"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        });
    }
}

pub fn filter_get_album_list(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    let trashed_only = if allowed.is_empty() {
        db::get_trashed_only_artists(user)
    } else {
        HashSet::new()
    };
    let hidden_albums = db::get_fully_trashed_album_ids(user);
    let library_user = !allowed.is_empty();
    let should_remove = {
        let albums = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList"))
            .and_then(|al| al.get_mut("album"))
            .and_then(|a| a.as_array_mut());
        if let Some(albums) = albums {
            albums.retain(|a| {
                let artist = a["artist"].as_str().unwrap_or("");
                let album_id = a["id"].as_str().unwrap_or("");
                let album_name = a["name"].as_str().unwrap_or("");
                if !album_id.is_empty() && hidden_albums.contains(album_id) {
                    info!(
                        "filter_get_album_list: hiding '{}' ({}) - all songs trashed for user '{}'",
                        album_name, album_id, user
                    );
                    return false;
                }
                if library_user {
                    return artist_allowed(artist, &allowed);
                }
                !trashed_only
                    .iter()
                    .any(|ta| ta.eq_ignore_ascii_case(artist))
            });
            albums.is_empty()
        } else {
            false
        }
    };
    if should_remove {
        if let Some(list) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList"))
            .and_then(|al| al.as_object_mut())
        {
            list.remove("album");
        }
    }
}

pub fn filter_get_album_list2(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    let trashed_only = if allowed.is_empty() {
        db::get_trashed_only_artists(user)
    } else {
        HashSet::new()
    };
    let hidden_albums = db::get_fully_trashed_album_ids(user);
    let library_user = !allowed.is_empty();
    let should_remove = {
        let albums = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList2"))
            .and_then(|al| al.get_mut("album"))
            .and_then(|a| a.as_array_mut());
        if let Some(albums) = albums {
            let before = albums.len();
            albums.retain(|a| {
                let artist = a["artist"].as_str().unwrap_or("");
                let album_id = a["id"].as_str().unwrap_or("");
                if !album_id.is_empty() && hidden_albums.contains(album_id) {
                    return false;
                }
                if library_user {
                    return artist_allowed(artist, &allowed);
                }
                !trashed_only
                    .iter()
                    .any(|ta| ta.eq_ignore_ascii_case(artist))
            });
            let removed = before - albums.len();
            if removed > 0 {
                info!(
                    "filter_get_album_list2: removed {} album(s) for user {}",
                    removed, user
                );
            }
            albums.is_empty()
        } else {
            false
        }
    };
    if should_remove {
        if let Some(list) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList2"))
            .and_then(|al| al.as_object_mut())
        {
            list.remove("album");
        }
    }
}
