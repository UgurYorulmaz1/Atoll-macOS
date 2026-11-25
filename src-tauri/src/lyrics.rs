// Lyrics: LRCLIB (lrclib.net). Only the track title, artist and duration are sent.
// Returns word-level timings (lyricsfile) when available, otherwise word timings estimated from line timings.
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

const UA: &str = "PC-Dynamic-Island/1.0";
static CACHE: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

#[derive(Deserialize)]
pub struct Query { title: String, #[serde(default)] artist: String, #[serde(default)] duration: f64 }

#[derive(Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct Rec {
    #[serde(default)] duration: f64,
    #[serde(default)] instrumental: bool,
    #[serde(default)] has_word_sync: bool,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
    lyricsfile: Option<String>,
}

#[derive(Deserialize)] struct WFile { lines: Option<Vec<WLine>> }
#[derive(Deserialize)] struct WLine { text: Option<String>, start_ms: Option<f64>, end_ms: Option<f64>, words: Option<Vec<WWord>> }
#[derive(Deserialize)] struct WWord { text: Option<serde_yaml::Value>, start_ms: Option<f64>, end_ms: Option<f64> }

/// "Song (Remastered) - X Cover" -> "Song"
fn clean(t: &str) -> String {
    let mut s = String::new();
    let mut depth = 0;
    for c in t.chars() {
        match c { '(' | '[' => depth += 1, ')' | ']' => { if depth > 0 { depth -= 1; } } _ if depth == 0 => s.push(c), _ => {} }
    }
    s.split(" - ").next().unwrap_or("").trim().to_string()
}
fn first_artist(a: &str) -> String {
    a.split(|c| c == ',' || c == '&').next().unwrap_or("").split(" feat").next().unwrap_or("").trim().to_string()
}

fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut rest = raw;
        let mut times = Vec::new();
        while let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            let tag = &r[..end];
            if let Some((m, s)) = tag.split_once(':') {
                if let (Ok(m), Ok(s)) = (m.parse::<f64>(), s.parse::<f64>()) { times.push(m * 60.0 + s); }
            }
            rest = &r[end + 1..];
        }
        let words = rest.trim().to_string();
        for t in times { out.push((t, words.clone())); }
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    out
}

/// Split a line's duration across its words by letter count
fn estimate_words(lines: &[(f64, String)]) -> Vec<Value> {
    lines.iter().enumerate().map(|(i, (t, text))| {
        let next = lines.get(i + 1).map(|l| l.0).unwrap_or(t + 6.0);
        let sing = (next - t).min((text.chars().count() as f64 * 0.075).max(1.2));
        let end = t + sing.max(0.4);
        let parts: Vec<String> = text.split_inclusive(' ').map(String::from).collect();
        let total: f64 = parts.iter().map(|p| p.trim().chars().count() as f64 + 1.0).sum::<f64>().max(1.0);
        let mut cur = *t;
        let words: Vec<Value> = parts.iter().map(|p| {
            let d = (end - t) * (p.trim().chars().count() as f64 + 1.0) / total;
            let w = json!({ "text": p, "t": cur, "end": cur + d });
            cur += d;
            w
        }).collect();
        json!({ "t": t, "end": end, "text": text, "words": words })
    }).collect()
}

fn yaml_text(v: &Option<serde_yaml::Value>) -> String {
    match v {
        Some(serde_yaml::Value::String(s)) => s.clone(),
        Some(serde_yaml::Value::Number(n)) => n.to_string(),
        Some(serde_yaml::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn parse_word_file(doc: &str) -> Option<Vec<Value>> {
    let f: WFile = serde_yaml::from_str(doc).ok()?;
    let mut lines: Vec<Value> = f.lines?.into_iter().filter_map(|l| {
        let s = l.start_ms?;
        let e = l.end_ms.unwrap_or(s + 3000.0);
        let ws = l.words.unwrap_or_default();
        let words: Vec<Value> = if ws.is_empty() {
            vec![json!({ "text": l.text.clone().unwrap_or_default(), "t": s / 1000.0, "end": e / 1000.0 })]
        } else {
            ws.iter().enumerate().map(|(i, w)| {
                let ws_ = w.start_ms.unwrap_or(s);
                let we = w.end_ms.or_else(|| ws.get(i + 1).and_then(|n| n.start_ms)).unwrap_or(e);
                json!({ "text": yaml_text(&w.text), "t": ws_ / 1000.0, "end": we / 1000.0 })
            }).collect()
        };
        let text = l.text.unwrap_or_else(|| words.iter().filter_map(|w| w["text"].as_str()).collect());
        Some(json!({ "t": s / 1000.0, "end": e / 1000.0, "text": text, "words": words }))
    }).collect();
    lines.sort_by(|a, b| a["t"].as_f64().partial_cmp(&b["t"].as_f64()).unwrap());
    if lines.is_empty() { None } else { Some(lines) }
}

async fn search(track: &str, artist: &str) -> Vec<Rec> {
    let mut u = url::Url::parse("https://lrclib.net/api/search").unwrap();
    u.query_pairs_mut().append_pair("track_name", track);
    if !artist.is_empty() { u.query_pairs_mut().append_pair("artist_name", artist); }
    let Ok(r) = reqwest::Client::new().get(u).header("User-Agent", UA).send().await else { return vec![] };
    if !r.status().is_success() { return vec![]; }
    r.json::<Vec<Rec>>().await.unwrap_or_default()
}

async fn get(q: &Query) -> Value {
    let key = format!("{}|{}", q.title, q.artist);
    if let Some(v) = CACHE.lock().unwrap().as_ref().and_then(|c| c.get(&key)) { return v.clone(); }
    let tries = [(q.title.clone(), q.artist.clone()), (clean(&q.title), first_artist(&q.artist)), (clean(&q.title), String::new())];
    // Score (lower = better): duration difference + penalty when word data is missing -> avoid picking word data from the wrong version
    let score = |x: &Rec| (if q.duration > 0.0 { (x.duration - q.duration).abs() } else { 0.0 })
        + if x.has_word_sync && x.lyricsfile.is_some() { 0.0 } else if x.synced_lyrics.is_some() { 6.0 } else { 1000.0 };
    let mut best: Option<Rec> = None;
    for (t, a) in tries.iter() {
        if t.is_empty() { continue; }
        let mut list: Vec<Rec> = search(t, a).await.into_iter()
            .filter(|x| x.synced_lyrics.is_some() || x.plain_lyrics.is_some() || x.instrumental || x.lyricsfile.is_some()).collect();
        if list.is_empty() { continue; }
        list.sort_by(|p, r| score(p).partial_cmp(&score(r)).unwrap());
        let b = list.remove(0);
        let close = q.duration <= 0.0 || (b.duration - q.duration).abs() < 15.0 || a.is_empty();
        best = Some(b);
        if close { break; }
    }
    let res = match best {
        None => json!({ "found": false }),
        Some(b) if b.instrumental => json!({ "found": true, "instrumental": true }),
        Some(b) => {
            let word = if b.has_word_sync { b.lyricsfile.as_deref().and_then(parse_word_file) } else { None };
            if let Some(lines) = word { json!({ "found": true, "synced": true, "word": true, "lines": lines }) }
            else if let Some(s) = &b.synced_lyrics {
                let l: Vec<(f64, String)> = parse_lrc(s).into_iter().filter(|x| !x.1.is_empty()).collect();
                json!({ "found": true, "synced": true, "word": false, "lines": estimate_words(&l) })
            } else {
                let lines: Vec<Value> = b.plain_lyrics.unwrap_or_default().lines().map(|t| json!({ "t": -1, "text": t.trim() })).collect();
                json!({ "found": true, "synced": false, "lines": lines })
            }
        }
    };
    let mut g = CACHE.lock().unwrap();
    let c = g.get_or_insert_with(HashMap::new);
    if c.len() > 50 { c.clear(); }
    c.insert(key, res.clone());
    res
}

#[tauri::command]
pub async fn lyrics_get(q: Query) -> Value {
    let q = Query { title: q.title.chars().take(200).collect(), artist: q.artist.chars().take(200).collect(), duration: q.duration };
    get(&q).await
}
