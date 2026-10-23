//! Deterministic text helpers shared by candidate identity and validation.

/// Stable, human-readable slug for a title.
pub(crate) fn slugify(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        // A title with no ASCII at all — every CJK title — must not fall back
        // to a timestamp: that made the id depend on when the write happened.
        // Derive it from the title so the id is a function of the content.
        format!("mem-{}", short_hash(title.trim()))
    } else {
        s.chars().take(48).collect()
    }
}

/// Short stable hex hash (FNV-1a 64). Not cryptographic — it only has to be
/// stable and collision-shy for identities.
pub(crate) fn short_hash(raw: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Seconds-precision RFC 3339 timestamp, the format the records store.
pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// True for ideographic / kana / hangul scripts that write without spaces.
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x9FFF   // CJK unified (incl. ext A)
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0x3040..=0x30FF // hiragana + katakana
        | 0xAC00..=0xD7A3 // hangul syllables
    )
}

/// Split text into lexical terms for FTS. ASCII/alphanumeric runs become whole
/// words; a code identifier is also split at camel boundaries; CJK runs — which
/// have no spaces — become overlapping character bigrams so Chinese queries
/// actually match. Without the bigrams, `is_alphanumeric()` treats a whole
/// Chinese phrase as one token and search never matches.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();

    fn flush_word(word: &mut String, out: &mut Vec<String>) {
        let original = std::mem::take(word);
        if original.chars().count() <= 1 {
            return;
        }
        let whole = original.to_lowercase();
        out.push(whole.clone());
        let parts = camel_parts(&original);
        if parts.len() > 1 {
            for part in parts {
                let part = part.to_lowercase();
                if part.chars().count() > 1 && part != whole {
                    out.push(part);
                }
            }
        }
    }
    fn flush_cjk(cjk: &mut Vec<char>, out: &mut Vec<String>) {
        match cjk.len() {
            0 => {}
            1 => out.push(cjk[0].to_string()),
            _ => {
                for w in cjk.windows(2) {
                    out.push(w.iter().collect());
                }
            }
        }
        cjk.clear();
    }

    for c in text.chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut out);
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            // Keep the original case until flush: the camel boundary is the
            // only place it matters.
            word.push(c);
        } else {
            flush_word(&mut word, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
}

/// Split a code identifier at lower/digit → upper boundaries. `SQLite` and
/// `HTTP2` stay one part; `PermissionProfile` becomes `Permission`, `Profile`.
fn camel_parts(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut previous_was_lower_or_digit = false;
    for character in word.chars() {
        if character.is_uppercase() && previous_was_lower_or_digit && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        previous_was_lower_or_digit = character.is_lowercase() || character.is_ascii_digit();
        current.push(character);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}
