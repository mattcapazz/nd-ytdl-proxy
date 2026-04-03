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
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '+' => out.push(' '),
            '%' => {
                let hi = chars.next().unwrap_or('0');
                let lo = chars.next().unwrap_or('0');
                let hex = format!("{}{}", hi, lo);
                if let Ok(b) = u8::from_str_radix(&hex, 16) {
                    out.push(b as char);
                } else {
                    out.push('%');
                    out.push(hi);
                    out.push(lo);
                }
            }
            _ => out.push(c),
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
    let candidate = format!("{}/{}", base, sanitized);
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
