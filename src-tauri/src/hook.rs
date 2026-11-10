//! Sanctuary Screens webhook: tells the display which hymn is playing.
//!
//! Audio never waits for the network: a watcher notices pads starting and stopping and hands
//! small jobs to one sender thread, which posts them with a short timeout and records the
//! outcome for a status indicator. Failures are shown, logged (never the token) and not retried.

use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        mpsc::{channel, Sender},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TIMEOUT: Duration = Duration::from_secs(3);
/// After the last song ends, wait this long for the next one before telling the display.
const END_GRACE: Duration = Duration::from_millis(600);

/// What a pad's song is, as the display wants it.
#[derive(Debug, Clone, PartialEq)]
pub struct Song {
    pub songbook: String,
    pub title: String,
    pub page: String,
}

/// A pad: (is it the favorites bank, index).
pub type PadKey = (bool, usize);

#[derive(Debug, Clone, Serialize, Default)]
pub struct HookStatus {
    /// None until the first request.
    pub ok: Option<bool>,
    pub message: String,
    /// Unix seconds of the last request.
    pub at: u64,
}

#[derive(Clone)]
pub struct Job {
    pub url: String,
    pub token: String,
    pub body: Value,
    pub label: &'static str,
}

pub struct Hook {
    tx: Sender<Job>,
    pub status: Arc<Mutex<HookStatus>>,
}

/// `192.168.1.50:8091` or `http://host:8091/` -> `http://host:8091/api/hooks/bible`
pub fn endpoint(base: &str) -> Result<String, String> {
    let mut base = base.trim().trim_end_matches('/');
    // Accept the full webhook address as shown in Sanctuary Screens, with or without /api/hooks/bible.
    for suffix in ["/api/hooks/bible", "/api/hooks", "/api"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.trim_end_matches('/');
            break;
        }
    }
    if base.is_empty() {
        return Err("Enter the Sanctuary Screens address, for example http://192.168.1.50:8091".into());
    }
    let base = if base.starts_with("http://") || base.starts_with("https://") { base.to_string() } else { format!("http://{base}") };
    Ok(format!("{base}/api/hooks/bible"))
}

/// Songbook names as the display knows them: "Local = Remote" lines, case-insensitive.
pub fn map_songbook(map: &str, name: &str) -> String {
    for line in map.lines() {
        if let Some((from, to)) = line.split_once('=') {
            if from.trim().eq_ignore_ascii_case(name.trim()) && !to.trim().is_empty() {
                return to.trim().to_string();
            }
        }
    }
    name.to_string()
}

pub fn hymn_body(song: &Song) -> Value {
    json!({"action": "hymn", "songbook": song.songbook, "title": song.title, "page": song.page})
}

pub fn clear_if_hymn_body() -> Value {
    json!({"action": "clear", "only_if": "hymn"})
}

/// Plain-language result of one request. `Ok` text is shown quietly, `Err` text as a problem.
fn describe(label: &str, status: u16, body: &Value) -> Result<String, String> {
    match status {
        200 => Ok(match label {
            "hymn" => {
                if body.get("song").map_or(true, Value::is_null) {
                    "Sent (song not in the display's songbook; slide built from what was sent)".to_string()
                } else {
                    "Sent".to_string()
                }
            }
            _ => "Sent".to_string(),
        }),
        401 => Err("Wrong token".into()),
        403 => Err("Webhook is off in Sanctuary Screens, or no token has been created yet".into()),
        404 => Err("Song not found in the display's songbook (check the songbook name and page)".into()),
        409 => Err("Several songs share that page and the title did not pick one".into()),
        422 => Err("The display rejected the request".into()),
        other => Err(format!("The display answered with an error ({other})")),
    }
}

/// Add what was sent (for a hymn) and the server's own explanation to a failure message.
fn with_context(job: &Job, body: &Value, msg: String) -> String {
    let mut out = msg;
    if job.label == "hymn" {
        let g = |k: &str| job.body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        out.push_str(&format!(" [sent: {} / {} / p. {}]", g("songbook"), g("title"), g("page")));
    }
    if let Some(d) = body.get("detail").and_then(Value::as_str) {
        out.push_str(&format!(" ({d})"));
    } else if !body.is_null() {
        let raw = body.to_string();
        out.push_str(&format!(" (server said: {})", raw.chars().take(200).collect::<String>()));
    }
    out.push_str(&format!(" [to: {}]", job.url));
    out
}

/// One blocking request. Returns the text for the status line.
pub fn post(job: &Job) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let result = agent
        .post(&job.url)
        .set("Authorization", &format!("Bearer {}", job.token))
        .set("Content-Type", "application/json")
        .send_string(&job.body.to_string());
    match result {
        Ok(resp) => {
            let status = resp.status();
            let body: Value = resp.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            describe(job.label, status, &body)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body: Value = resp.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            describe(job.label, code, &body).map_err(|m| with_context(job, &body, m))
        }
        Err(ureq::Error::Transport(t)) => {
            let text = t.to_string().to_ascii_lowercase();
            Err(if text.contains("timed out") || text.contains("timeout") {
                "Sanctuary Screens did not answer in time".to_string()
            } else {
                "Cannot reach Sanctuary Screens (is it on, and is the address right?)".to_string()
            })
        }
    }
}

impl Hook {
    pub fn new() -> Self {
        let (tx, rx) = channel::<Job>();
        let status = Arc::new(Mutex::new(HookStatus::default()));
        let shared = status.clone();
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let result = post(&job);
                let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let mut st = shared.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(m) => *st = HookStatus { ok: Some(true), message: m, at },
                    Err(m) => {
                        eprintln!("Sanctuary Screens webhook ({}): {m}", job.label);
                        *st = HookStatus { ok: Some(false), message: m, at };
                    }
                }
            }
        });
        Self { tx, status }
    }

    /// Queue a request; never blocks.
    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }

    pub fn status(&self) -> HookStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// What the watcher should do after looking at the pads.
#[derive(Debug, PartialEq)]
pub enum Action {
    Hymn(Song),
    ClearIfHymn,
}

/// Turns "which pads are playing" snapshots into hymn / clear actions.
#[derive(Default)]
pub struct Tracker {
    prev_playing: HashSet<PadKey>,
    /// Pads playing a song we announced.
    announced: HashSet<PadKey>,
    ended_at: Option<Instant>,
}

impl Tracker {
    /// `playing` = every pad that is playing now; `songs` = song info by pad.
    pub fn update(&mut self, playing: &HashSet<PadKey>, songs: &HashMap<PadKey, Song>, clear_on_end: bool, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let mut started_song = false;
        let mut started: Vec<PadKey> = playing.difference(&self.prev_playing).copied().collect();
        started.sort();
        for key in started {
            if let Some(song) = songs.get(&key) {
                out.push(Action::Hymn(song.clone()));
                self.announced.insert(key);
                started_song = true;
            }
        }
        let was_announced = !self.announced.is_empty();
        self.announced.retain(|k| playing.contains(k));
        if started_song {
            self.ended_at = None;
        } else if was_announced && self.announced.is_empty() && self.ended_at.is_none() {
            self.ended_at = Some(now);
        }
        if let Some(at) = self.ended_at {
            if !self.announced.is_empty() {
                self.ended_at = None;
            } else if now.duration_since(at) >= END_GRACE {
                self.ended_at = None;
                if clear_on_end {
                    out.push(Action::ClearIfHymn);
                }
            }
        }
        self.prev_playing = playing.clone();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn song(t: &str) -> Song {
        Song { songbook: "Baptist Hymnal".into(), title: t.into(), page: "112".into() }
    }
    fn set(keys: &[PadKey]) -> HashSet<PadKey> {
        keys.iter().copied().collect()
    }

    #[test]
    fn endpoint_is_built_from_what_people_type() {
        assert_eq!(endpoint("192.168.1.50:8091").unwrap(), "http://192.168.1.50:8091/api/hooks/bible");
        assert_eq!(endpoint(" http://nuc:8091/ ").unwrap(), "http://nuc:8091/api/hooks/bible");
        assert_eq!(endpoint("https://x.example").unwrap(), "https://x.example/api/hooks/bible");
        assert_eq!(endpoint("http://nuc:8091/api/hooks/bible").unwrap(), "http://nuc:8091/api/hooks/bible");
        assert_eq!(endpoint("http://nuc:8091/api/hooks/bible/").unwrap(), "http://nuc:8091/api/hooks/bible");
        assert_eq!(endpoint("nuc:8091/api/hooks/bible").unwrap(), "http://nuc:8091/api/hooks/bible");
        assert!(endpoint("  ").is_err());
    }

    #[test]
    fn songbook_names_can_be_mapped() {
        let map = "Hymnal = Baptist Hymnal\n baptist hymnal 2008 = Baptist Hymnal\nbad line";
        assert_eq!(map_songbook(map, "hymnal"), "Baptist Hymnal");
        assert_eq!(map_songbook(map, "Baptist Hymnal 2008"), "Baptist Hymnal");
        assert_eq!(map_songbook(map, "Chorus Book"), "Chorus Book");
    }

    #[test]
    fn bodies_match_the_contract() {
        assert_eq!(hymn_body(&song("Amazing Grace")).to_string(), r#"{"action":"hymn","page":"112","songbook":"Baptist Hymnal","title":"Amazing Grace"}"#);
        assert_eq!(clear_if_hymn_body().to_string(), r#"{"action":"clear","only_if":"hymn"}"#);
    }

    #[test]
    fn song_start_sends_hymn_and_end_sends_guarded_clear_once() {
        let t0 = Instant::now();
        let songs = HashMap::from([((false, 0), song("A")), ((false, 1), song("B"))]);
        let mut tr = Tracker::default();
        assert!(tr.update(&set(&[]), &songs, true, t0).is_empty());
        assert_eq!(tr.update(&set(&[(false, 0)]), &songs, true, t0), vec![Action::Hymn(song("A"))]);
        assert!(tr.update(&set(&[(false, 0)]), &songs, true, t0).is_empty(), "no repeat while playing");
        let t1 = t0 + Duration::from_secs(5);
        assert!(tr.update(&set(&[]), &songs, true, t1).is_empty(), "waits a moment before clearing");
        assert_eq!(tr.update(&set(&[]), &songs, true, t1 + END_GRACE), vec![Action::ClearIfHymn]);
        assert!(tr.update(&set(&[]), &songs, true, t1 + END_GRACE * 5).is_empty(), "only once");
    }

    #[test]
    fn a_song_starting_right_after_replaces_instead_of_clearing() {
        let t0 = Instant::now();
        let songs = HashMap::from([((false, 0), song("A")), ((false, 1), song("B"))]);
        let mut tr = Tracker::default();
        tr.update(&set(&[(false, 0)]), &songs, true, t0);
        assert!(tr.update(&set(&[]), &songs, true, t0 + Duration::from_secs(1)).is_empty());
        let acts = tr.update(&set(&[(false, 1)]), &songs, true, t0 + Duration::from_millis(1300));
        assert_eq!(acts, vec![Action::Hymn(song("B"))], "no clear in between");
        assert!(tr.update(&set(&[(false, 1)]), &songs, true, t0 + Duration::from_secs(3)).is_empty());
    }

    #[test]
    fn overlapping_songs_clear_only_after_the_last_one() {
        let t0 = Instant::now();
        let songs = HashMap::from([((false, 0), song("A")), ((true, 2), song("B"))]);
        let mut tr = Tracker::default();
        tr.update(&set(&[(false, 0)]), &songs, true, t0);
        tr.update(&set(&[(false, 0), (true, 2)]), &songs, true, t0);
        let t = t0 + Duration::from_secs(10);
        assert!(tr.update(&set(&[(true, 2)]), &songs, true, t).is_empty());
        assert!(tr.update(&set(&[(true, 2)]), &songs, true, t + END_GRACE * 2).is_empty());
        tr.update(&set(&[]), &songs, true, t + END_GRACE * 3);
        assert_eq!(tr.update(&set(&[]), &songs, true, t + END_GRACE * 5), vec![Action::ClearIfHymn]);
    }

    #[test]
    fn files_without_song_info_send_nothing_and_clear_can_be_turned_off() {
        let t0 = Instant::now();
        let mut tr = Tracker::default();
        let none = HashMap::new();
        assert!(tr.update(&set(&[(false, 3)]), &none, true, t0).is_empty());
        assert!(tr.update(&set(&[]), &none, true, t0 + Duration::from_secs(9)).is_empty());
        assert!(tr.update(&set(&[]), &none, true, t0 + Duration::from_secs(20)).is_empty(), "no clear for a non-song");

        let songs = HashMap::from([((false, 0), song("A"))]);
        let mut tr = Tracker::default();
        tr.update(&set(&[(false, 0)]), &songs, false, t0);
        tr.update(&set(&[]), &songs, false, t0);
        assert!(tr.update(&set(&[]), &songs, false, t0 + Duration::from_secs(5)).is_empty(), "clear setting off");
    }

    /// A one-request HTTP server: answers with `status` and returns what it was sent.
    fn serve(status: &'static str, reply: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/hooks/bible", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let mut got = String::new();
            loop {
                let n = s.read(&mut buf).unwrap();
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(i) = got.find("\r\n\r\n") {
                    let len = got.to_ascii_lowercase().split("content-length: ").nth(1).and_then(|r| r.split("\r\n").next()?.trim().parse::<usize>().ok()).unwrap_or(0);
                    if got.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            got
        });
        (url, handle)
    }

    #[test]
    fn posts_json_with_bearer_token_and_reads_the_answer() {
        let (url, h) = serve("200 OK", r#"{"state":{},"song":{"title":"x"},"logged":true}"#);
        let job = Job { url, token: "s3cret".into(), body: hymn_body(&song("Amazing Grace")), label: "hymn" };
        assert_eq!(post(&job).unwrap(), "Sent");
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /api/hooks/bible"), "{req}");
        assert!(req.to_ascii_lowercase().contains("authorization: bearer s3cret"), "{req}");
        assert!(req.to_ascii_lowercase().contains("content-type: application/json"));
        assert!(req.contains(r#""title":"Amazing Grace""#), "{req}");
    }

    #[test]
    fn errors_become_plain_messages() {
        let (url, h) = serve("401 Unauthorized", r#"{"detail":"no"}"#);
        let job = Job { url, token: "bad".into(), body: clear_if_hymn_body(), label: "clear" };
        assert!(post(&job).unwrap_err().starts_with("Wrong token (no)"));
        h.join().unwrap();
        let (url, h) = serve("200 OK", r#"{"state":{},"song":null,"logged":false}"#);
        let job = Job { url, token: "t".into(), body: hymn_body(&song("X")), label: "hymn" };
        assert!(post(&job).unwrap().contains("not in the display's songbook"));
        h.join().unwrap();
        // nothing listening
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/hooks/bible", l.local_addr().unwrap());
        drop(l);
        let job = Job { url, token: "t".into(), body: clear_if_hymn_body(), label: "clear" };
        assert!(post(&job).unwrap_err().starts_with("Cannot reach"));
    }
}
