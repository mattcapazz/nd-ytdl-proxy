use std::collections::HashMap;
use std::sync::LazyLock;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

static UPSTREAM: LazyLock<String> = LazyLock::new(|| {
    std::env::var("UPSTREAM_URL").unwrap_or_else(|_| "http://0.0.0.0:4533".to_string())
});

static MUSIC: LazyLock<String> =
    LazyLock::new(|| std::env::var("MUSIC_DIR").unwrap_or_else(|_| "/music".to_string()));

static ADMIN_USER: LazyLock<String> =
    LazyLock::new(|| std::env::var("ND_ADMIN_USER").unwrap_or_else(|_| "admin".to_string()));

static ADMIN_PASS: LazyLock<String> =
    LazyLock::new(|| std::env::var("ND_ADMIN_PASS").unwrap_or_else(|_| "admin".to_string()));

pub fn http_client() -> &'static reqwest::Client {
    &HTTP_CLIENT
}

pub fn upstream_url() -> &'static str {
    &UPSTREAM
}

pub fn music_dir() -> &'static str {
    &MUSIC
}

pub fn admin_auth_query() -> String {
    format!(
        "u={}&p={}&v=1.16.1&c=nd-ytdl-proxy&f=json",
        &*ADMIN_USER, &*ADMIN_PASS
    )
}

pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            if key.is_empty() {
                return None;
            }
            let val = url_decode(parts.next().unwrap_or(""));
            Some((key.to_string(), val))
        })
        .collect()
}

pub fn url_decode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.bytes().peekable();
    while let Some(b) = chars.next() {
        match b {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hi = chars.next().unwrap_or(b'0');
                let lo = chars.next().unwrap_or(b'0');
                let hex = [hi, lo];
                if let Ok(decoded) =
                    u8::from_str_radix(std::str::from_utf8(&hex).unwrap_or("00"), 16)
                {
                    bytes.push(decoded);
                } else {
                    bytes.push(b'%');
                    bytes.push(hi);
                    bytes.push(lo);
                }
            }
            _ => bytes.push(b),
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

pub fn url_encode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
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

pub fn find_artist_dir(base: &str, artist: &str) -> String {
    use deunicode::deunicode;

    fn canonical(s: &str) -> String {
        let mut out = deunicode(s).to_lowercase();
        out.retain(|c| c.is_alphanumeric());
        out
    }

    let sanitized = sanitize_filename(artist);
    // strip trailing dots (yt-dlp replaces trailing dots in path components with '#')
    let sanitized = sanitized.trim_end_matches('.').to_string();
    let candidate = format!("{}/{}", base, sanitized);
    // yt-dlp appends '#' when stripping a trailing dot, so prefer that dir if it exists
    let hash_candidate = format!("{}#", candidate);
    if std::path::Path::new(&hash_candidate).exists() {
        return hash_candidate;
    }
    if std::path::Path::new(&candidate).exists() {
        return candidate;
    }

    let target = canonical(artist);
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            if let Some(name_os) = entry.file_name().to_str() {
                let name = name_os.to_string();
                let existing = canonical(&name);
                if existing == target {
                    // prefer the # variant (yt-dlp trailing-dot sanitization)
                    let hash_path = format!("{}/{}#", base, name);
                    if std::path::Path::new(&hash_path).exists() {
                        return hash_path;
                    }
                    return format!("{}/{}", base, name);
                }
                // tolerate ligature transliterations like "ae" vs "a"
                if existing.replace("ae", "a") == target || existing == target.replace("ae", "a") {
                    return format!("{}/{}", base, name);
                }
            }
        }
    }

    candidate
}

pub fn split_artists(s: &str) -> Vec<String> {
    let mut tmp = s.to_string();

    // normalize common separators into a single delimiter
    tmp = tmp.replace(",", " /");
    tmp = tmp.replace(" feat. ", " /");
    tmp = tmp.replace(" feat ", " /");
    tmp = tmp.replace(" ft. ", " /");
    tmp = tmp.replace(" ft ", " /");
    tmp = tmp.replace(" & ", " /");
    tmp = tmp.replace(" x ", " /");

    tmp.split('/')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

// strip "Artist - " or "Artist: " from the start of a title when it duplicates the artist
pub fn strip_artist_prefix(artist: &str, title: &str) -> String {
    let norm_artist = deunicode::deunicode(artist).to_lowercase();
    let norm_title = deunicode::deunicode(title).to_lowercase();

    for sep in &[" - ", ": "] {
        let prefix = format!("{}{}", norm_artist, sep);
        if norm_title.starts_with(&prefix) {
            return title[prefix.len()..].trim().to_string();
        }
    }
    title.to_string()
}

// returns artist name with " / " separators so Navidrome can parse individual artists
pub fn artist_display_name(s: &str) -> String {
    let parts = split_artists(s);
    if parts.len() > 1 {
        parts.join(" / ")
    } else {
        s.to_string()
    }
}
