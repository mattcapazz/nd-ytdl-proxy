// strips trailing parenthetical/bracketed YouTube tags from a video title
pub fn strip_tags(title: &str) -> String {
    let keywords = [
        "official",
        "audio",
        "video",
        "lyric",
        "lyrics",
        "visualizer",
        "hd",
        "hq",
        "4k",
        "remaster",
        "extended",
        "radio edit",
    ];
    let mut s = title.trim().to_string();
    loop {
        let t = s.trim_end();
        let open = match t.chars().last() {
            Some(')') => '(',
            Some(']') => '[',
            _ => break,
        };
        if let Some(pos) = t.rfind(open) {
            let inside = t[pos + 1..t.len() - 1].to_lowercase();
            if keywords.iter().any(|k| inside.contains(k)) {
                s = t[..pos].trim().to_string();
            } else {
                break;
            }
        } else {
            break;
        }
    }
    s
}
