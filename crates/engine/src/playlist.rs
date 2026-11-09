//! Playlists from elsewhere, and back out again.
//!
//! - [`parse`] reads what other players and services export: M3U/M3U8, PLS, XSPF, WPL, iTunes /
//!   Music XML, the CSV/TSV lists of Exportify (Spotify), Apple Music's "Export Playlist" text,
//!   TuneMyMusic / Soundiiz, and plain "Artist - Title" lines pasted from anywhere.
//! - [`resolve`] finds each entry in the library: by its path, by the tail of a path from
//!   another computer, or by its tags (title, artist, album, length).
//! - [`export`] writes M3U8, PLS, XSPF or CSV.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::library::TrackInfo;

/// One line of a playlist, as the file has it. Everything is optional: a path-only M3U knows
/// nothing but the location, a Spotify CSV knows everything but.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    /// File path, `file://` URL, web link, or a streaming id like `qobuz:track:123`.
    pub location: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds.
    pub duration: Option<f64>,
    pub isrc: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Parsed {
    pub name: Option<String>,
    pub entries: Vec<Entry>,
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// Bytes to text: UTF-8 or UTF-16 (with or without BOM), else the 8-bit code page an old
/// Windows player would have written — Cyrillic 1251 or Western 1252, guessed from the bytes.
pub fn decode_text(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let units = b.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) });
        char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')).collect::<String>()
    };
    if let Some(b) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(b).into_owned();
    }
    if let Some(b) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(b, true);
    }
    if let Some(b) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(b, false);
    }
    // UTF-16 without a BOM: ASCII text then has every other byte zero.
    if bytes.len() >= 4 {
        let n = bytes.len().min(400) / 2 * 2;
        let zeros_odd = bytes[..n].iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
        let zeros_even = bytes[..n].iter().step_by(2).filter(|&&b| b == 0).count();
        if zeros_odd * 3 > n / 2 * 2 && zeros_even == 0 {
            return utf16(bytes, true);
        }
        if zeros_even * 3 > n / 2 * 2 && zeros_odd == 0 {
            return utf16(bytes, false);
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // Cyrillic words are runs of high bytes; Western accents sit alone between ASCII letters.
    let high = bytes.iter().filter(|&&b| b >= 0xC0).count();
    let paired = bytes.windows(2).filter(|w| w[0] >= 0xC0 && w[1] >= 0xC0).count();
    let enc = if high > 0 && paired * 2 >= high { encoding_rs::WINDOWS_1251 } else { encoding_rs::WINDOWS_1252 };
    enc.decode_without_bom_handling(bytes).0.into_owned()
}

/// Read a playlist file of any supported kind. `name` is the file name (its extension picks
/// the format; the content decides when it doesn't). An iTunes library export may hold several
/// playlists; everything else holds one.
pub fn parse(bytes: &[u8], name: &str) -> Result<Vec<Parsed>> {
    let text = decode_text(bytes);
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().into_owned());
    let mut out = match ext.as_str() {
        "m3u" | "m3u8" => vec![parse_m3u(&text)],
        "pls" => vec![parse_pls(&text)],
        "xspf" => vec![parse_xspf(&text)?],
        "wpl" | "zpl" => vec![parse_wpl(&text)?],
        "csv" => vec![parse_table(&text, ',')?],
        "tsv" => vec![parse_table(&text, '\t')?],
        _ => parse_text(&text)?,
    };
    for p in &mut out {
        if p.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
            p.name = stem.clone();
        }
    }
    Ok(out)
}

/// Text of unknown kind (a file without a telling extension, or text pasted into the app).
pub fn parse_text(text: &str) -> Result<Vec<Parsed>> {
    let t = text.trim_start_matches('\u{FEFF}').trim_start();
    let first = t.lines().next().unwrap_or("");
    let head = &t[..t.len().min(600)].to_ascii_lowercase();
    Ok(if t.starts_with("#EXTM3U") {
        vec![parse_m3u(t)]
    } else if first.trim().eq_ignore_ascii_case("[playlist]") {
        vec![parse_pls(t)]
    } else if head.contains("<plist") {
        parse_itunes(t)?
    } else if head.contains("<smil") {
        vec![parse_wpl(t)?]
    } else if head.contains("<playlist") {
        vec![parse_xspf(t)?]
    } else if first.contains('\t') && header_columns(&split_row(first, '\t')).title.is_some() {
        vec![parse_table(t, '\t')?]
    } else if first.contains(',') && header_columns(&split_row(first, ',')).title.is_some() {
        vec![parse_table(t, ',')?]
    } else if first.contains(';') && header_columns(&split_row(first, ';')).title.is_some() {
        vec![parse_table(t, ';')?]
    } else {
        vec![parse_lines(t)]
    })
}

fn clean(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// "Artist - Title" → (artist, title). Without a separator it is all title.
fn split_artist_title(s: &str) -> (Option<String>, Option<String>) {
    for sep in [" - ", " – ", " — "] {
        if let Some((a, t)) = s.split_once(sep) {
            return (clean(a), clean(t));
        }
    }
    (None, clean(s))
}

fn parse_m3u(text: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut pending = Entry::default();
    for line in text.lines() {
        let l = line.trim().trim_start_matches('\u{FEFF}');
        if l.is_empty() {
            continue;
        }
        if let Some(rest) = l.strip_prefix("#EXTINF:") {
            // "#EXTINF:245,Artist - Title"; IPTV-style lists put attributes before the comma.
            let (dur, info) = rest.split_once(',').unwrap_or((rest, ""));
            pending.duration = dur.split_whitespace().next().and_then(|d| d.parse::<f64>().ok()).filter(|d| *d > 0.0);
            let (a, t) = split_artist_title(info);
            pending.artist = a;
            pending.title = t;
        } else if let Some(n) = l.strip_prefix("#PLAYLIST:") {
            out.name = clean(n);
        } else if let Some(a) = l.strip_prefix("#EXTALB:") {
            pending.album = clean(a);
        } else if let Some(a) = l.strip_prefix("#EXTART:") {
            if pending.artist.is_none() {
                pending.artist = clean(a);
            }
        } else if !l.starts_with('#') {
            pending.location = Some(l.to_string());
            out.entries.push(std::mem::take(&mut pending));
        }
    }
    out
}

fn parse_pls(text: &str) -> Parsed {
    let mut by_n: std::collections::BTreeMap<u32, Entry> = Default::default();
    let mut out = Parsed::default();
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else { continue };
        let k = k.trim().to_ascii_lowercase();
        if k == "x-playlist-name" || k == "playlistname" {
            out.name = clean(v);
            continue;
        }
        let digits = k.trim_start_matches(|c: char| c.is_ascii_alphabetic());
        let Ok(n) = digits.parse::<u32>() else { continue };
        let e = by_n.entry(n).or_default();
        match &k[..k.len() - digits.len()] {
            "file" => e.location = clean(v),
            "title" => {
                let (a, t) = split_artist_title(v);
                e.artist = a;
                e.title = t;
            }
            "length" => e.duration = v.trim().parse::<f64>().ok().filter(|d| *d > 0.0),
            _ => {}
        }
    }
    out.entries = by_n.into_values().filter(|e| e.location.is_some()).collect();
    out
}

fn xml(text: &str) -> Result<roxmltree::Document<'_>> {
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    Ok(roxmltree::Document::parse_with_options(text, opts)?)
}

fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name().eq_ignore_ascii_case(name))
}

fn child_text(n: roxmltree::Node, name: &str) -> Option<String> {
    child(n, name).and_then(|c| c.text()).and_then(clean)
}

fn parse_xspf(text: &str) -> Result<Parsed> {
    let doc = xml(text)?;
    let root = doc.root_element();
    let mut out = Parsed { name: child_text(root, "title"), entries: Vec::new() };
    let Some(list) = child(root, "trackList") else { return Ok(out) };
    for t in list.children().filter(|c| c.is_element() && c.tag_name().name() == "track") {
        out.entries.push(Entry {
            location: child_text(t, "location"),
            title: child_text(t, "title"),
            artist: child_text(t, "creator"),
            album: child_text(t, "album"),
            duration: child_text(t, "duration").and_then(|d| d.parse::<f64>().ok()).map(|ms| ms / 1000.0).filter(|d| *d > 0.0),
            isrc: t
                .children()
                .filter(|c| c.tag_name().name() == "identifier")
                .filter_map(|c| c.text())
                .find_map(|s| s.trim().strip_prefix("isrc:").map(|x| x.to_string())),
        });
    }
    Ok(out)
}

/// Windows Media Player / Zune: `<smil><head><title/></head><body><seq><media src=""/>`.
fn parse_wpl(text: &str) -> Result<Parsed> {
    let doc = xml(text)?;
    let root = doc.root_element();
    let name = child(root, "head").and_then(|h| child_text(h, "title"));
    let entries = root
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "media")
        .filter_map(|n| n.attribute("src").map(|s| Entry { location: Some(s.to_string()), ..Default::default() }))
        .collect();
    Ok(Parsed { name, entries })
}

/// A plist value, as much of it as iTunes / Music exports use.
enum Plist<'a> {
    Dict(Vec<(String, Plist<'a>)>),
    Array(Vec<Plist<'a>>),
    Str(String),
    Int(i64),
    Bool(bool),
    Other(#[allow(dead_code)] roxmltree::Node<'a, 'a>),
}

impl<'a> Plist<'a> {
    fn from(n: roxmltree::Node<'a, 'a>) -> Self {
        let elements = || n.children().filter(|c| c.is_element());
        match n.tag_name().name() {
            "dict" => {
                let mut v = Vec::new();
                let mut key = None;
                for c in elements() {
                    if c.tag_name().name() == "key" {
                        key = Some(c.text().unwrap_or("").to_string());
                    } else if let Some(k) = key.take() {
                        v.push((k, Plist::from(c)));
                    }
                }
                Plist::Dict(v)
            }
            "array" => Plist::Array(elements().map(Plist::from).collect()),
            "string" => Plist::Str(n.text().unwrap_or("").to_string()),
            "integer" => Plist::Int(n.text().and_then(|t| t.trim().parse().ok()).unwrap_or(0)),
            "true" => Plist::Bool(true),
            "false" => Plist::Bool(false),
            _ => Plist::Other(n),
        }
    }
    fn get(&self, key: &str) -> Option<&Plist<'a>> {
        match self {
            Plist::Dict(v) => v.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn str(&self, key: &str) -> Option<String> {
        match self.get(key)? {
            Plist::Str(s) => clean(s),
            _ => None,
        }
    }
    fn int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Plist::Int(i) => Some(*i),
            _ => None,
        }
    }
    fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some(Plist::Bool(true)))
    }
}

/// iTunes / Music "Export Playlist… (XML)" or the whole library XML: the tracks dictionary and
/// the playlists that point into it. The library's built-in lists (Music, Movies, Downloaded…)
/// are left out.
fn parse_itunes(text: &str) -> Result<Vec<Parsed>> {
    let doc = xml(text)?;
    let Some(top) = child(doc.root_element(), "dict") else { bail!("the XML holds no playlist dictionary") };
    let top = Plist::from(top);
    let mut tracks: HashMap<i64, Entry> = HashMap::new();
    if let Some(Plist::Dict(list)) = top.get("Tracks") {
        for (id, t) in list {
            let Ok(id) = id.parse::<i64>() else { continue };
            tracks.insert(
                id,
                Entry {
                    location: t.str("Location"),
                    title: t.str("Name"),
                    artist: t.str("Artist").or_else(|| t.str("Album Artist")),
                    album: t.str("Album"),
                    duration: t.int("Total Time").map(|ms| ms as f64 / 1000.0),
                    isrc: None,
                },
            );
        }
    }
    let mut out = Vec::new();
    if let Some(Plist::Array(lists)) = top.get("Playlists") {
        for p in lists {
            if p.flag("Master") || p.get("Distinguished Kind").is_some() || p.flag("Folder") || matches!(p.get("Visible"), Some(Plist::Bool(false))) {
                continue;
            }
            let Some(Plist::Array(items)) = p.get("Playlist Items") else { continue };
            let entries: Vec<Entry> = items.iter().filter_map(|i| tracks.get(&i.int("Track ID")?).cloned()).collect();
            if !entries.is_empty() {
                out.push(Parsed { name: p.str("Name"), entries });
            }
        }
    }
    if out.is_empty() {
        bail!("the file holds no playlists");
    }
    Ok(out)
}

/// One CSV/TSV row, quotes and doubled quotes handled (no newlines inside fields here — see
/// [`rows`] for those).
fn split_row(line: &str, sep: char) -> Vec<String> {
    rows(line, sep).into_iter().next().unwrap_or_default()
}

/// RFC 4180 rows: quoted fields may hold the separator, `""` and line breaks.
fn rows(text: &str, sep: char) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.trim().is_empty() {
            field.clear();
            quoted = true;
        } else if c == sep {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut field));
            if row.iter().any(|f| !f.trim().is_empty()) {
                out.push(std::mem::take(&mut row));
            } else {
                row.clear();
            }
        } else {
            field.push(c);
        }
    }
    row.push(field);
    if row.iter().any(|f| !f.trim().is_empty()) {
        out.push(row);
    }
    out
}

#[derive(Default, Debug)]
struct Columns {
    title: Option<usize>,
    artist: Option<usize>,
    album: Option<usize>,
    /// Column index, and whether it counts milliseconds.
    duration: Option<(usize, bool)>,
    location: Option<usize>,
    isrc: Option<usize>,
    /// A streaming link per row (Spotify URIs are of no use here; Qobuz/Tidal ones are).
    link: Option<usize>,
}

/// Recognise the header row of the exports people actually have: Exportify ("Track Name",
/// "Artist Name(s)", "Duration (ms)"), Apple Music's text export in English or Russian ("Name",
/// "Artist", "Time" in seconds, "Location"), TuneMyMusic / Soundiiz ("Track name", "Artist
/// name"), and our own CSV.
fn header_columns(header: &[String]) -> Columns {
    let mut c = Columns::default();
    for (i, h) in header.iter().enumerate() {
        let h = h.trim().trim_start_matches('\u{FEFF}').to_lowercase();
        let h = h.as_str();
        let set = |slot: &mut Option<usize>| {
            slot.get_or_insert(i);
        };
        match h {
            "track name" | "title" | "name" | "track" | "song" | "song name" | "track title" | "название" | "трек" | "песня" | "имя" => set(&mut c.title),
            "artist name(s)" | "artist name" | "artist" | "artists" | "artist(s)" | "creator" | "исполнитель" | "артист" | "исполнители" => set(&mut c.artist),
            "album name" | "album" | "album title" | "альбом" => set(&mut c.album),
            "duration (ms)" | "duration_ms" | "track duration (ms)" => {
                c.duration.get_or_insert((i, true));
            }
            "time" | "duration" | "length" | "время" | "длительность" | "продолжительность" => {
                c.duration.get_or_insert((i, false));
            }
            "location" | "path" | "file" | "filename" | "file path" | "расположение" | "путь" | "файл" => set(&mut c.location),
            "isrc" => set(&mut c.isrc),
            "url" | "link" | "track url" | "track uri" | "ссылка" => set(&mut c.link),
            _ => {}
        }
    }
    c
}

/// "245", "245000" (ms), "4:05", "1:02:03".
fn parse_duration(s: &str, ms: bool) -> Option<f64> {
    let s = s.trim();
    if s.contains(':') {
        let mut total = 0.0;
        for part in s.split(':') {
            total = total * 60.0 + part.trim().replace(',', ".").parse::<f64>().ok()?;
        }
        return Some(total).filter(|d| *d > 0.0);
    }
    let v = s.replace(',', ".").parse::<f64>().ok()?;
    Some(if ms || v > 36_000.0 { v / 1000.0 } else { v }).filter(|d| *d > 0.0)
}

fn parse_table(text: &str, sep: char) -> Result<Parsed> {
    let all = rows(text, sep);
    let Some((header, body)) = all.split_first() else { return Ok(Parsed::default()) };
    let cols = header_columns(header);
    if cols.title.is_none() && cols.location.is_none() {
        bail!("the table has no track title column");
    }
    let cell = |r: &Vec<String>, i: Option<usize>| i.and_then(|i| r.get(i)).and_then(|s| clean(s));
    let entries = body
        .iter()
        .map(|r| Entry {
            location: cell(r, cols.location)
                .map(|l| hfs_to_posix(&l))
                .or_else(|| cell(r, cols.link).filter(|l| streaming_uri(l).is_some())),
            title: cell(r, cols.title),
            artist: cell(r, cols.artist),
            album: cell(r, cols.album),
            duration: cols.duration.and_then(|(i, ms)| r.get(i).and_then(|s| parse_duration(s, ms))),
            isrc: cell(r, cols.isrc),
        })
        .filter(|e| e.title.is_some() || e.location.is_some())
        .collect();
    Ok(Parsed { name: None, entries })
}

/// Old Apple exports write "Macintosh HD:Users:me:Music:a.flac".
fn hfs_to_posix(s: &str) -> String {
    if s.contains('/') || s.contains('\\') || !s.contains(':') || s.contains("://") || s.as_bytes().get(1) == Some(&b':') {
        return s.to_string();
    }
    let mut parts = s.split(':');
    parts.next(); // volume name
    format!("/{}", parts.collect::<Vec<_>>().join("/"))
}

fn looks_like_location(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    s.starts_with('/')
        || s.starts_with('~')
        || s.starts_with("\\\\")
        || lower.starts_with("file:")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || streaming_uri(s).is_some()
        || (s.len() > 2 && s.as_bytes()[1] == b':' && matches!(s.as_bytes()[2], b'\\' | b'/'))
        || crate::library::is_audio(Path::new(s))
}

/// Anything pasted line by line: paths and links, or "Artist - Title", maybe numbered.
fn parse_lines(text: &str) -> Parsed {
    let mut out = Parsed::default();
    for line in text.lines() {
        let l = line.trim().trim_start_matches('\u{FEFF}');
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if looks_like_location(l) {
            out.entries.push(Entry { location: Some(l.to_string()), ..Default::default() });
            continue;
        }
        // "1. Artist - Title", "01) Artist - Title"
        let unnumbered = l
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .strip_prefix(['.', ')'])
            .filter(|_| l.starts_with(|c: char| c.is_ascii_digit()))
            .map(str::trim_start)
            .unwrap_or(l);
        let (artist, title) = split_artist_title(unnumbered);
        out.entries.push(Entry { title, artist, ..Default::default() });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Streaming links
// ---------------------------------------------------------------------------------------------

/// `qobuz:track:123` / `tidal:track:123` from a share link of either service (or the id itself).
pub fn streaming_uri(s: &str) -> Option<String> {
    let s = s.trim();
    for svc in ["qobuz", "tidal"] {
        if let Some(id) = s.strip_prefix(&format!("{svc}:track:")) {
            return id.chars().all(|c| c.is_ascii_digit()).then(|| s.to_string()).filter(|_| !id.is_empty());
        }
    }
    let lower = s.to_ascii_lowercase();
    let svc = if lower.contains("qobuz.com/") {
        "qobuz"
    } else if lower.contains("tidal.com/") {
        "tidal"
    } else {
        return None;
    };
    let after = &s[lower.find("/track/")? + "/track/".len()..];
    let id: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!id.is_empty()).then(|| format!("{svc}:track:{id}"))
}

/// The public page of a streaming track, for exports other apps can follow.
pub fn streaming_link(uri: &str) -> Option<String> {
    if let Some(id) = uri.strip_prefix("qobuz:track:") {
        return Some(format!("https://open.qobuz.com/track/{id}"));
    }
    uri.strip_prefix("tidal:track:").map(|id| format!("https://tidal.com/browse/track/{id}"))
}

// ---------------------------------------------------------------------------------------------
// Finding entries in the library
// ---------------------------------------------------------------------------------------------

/// How an entry was found.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// A library track (index into the slice given to [`resolve`]).
    Library(usize),
    /// A file that exists but isn't in the library.
    File(String),
    /// A streaming track id (`qobuz:track:…`).
    Stream(String),
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A playlist location as a path on this computer (relative ones against the playlist's
/// folder). None for web links.
pub fn local_path(location: &str, base: Option<&Path>) -> Option<PathBuf> {
    let l = location.trim();
    let lower = l.to_ascii_lowercase();
    let p = if lower.starts_with("file:") {
        let rest = &l[5..];
        let rest = rest.strip_prefix("//localhost").or_else(|| rest.strip_prefix("//")).unwrap_or(rest);
        let mut p = percent_decode(rest);
        // file:///C:/Music → C:/Music
        if p.len() > 3 && p.starts_with('/') && p.as_bytes()[2] == b':' {
            p.remove(0);
        }
        p
    } else if lower.contains("://") {
        return None;
    } else if let Some(rest) = l.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    } else {
        l.to_string()
    };
    let windows_abs = p.len() > 2 && p.as_bytes()[1] == b':' || p.starts_with("\\\\");
    // Playlists from Windows use backslashes; elsewhere they are path separators too.
    let p = if cfg!(windows) { p } else { p.replace('\\', "/") };
    let path = PathBuf::from(&p);
    if path.is_absolute() || windows_abs {
        Some(path)
    } else {
        Some(base.map_or(path.clone(), |b| b.join(&path)))
    }
}

/// Lower case, letters and digits only ("AC/DC" = "acdc", "Ёлка" = "елка").
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'ё' => 'е',
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            c => c,
        };
        if c.is_alphanumeric() {
            out.push(c);
        }
    }
    out
}

/// A title without what versions add: "(Remastered 2009)", "[Live]", " - 2011 Remaster",
/// "(feat. X)".
fn bare_title(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    for sep in [" - ", " – ", " — "] {
        if let Some((head, tail)) = out.split_once(sep) {
            let t = tail.to_lowercase();
            if ["remaster", "version", "mix", "edit", "live", "mono", "stereo", "demo", "ремастер", "версия"].iter().any(|k| t.contains(k)) {
                out = head.to_string();
            }
        }
    }
    let lower = out.to_lowercase();
    if let Some(i) = [" feat.", " feat ", " ft.", " featuring "].iter().filter_map(|k| lower.find(k)).min() {
        out.truncate(i);
    }
    norm(&out)
}

/// The artists an entry names ("A, B & C feat. D").
fn artist_parts(s: &str) -> Vec<String> {
    let mut s = s.to_lowercase();
    for sep in [" feat. ", " feat ", " ft. ", " featuring ", " & ", " and ", " и ", " x ", ";", ",", "/"] {
        s = s.replace(sep, "\u{1}");
    }
    s.split('\u{1}').map(|p| norm(p.trim().trim_start_matches("the "))).filter(|p| !p.is_empty()).collect()
}

fn artist_matches(entry_artist: &str, t: &TrackInfo) -> bool {
    let theirs: Vec<String> = [t.artist.as_deref(), t.album_artist.as_deref()]
        .into_iter()
        .flatten()
        .flat_map(artist_parts)
        .collect();
    artist_parts(entry_artist).iter().any(|a| theirs.iter().any(|b| a == b || (a.len() >= 4 && b.contains(a.as_str())) || (b.len() >= 4 && a.contains(b.as_str()))))
}

/// Lookup tables over the library, built once per import.
pub struct Index<'a> {
    tracks: &'a [TrackInfo],
    by_file: HashMap<String, Vec<usize>>,
    by_title: HashMap<String, Vec<usize>>,
    by_path: HashMap<&'a str, usize>,
}

impl<'a> Index<'a> {
    pub fn new(tracks: &'a [TrackInfo]) -> Self {
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_title: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_path = HashMap::new();
        for (i, t) in tracks.iter().enumerate() {
            by_path.insert(t.path.as_str(), i);
            if let Some(f) = Path::new(&t.path).file_name() {
                by_file.entry(f.to_string_lossy().to_lowercase()).or_default().push(i);
            }
            let full = norm(&t.title);
            let bare = bare_title(&t.title);
            if bare != full && !bare.is_empty() {
                by_title.entry(bare).or_default().push(i);
            }
            by_title.entry(full).or_default().push(i);
        }
        Self { tracks, by_file, by_title, by_path }
    }

    /// The library track at the same place, or with the same file name and the longest run of
    /// matching folders above it (the same album on another computer or drive).
    fn by_location(&self, path: &Path) -> Option<usize> {
        if let Some(&i) = self.by_path.get(path.to_string_lossy().as_ref()) {
            return Some(i);
        }
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        let theirs: Vec<String> = path.components().rev().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect();
        self.by_file
            .get(&name)?
            .iter()
            .map(|&i| {
                let ours = Path::new(&self.tracks[i].path).components().rev().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
                (ours.zip(&theirs).take_while(|(a, b)| a == *b).count(), i)
            })
            // The file name alone could be "01 Intro.flac" of any album: want a folder too,
            // unless the name is the only candidate.
            .filter(|(n, _)| *n >= 2 || self.by_file[&name].len() == 1)
            .max_by_key(|(n, _)| *n)
            .map(|(_, i)| i)
    }

    /// Best library track for the entry's tags, if one is convincing.
    fn by_tags(&self, e: &Entry) -> Option<usize> {
        let title = e.title.as_deref()?;
        let (full, bare) = (norm(title), bare_title(title));
        let mut candidates: Vec<usize> = self.by_title.get(&full).into_iter().chain(self.by_title.get(&bare)).flatten().copied().collect();
        candidates.sort_unstable();
        candidates.dedup();
        let album = e.album.as_deref().map(bare_title);
        candidates
            .into_iter()
            .filter_map(|i| {
                let t = &self.tracks[i];
                let mut score = if norm(&t.title) == full { 4 } else { 3 };
                match e.artist.as_deref() {
                    Some(a) if artist_matches(a, t) => score += 3,
                    Some(_) => return None,
                    None => {}
                }
                if let (Some(a), Some(b)) = (&album, &t.album) {
                    if !a.is_empty() && *a == bare_title(b) {
                        score += 2;
                    }
                }
                if let Some(d) = e.duration {
                    let diff = (d - t.duration).abs();
                    score += if diff <= 2.0 { 2 } else if diff <= 5.0 { 1 } else if diff > 15.0 { -3 } else { 0 };
                }
                // Title alone, no artist: only with the album or the length agreeing too.
                (score >= 5).then_some((score, t.lossless, t.bits.unwrap_or(0), t.sample_rate, i))
            })
            // Among equals, the best copy: lossless, then more bits, then a higher rate.
            .max()
            .map(|(.., i)| i)
    }
}

/// Find every entry: streaming ids as they are, files by location, the rest by tags.
pub fn resolve(entries: &[Entry], index: &Index, base: Option<&Path>) -> Vec<Option<Found>> {
    entries
        .iter()
        .map(|e| {
            if let Some(loc) = &e.location {
                if let Some(uri) = streaming_uri(loc) {
                    return Some(Found::Stream(uri));
                }
                if let Some(p) = local_path(loc, base) {
                    if let Some(i) = index.by_location(&p) {
                        return Some(Found::Library(i));
                    }
                    if p.is_file() {
                        return Some(Found::File(p.to_string_lossy().into_owned()));
                    }
                }
            }
            index.by_tags(e).map(Found::Library)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    M3u8,
    Pls,
    Xspf,
    Csv,
}

impl Format {
    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "m3u8" | "m3u" => Self::M3u8,
            "pls" => Self::Pls,
            "xspf" => Self::Xspf,
            "csv" => Self::Csv,
            _ => return None,
        })
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

fn file_url(path: &str) -> String {
    let p = path.replace('\\', "/");
    let mut out = String::from(if p.starts_with('/') { "file://" } else { "file:///" });
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Where the entry lives, as the exported file should say it: streaming ids as their public
/// links, files relative to the playlist when they sit below it (so a music folder with its
/// playlist can move as a whole), absolute otherwise.
fn export_location(location: &str, dir: Option<&Path>) -> String {
    if let Some(link) = streaming_link(location) {
        return link;
    }
    if let Some(rel) = dir.and_then(|d| Path::new(location).strip_prefix(d).ok()) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    location.to_string()
}

/// The playlist as text in `format`. `dest` is the file it will be saved as.
pub fn export(name: &str, entries: &[Entry], format: Format, dest: Option<&Path>) -> String {
    let dir = dest.and_then(Path::parent);
    let display = |e: &Entry| match (&e.artist, &e.title) {
        (Some(a), Some(t)) => format!("{a} - {t}"),
        (None, Some(t)) => t.clone(),
        _ => e.location.as_deref().map(|l| Path::new(l).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()).unwrap_or_default(),
    };
    let mut s = String::new();
    match format {
        Format::M3u8 => {
            s.push_str(&format!("#EXTM3U\n#PLAYLIST:{name}\n"));
            for e in entries {
                let Some(loc) = &e.location else { continue };
                s.push_str(&format!("#EXTINF:{},{}\n", e.duration.map_or(-1, |d| d.round() as i64), display(e)));
                if let Some(a) = &e.album {
                    s.push_str(&format!("#EXTALB:{a}\n"));
                }
                s.push_str(&export_location(loc, dir));
                s.push('\n');
            }
        }
        Format::Pls => {
            s.push_str("[playlist]\n");
            s.push_str(&format!("X-Playlist-Name={name}\n"));
            let with_loc: Vec<&Entry> = entries.iter().filter(|e| e.location.is_some()).collect();
            for (i, e) in with_loc.iter().enumerate() {
                let n = i + 1;
                s.push_str(&format!("File{n}={}\n", export_location(e.location.as_deref().unwrap(), dir)));
                s.push_str(&format!("Title{n}={}\n", display(e)));
                s.push_str(&format!("Length{n}={}\n", e.duration.map_or(-1, |d| d.round() as i64)));
            }
            s.push_str(&format!("NumberOfEntries={}\nVersion=2\n", with_loc.len()));
        }
        Format::Xspf => {
            s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">\n");
            s.push_str(&format!("  <title>{}</title>\n  <trackList>\n", xml_escape(name)));
            for e in entries {
                s.push_str("    <track>\n");
                if let Some(loc) = &e.location {
                    let l = match streaming_link(loc) {
                        Some(link) => link,
                        None => file_url(loc),
                    };
                    s.push_str(&format!("      <location>{}</location>\n", xml_escape(&l)));
                }
                for (tag, v) in [("title", &e.title), ("creator", &e.artist), ("album", &e.album)] {
                    if let Some(v) = v {
                        s.push_str(&format!("      <{tag}>{}</{tag}>\n", xml_escape(v)));
                    }
                }
                if let Some(d) = e.duration {
                    s.push_str(&format!("      <duration>{}</duration>\n", (d * 1000.0).round() as i64));
                }
                if let Some(i) = &e.isrc {
                    s.push_str(&format!("      <identifier>isrc:{}</identifier>\n", xml_escape(i)));
                }
                s.push_str("    </track>\n");
            }
            s.push_str("  </trackList>\n</playlist>\n");
        }
        Format::Csv => {
            // The columns TuneMyMusic, Soundiiz and our own import all read.
            s.push_str("Title,Artist,Album,Duration,ISRC,Location\n");
            for e in entries {
                let f = |v: &Option<String>| csv_field(v.as_deref().unwrap_or(""));
                let loc = e.location.as_deref().map(|l| streaming_link(l).unwrap_or_else(|| l.to_string()));
                s.push_str(&format!(
                    "{},{},{},{},{},{}\n",
                    f(&e.title),
                    f(&e.artist),
                    f(&e.album),
                    e.duration.map(|d| short_clock(d)).unwrap_or_default(),
                    f(&e.isrc),
                    f(&loc)
                ));
            }
        }
    }
    s
}

/// "4:05".
fn short_clock(secs: f64) -> String {
    let s = secs.round() as i64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str, title: &str, artist: &str, album: &str, duration: f64) -> TrackInfo {
        TrackInfo {
            path: path.into(),
            title: title.into(),
            artist: Some(artist.into()),
            album_artist: None,
            album: Some(album.into()),
            genre: None,
            year: None,
            track_number: None,
            disc_number: None,
            duration,
            sample_rate: 44_100,
            bits: Some(16),
            channels: 2,
            format: "FLAC".into(),
            lossless: true,
            has_cover: false,
            size: 0,
            mtime: 0,
        }
    }

    fn library() -> Vec<TrackInfo> {
        vec![
            track("/Music/Miles Davis/Kind of Blue/01 So What.flac", "So What", "Miles Davis", "Kind of Blue", 562.0),
            track("/Music/Miles Davis/Kind of Blue/03 Blue in Green.flac", "Blue in Green", "Miles Davis", "Kind of Blue", 337.0),
            track("/Music/The Beatles/Abbey Road/01 Come Together.flac", "Come Together (Remastered 2009)", "The Beatles", "Abbey Road", 259.0),
            track("/Music/Кино/Группа крови/01 Группа крови.flac", "Группа крови", "Кино", "Группа крови", 286.0),
            track("/Music/Other/Intro/01 Intro.flac", "Intro", "Someone", "Intro", 60.0),
            track("/Music/Else/Intro/01 Intro.flac", "Intro", "Someone Else", "Intro", 61.0),
        ]
    }

    #[test]
    fn m3u_with_extinf() {
        let p = &parse(b"#EXTM3U\n#PLAYLIST:Jazz\n#EXTINF:562,Miles Davis - So What\n/x/So What.flac\n\nrelative/b.flac\n", "a.m3u8").unwrap()[0];
        assert_eq!(p.name.as_deref(), Some("Jazz"));
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.entries[0].artist.as_deref(), Some("Miles Davis"));
        assert_eq!(p.entries[0].title.as_deref(), Some("So What"));
        assert_eq!(p.entries[0].duration, Some(562.0));
        assert_eq!(p.entries[1].location.as_deref(), Some("relative/b.flac"));
        assert_eq!(p.entries[1].title, None);
    }

    #[test]
    fn pls_and_name_from_file() {
        let p = &parse(b"[playlist]\nFile1=/a.flac\nTitle1=A - B\nLength1=10\nFile2=/c.flac\nNumberOfEntries=2\n", "Мой.pls").unwrap()[0];
        assert_eq!(p.name.as_deref(), Some("Мой"));
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.entries[0].title.as_deref(), Some("B"));
    }

    #[test]
    fn xspf_roundtrip() {
        let entries = vec![
            Entry { location: Some("/Music/A & B/01 x.flac".into()), title: Some("X <1>".into()), artist: Some("A & B".into()), album: None, duration: Some(61.0), isrc: Some("USX1".into()) },
            Entry { location: Some("qobuz:track:42".into()), title: Some("Q".into()), ..Default::default() },
        ];
        let text = export("Тест", &entries, Format::Xspf, None);
        let p = &parse(text.as_bytes(), "t.xspf").unwrap()[0];
        assert_eq!(p.name.as_deref(), Some("Тест"));
        assert_eq!(p.entries[0].title.as_deref(), Some("X <1>"));
        assert_eq!(p.entries[0].isrc.as_deref(), Some("USX1"));
        assert_eq!(local_path(p.entries[0].location.as_deref().unwrap(), None).unwrap(), PathBuf::from("/Music/A & B/01 x.flac"));
        assert_eq!(streaming_uri(p.entries[1].location.as_deref().unwrap()).as_deref(), Some("qobuz:track:42"));
    }

    #[test]
    fn exportify_csv() {
        let csv = "\"Track URI\",\"Track Name\",\"Artist URI(s)\",\"Artist Name(s)\",\"Album Name\",\"Duration (ms)\",\"ISRC\"\n\
                   \"spotify:track:1\",\"Come Together - Remastered 2009\",\"x\",\"The Beatles\",\"Abbey Road (Remastered)\",\"259946\",\"GBAYE0601690\"\n\
                   \"spotify:track:2\",\"Song, with comma\",\"x\",\"A, B\",\"Al\",\"1000\",\"\"\n";
        let p = &parse(csv.as_bytes(), "liked.csv").unwrap()[0];
        assert_eq!(p.entries.len(), 2);
        assert_eq!(p.entries[0].duration.map(|d| d.round()), Some(260.0));
        assert_eq!(p.entries[0].isrc.as_deref(), Some("GBAYE0601690"));
        assert_eq!(p.entries[0].location, None, "Spotify URIs are no use to us");
        assert_eq!(p.entries[1].title.as_deref(), Some("Song, with comma"));
        let lib = library();
        let found = resolve(&p.entries, &Index::new(&lib), None);
        assert_eq!(found[0], Some(Found::Library(2)));
        assert_eq!(found[1], None);
    }

    #[test]
    fn apple_music_text_export_utf16() {
        let text = "Название\tАртист\tКомпозитор\tАльбом\tВремя\tРасположение\r\nSo What\tMiles Davis\t\tKind of Blue\t562\tMacintosh HD:Users:old:Music:Miles Davis:Kind of Blue:01 So What.flac\r\n";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        let p = &parse(&bytes, "Jazz.txt").unwrap()[0];
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].location.as_deref(), Some("/Users/old/Music/Miles Davis/Kind of Blue/01 So What.flac"));
        let lib = library();
        // Found by the tail of the old path.
        assert_eq!(resolve(&p.entries, &Index::new(&lib), None)[0], Some(Found::Library(0)));
    }

    #[test]
    fn itunes_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
 <key>Tracks</key><dict>
  <key>10</key><dict><key>Track ID</key><integer>10</integer><key>Name</key><string>Blue in Green</string><key>Artist</key><string>Miles Davis</string><key>Total Time</key><integer>337000</integer><key>Location</key><string>file:///Volumes/Old/Miles%20Davis/Kind%20of%20Blue/03%20Blue%20in%20Green.flac</string></dict>
 </dict>
 <key>Playlists</key><array>
  <dict><key>Name</key><string>Library</string><key>Master</key><true/><key>Playlist Items</key><array><dict><key>Track ID</key><integer>10</integer></dict></array></dict>
  <dict><key>Name</key><string>Night</string><key>Playlist Items</key><array><dict><key>Track ID</key><integer>10</integer></dict></array></dict>
 </array>
</dict></plist>"#;
        let all = parse(xml.as_bytes(), "Library.xml").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name.as_deref(), Some("Night"));
        let lib = library();
        assert_eq!(resolve(&all[0].entries, &Index::new(&lib), None)[0], Some(Found::Library(1)));
    }

    #[test]
    fn wpl() {
        let p = &parse(br#"<?wpl version="1.0"?><smil><head><title>W</title></head><body><seq><media src="..\Kind of Blue\01 So What.flac"/></seq></body></smil>"#, "w.wpl").unwrap()[0];
        assert_eq!(p.name.as_deref(), Some("W"));
        assert_eq!(p.entries.len(), 1);
    }

    #[test]
    fn plain_lines_and_links() {
        let all = parse_text("1. Кино — Группа крови\nhttps://open.qobuz.com/track/123456\nhttps://tidal.com/browse/track/77?u\nMiles Davis - so what\n").unwrap();
        let p = &all[0];
        assert_eq!(p.entries.len(), 4);
        assert_eq!(p.entries[0].artist.as_deref(), Some("Кино"));
        let lib = library();
        let found = resolve(&p.entries, &Index::new(&lib), None);
        assert_eq!(found[0], Some(Found::Library(3)));
        assert_eq!(found[1], Some(Found::Stream("qobuz:track:123456".into())));
        assert_eq!(found[2], Some(Found::Stream("tidal:track:77".into())));
        assert_eq!(found[3], Some(Found::Library(0)));
    }

    #[test]
    fn ambiguous_file_name_needs_a_folder() {
        let lib = library();
        let idx = Index::new(&lib);
        assert_eq!(idx.by_location(Path::new("/elsewhere/01 Intro.flac")), None);
        assert_eq!(idx.by_location(Path::new("/elsewhere/Else/Intro/01 Intro.flac")), Some(5));
    }

    #[test]
    fn wrong_artist_is_no_match() {
        let lib = library();
        let e = Entry { title: Some("So What".into()), artist: Some("Pink Floyd".into()), ..Default::default() };
        assert_eq!(resolve(&[e], &Index::new(&lib), None)[0], None);
    }

    #[test]
    fn cp1251_m3u() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode("#EXTM3U\n#EXTINF:286,Кино - Группа крови\nD:\\Музыка\\Кино\\Группа крови\\01 Группа крови.flac\n");
        let p = &parse(&bytes, "old.m3u").unwrap()[0];
        assert_eq!(p.entries[0].artist.as_deref(), Some("Кино"));
        let lib = library();
        assert_eq!(resolve(&p.entries, &Index::new(&lib), None)[0], Some(Found::Library(3)));
    }

    #[test]
    fn m3u_export_relative_below_playlist() {
        let entries = vec![
            Entry { location: Some("/Music/Miles Davis/Kind of Blue/01 So What.flac".into()), title: Some("So What".into()), artist: Some("Miles Davis".into()), duration: Some(562.0), ..Default::default() },
            Entry { location: Some("/Elsewhere/x.flac".into()), ..Default::default() },
        ];
        let s = export("J", &entries, Format::M3u8, Some(Path::new("/Music/J.m3u8")));
        assert!(s.contains("#EXTINF:562,Miles Davis - So What\nMiles Davis/Kind of Blue/01 So What.flac\n"), "{s}");
        assert!(s.contains("\n/Elsewhere/x.flac\n"));
        let back = &parse(s.as_bytes(), "J.m3u8").unwrap()[0];
        assert_eq!(local_path(back.entries[0].location.as_deref().unwrap(), Some(Path::new("/Music"))).unwrap(), PathBuf::from("/Music/Miles Davis/Kind of Blue/01 So What.flac"));
    }

    #[test]
    fn csv_export_reads_back() {
        let entries = vec![Entry { location: Some("tidal:track:5".into()), title: Some("A \"quoted\", title".into()), artist: Some("X".into()), duration: Some(3725.0), ..Default::default() }];
        let s = export("C", &entries, Format::Csv, None);
        let back = &parse(s.as_bytes(), "c.csv").unwrap()[0];
        assert_eq!(back.entries[0].title.as_deref(), Some("A \"quoted\", title"));
        assert_eq!(back.entries[0].duration, Some(3725.0));
        assert_eq!(back.entries[0].location.as_deref(), Some("https://tidal.com/browse/track/5"));
    }
}
