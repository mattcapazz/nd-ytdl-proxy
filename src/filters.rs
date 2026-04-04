use std::collections::HashSet;

use serde_json::Value;

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
    let should_remove = {
        let songs = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("album"))
            .and_then(|a| a.get_mut("song"))
            .and_then(|s| s.as_array_mut());
        if let Some(song_list) = songs {
            song_list.retain(|s| {
                let artist = s["artist"].as_str().unwrap_or("");
                let title = s["title"].as_str().unwrap_or("");
                if !artist_allowed(artist, &allowed) {
                    return false;
                }
                !trashed
                    .iter()
                    .any(|(a, t)| a.eq_ignore_ascii_case(artist) && t.eq_ignore_ascii_case(title))
            });
            song_list.is_empty()
        } else {
            false
        }
    };
    if should_remove {
        if let Some(album) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("album"))
            .and_then(|a| a.as_object_mut())
        {
            album.remove("song");
        }
    }
}

pub fn filter_get_artists(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    let indexes = data
        .get_mut("subsonic-response")
        .and_then(|r| r.get_mut("artists"))
        .and_then(|a| a.get_mut("index"))
        .and_then(|i| i.as_array_mut());
    if let Some(indexes) = indexes {
        for index in indexes.iter_mut() {
            if let Some(artists) = index.get_mut("artist").and_then(|a| a.as_array_mut()) {
                artists.retain(|a| {
                    a["name"]
                        .as_str()
                        .map(|n| artist_allowed(n, &allowed))
                        .unwrap_or(false)
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
    let should_remove = {
        let albums = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList"))
            .and_then(|al| al.get_mut("album"))
            .and_then(|a| a.as_array_mut());
        if let Some(albums) = albums {
            albums.retain(|a| {
                let artist = a["artist"].as_str().unwrap_or("");
                artist_allowed(artist, &allowed)
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
    let should_remove = {
        let albums = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList2"))
            .and_then(|al| al.get_mut("album"))
            .and_then(|a| a.as_array_mut());
        if let Some(albums) = albums {
            albums.retain(|a| {
                let artist = a["artist"].as_str().unwrap_or("");
                artist_allowed(artist, &allowed)
            });
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
