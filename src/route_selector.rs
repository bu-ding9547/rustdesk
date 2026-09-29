//! Which saved server to ask first for a peer.
//!
//! The stated goal is a *stable* route, not the lowest average latency: a steady
//! 400 ms beats a link that alternates between 100 ms and 2000 ms. Averaging hides
//! that, so every server keeps a window of recent round trips and is classified by
//! how far its tail sits from its median. A server that has proved it knows a peer
//! is preferred over one that had to answer "unknown", and the result is remembered
//! per peer so later connections start from the route that worked.
//!
//! The choice is made once per connection. Nothing here switches a live session.

use hbb_common::{config::LocalConfig, log};
use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Local option holding what each server did for each peer.
///
/// Local, not shared: the UI pushes its whole shared-option map to the service, so a
/// key the service writes there is erased by the next settings change.
pub const OPTION_ROUTE_MEMORY: &str = "route-memory";
/// Option naming the preference: `latency`, `balanced` or `stability`.
pub const OPTION_ROUTE_PREFERENCE: &str = "route-preference";
/// Server the user picked by hand in the session toolbar; empty means "automatic".
/// The session menu writes it and then reconnects, so this is the manual route switch.
pub const OPTION_ROUTE_FORCED_SERVER: &str = "route-forced-server";

/// Round trips kept per server; the mediator samples about once a second.
const WINDOW: usize = 32;
/// Most peers remembered; the oldest entries are dropped beyond this.
const MAX_ENTRIES: usize = 200;
/// Entries older than a week are ignored, so a peer that moved on is re-learnt.
const ENTRY_TTL_SECS: i64 = 7 * 24 * 3600;
/// Below this many samples a window says nothing, and memory or a neutral
/// classification decides instead.
const MIN_SAMPLES: usize = 3;
/// Neutral score in milli-ms, used when nothing is known about a server.
const NEUTRAL_SCORE: i64 = 200_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preference {
    Latency,
    Balanced,
    Stability,
}

impl Preference {
    pub fn from_option() -> Self {
        match LocalConfig::get_option(OPTION_ROUTE_PREFERENCE)
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "latency" => Self::Latency,
            "balanced" => Self::Balanced,
            _ => Self::Stability,
        }
    }

    /// How much the tail and the jitter count against a server's median.
    fn tail_weight(self) -> f64 {
        match self {
            Self::Latency => 0.25,
            Self::Balanced => 1.0,
            Self::Stability => 2.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Serialize, Deserialize)]
pub enum Stability {
    Poor,
    #[default]
    Fair,
    Stable,
}

/// Recent round trips to one server, in milliseconds, plus the samples that never
/// came back.
#[derive(Clone, Debug, Default)]
pub struct Window {
    rtt_ms: Vec<u32>,
    lost: u32,
}

impl Window {
    pub fn note(&mut self, rtt_ms: u32) {
        if rtt_ms == 0 {
            return;
        }
        if self.rtt_ms.len() == WINDOW {
            self.rtt_ms.remove(0);
        }
        self.rtt_ms.push(rtt_ms);
    }

    pub fn note_loss(&mut self) {
        self.lost = self.lost.saturating_add(1);
    }

    pub fn loss_rate(&self) -> f64 {
        let total = self.rtt_ms.len() as u32 + self.lost;
        if total == 0 {
            0.0
        } else {
            self.lost as f64 / total as f64
        }
    }

    /// Median, 90th percentile and median absolute deviation, all in ms.
    pub fn summary(&self) -> Option<(u32, u32, u32)> {
        if self.rtt_ms.len() < MIN_SAMPLES {
            return None;
        }
        let mut sorted = self.rtt_ms.clone();
        sorted.sort_unstable();
        let p50 = percentile(&sorted, 50);
        let p90 = percentile(&sorted, 90);
        let mut deviations: Vec<u32> = sorted.iter().map(|v| v.abs_diff(p50)).collect();
        deviations.sort_unstable();
        Some((p50, p90, percentile(&deviations, 50)))
    }

    /// Tail spread relative to the median; a small floor keeps a 5 ms link from
    /// looking unstable over 1 ms of noise.
    pub fn spread(&self) -> Option<f64> {
        let (p50, p90, _) = self.summary()?;
        Some((p90.saturating_sub(p50)) as f64 / p50.max(20) as f64)
    }

    pub fn stability(&self) -> Option<Stability> {
        let spread = self.spread()?;
        let loss = self.loss_rate();
        Some(if spread <= 0.5 && loss < 0.01 {
            Stability::Stable
        } else if spread <= 1.5 && loss < 0.05 {
            Stability::Fair
        } else {
            Stability::Poor
        })
    }

    /// Lower is better; milli-ms so it stays comparable without floats ordering.
    pub fn score(&self, preference: Preference) -> Option<i64> {
        let (p50, p90, mad) = self.summary()?;
        let tail = (p90.saturating_sub(p50)) as f64 * preference.tail_weight();
        let jitter = mad as f64 * preference.tail_weight();
        Some((p50 as f64 + tail + jitter + self.loss_rate() * 1000.0) as i64)
    }
}

fn percentile(sorted: &[u32], percent: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() * percent + 99) / 100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The server answered with a punch or relay instruction for this peer.
    Found,
    /// The server does not know this peer.
    Miss,
    /// The server could not be reached at all.
    Failed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    class: Stability,
    #[serde(default)]
    p50: u32,
    #[serde(default)]
    spread: f32,
    #[serde(default)]
    found: u32,
    #[serde(default)]
    miss: u32,
    #[serde(default)]
    fail: u32,
    #[serde(default)]
    at: i64,
}

/// Live round-trip windows per server, fed by the rendezvous mediators.
static SERVERS: Mutex<Option<HashMap<String, Window>>> = Mutex::new(None);

fn lock_servers() -> std::sync::MutexGuard<'static, Option<HashMap<String, Window>>> {
    match SERVERS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Record one round trip to `host` (microseconds, as the mediators measure it).
pub fn note_server_rtt(host: &str, rtt_us: i64) {
    if rtt_us <= 0 || host.is_empty() {
        return;
    }
    let ms = (rtt_us / 1000).clamp(1, u32::MAX as i64) as u32;
    let mut guard = lock_servers();
    guard
        .get_or_insert_with(HashMap::new)
        .entry(host.to_owned())
        .or_default()
        .note(ms);
}

/// Record that a registered server did not answer in time.
pub fn note_server_loss(host: &str) {
    if host.is_empty() {
        return;
    }
    let mut guard = lock_servers();
    guard
        .get_or_insert_with(HashMap::new)
        .entry(host.to_owned())
        .or_default()
        .note_loss();
}

fn server_window(host: &str) -> Option<Window> {
    lock_servers()
        .as_ref()
        .and_then(|map| map.get(host))
        .cloned()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn key_of(peer: &str, server: &str) -> String {
    format!("{peer}|{server}")
}

fn load_memory() -> HashMap<String, Entry> {
    let raw = LocalConfig::get_option(OPTION_ROUTE_MEMORY);
    if raw.trim().is_empty() {
        return HashMap::new();
    }
    serde_json::from_str(&raw).unwrap_or_else(|err| {
        log::warn!("invalid {OPTION_ROUTE_MEMORY}: {err}");
        HashMap::new()
    })
}

fn save_memory(memory: &mut HashMap<String, Entry>) {
    let now = now_secs();
    memory.retain(|_, entry| now - entry.at <= ENTRY_TTL_SECS);
    if memory.len() > MAX_ENTRIES {
        let mut by_age: Vec<(i64, String)> = memory
            .iter()
            .map(|(key, entry)| (entry.at, key.clone()))
            .collect();
        by_age.sort_unstable();
        for (_, key) in by_age.iter().take(memory.len() - MAX_ENTRIES) {
            memory.remove(key);
        }
    }
    match serde_json::to_string(memory) {
        Ok(json) => LocalConfig::set_option(OPTION_ROUTE_MEMORY.to_owned(), json),
        Err(err) => log::warn!("cannot store {OPTION_ROUTE_MEMORY}: {err}"),
    }
}

/// Remember what `server` did for `peer`. Called once per connection attempt, so
/// the config write it may trigger is not on any packet path.
pub fn remember(peer: &str, server: &str, outcome: Outcome) {
    if peer.is_empty() || server.is_empty() {
        return;
    }
    let mut memory = load_memory();
    let entry = memory.entry(key_of(peer, server)).or_default();
    match outcome {
        Outcome::Found => entry.found = entry.found.saturating_add(1),
        Outcome::Miss => entry.miss = entry.miss.saturating_add(1),
        Outcome::Failed => entry.fail = entry.fail.saturating_add(1),
    }
    if let Some(window) = server_window(server) {
        if let Some((p50, _, _)) = window.summary() {
            entry.p50 = p50;
            entry.spread = window.spread().unwrap_or_default() as f32;
            if let Some(class) = window.stability() {
                entry.class = class;
            }
        }
    }
    entry.at = now_secs();
    save_memory(&mut memory);
}

/// Order the servers to try for `peer`: the best first. `first` is the server the
/// caller would have used, `rest` the remaining enabled ones.
pub fn order(peer: &str, first: String, rest: Vec<String>, preference: Preference) -> (String, Vec<String>) {
    if rest.is_empty() {
        return (first, rest);
    }
    // A server picked in the session toolbar wins over everything the selector knows.
    let forced = LocalConfig::get_option(OPTION_ROUTE_FORCED_SERVER);
    let forced = forced.trim();
    let mut first = first;
    let mut rest = rest;
    if !forced.is_empty() {
        if forced == first {
            log::info!("route order for {peer}: kept at the server chosen by hand, {first}");
            return (first, rest);
        }
        if let Some(index) = rest.iter().position(|server| server == forced) {
            let chosen = rest.remove(index);
            rest.insert(0, first);
            first = chosen;
            log::info!("route order for {peer}: server chosen by hand, {first} then {rest:?}");
            return (first, rest);
        }
        log::warn!(
            "route order for {peer}: the chosen server {forced} is not among the enabled ones, ignoring it"
        );
    }
    let memory = load_memory();
    let mut windows = HashMap::new();
    for server in std::iter::once(&first).chain(rest.iter()) {
        if let Some(window) = server_window(server) {
            windows.insert(server.clone(), window);
        }
    }
    let (first, rest) = order_with(&memory, &windows, peer, first, rest, preference, now_secs());
    // One line per connection: which server is asked first and what the ranking saw.
    // Written at info so a user's log shows why a route was chosen.
    log::info!("route order for {peer}: {} then {:?}", first, rest);
    (first, rest)
}

/// [`order`] without the shared state, so the ranking can be tested against crafted
/// memory and windows.
fn order_with(
    memory: &HashMap<String, Entry>,
    windows: &HashMap<String, Window>,
    peer: &str,
    first: String,
    rest: Vec<String>,
    preference: Preference,
    now: i64,
) -> (String, Vec<String>) {
    let mut all: Vec<String> = Vec::with_capacity(rest.len() + 1);
    all.push(first);
    all.extend(rest);
    let mut ranked: Vec<(u8, i64, usize, String)> = all
        .into_iter()
        .enumerate()
        .map(|(index, server)| {
            let window = windows.get(&server);
            let (class, score) = rank_with(memory.get(&key_of(peer, &server)), window, preference, now);
            (class, score, index, server)
        })
        .collect();
    // Highest class first, then lowest score, then the caller's own order.
    ranked.sort_by(|a, b| {
        if ranks_before(a, b) {
            std::cmp::Ordering::Less
        } else if ranks_before(b, a) {
            std::cmp::Ordering::Greater
        } else {
            a.2.cmp(&b.2)
        }
    });
    let mut servers = ranked.into_iter().map(|(_, _, _, server)| server);
    let first = servers.next().unwrap_or_default();
    (first, servers.collect())
}

/// True when the rank of `a` should be tried before the rank of `b`. Kept next to
/// [`order`] so the tests assert the comparator the code really sorts with.
fn ranks_before(a: &(u8, i64, usize, String), b: &(u8, i64, usize, String)) -> bool {
    if a.0 != b.0 {
        a.0 > b.0
    } else {
        a.1 < b.1
    }
}

/// The comparison behind [`order`], without the shared state so it can be tested
/// against synthetic windows.
fn rank_with(
    entry: Option<&Entry>,
    live: Option<&Window>,
    preference: Preference,
    now: i64,
) -> (u8, i64) {
    let entry = entry.filter(|entry| now - entry.at <= ENTRY_TTL_SECS);
    // A server that answered "unknown" for this peer twice more often than it found
    // it is not worth asking first, however good its link looks.
    if let Some(entry) = entry {
        if entry.miss >= 2 && entry.miss > entry.found {
            return (Stability::Poor as u8, NEUTRAL_SCORE);
        }
    }
    let live_class = live.and_then(|window| window.stability());
    let live_score = live.and_then(|window| window.score(preference));
    // The stored class only counts when it was measured: a freshly created entry has
    // no quality yet, and treating its placeholder as a real one would push a server
    // that did find the peer below one that never answered.
    let class = entry
        .filter(|entry| entry.p50 > 0)
        .map(|entry| entry.class)
        .or(live_class)
        .unwrap_or(Stability::Fair);
    let score = entry
        .filter(|entry| entry.p50 > 0)
        .map(|entry| entry.p50 as i64 + (entry.spread as f64 * entry.p50 as f64) as i64)
        .or(live_score)
        .unwrap_or(NEUTRAL_SCORE);
    let penalty = entry.map(|entry| entry.fail as i64 * 250).unwrap_or(0);
    (class as u8, score + penalty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(samples: &[u32]) -> Window {
        let mut window = Window::default();
        for sample in samples {
            window.note(*sample);
        }
        window
    }

    fn steady(value: u32) -> Window {
        window(&vec![value; WINDOW])
    }

    fn flapping(low: u32, high: u32) -> Window {
        let mut window = Window::default();
        for index in 0..WINDOW {
            window.note(if index % 2 == 0 { low } else { high });
        }
        window
    }

    /// Widen a bare rank so the tests compare with the production comparator.
    fn to_full(rank: &(u8, i64)) -> (u8, i64, usize, String) {
        (rank.0, rank.1, 0, String::new())
    }

    /// The case from the requirement: a steady 400 ms must beat 100/2000 ms.
    #[test]
    fn a_steady_route_beats_a_flapping_one() {
        let steady = steady(400);
        let flapping = flapping(100, 2000);
        assert_eq!(steady.stability(), Some(Stability::Stable));
        assert_eq!(flapping.stability(), Some(Stability::Poor));
        let now = now_secs();
        let stable_rank = rank_with(None, Some(&steady), Preference::Stability, now);
        let flapping_rank = rank_with(None, Some(&flapping), Preference::Stability, now);
        assert!(
            ranks_before(&to_full(&stable_rank), &to_full(&flapping_rank)),
            "steady {stable_rank:?} should rank before flapping {flapping_rank:?}"
        );
    }

    /// Even latency-first must not pick a link whose tail is 20x its median.
    #[test]
    fn latency_preference_still_separates_by_class() {
        let fast_but_flapping = flapping(20, 2000);
        let slow_but_steady = steady(400);
        let now = now_secs();
        let steady_rank = rank_with(None, Some(&slow_but_steady), Preference::Latency, now);
        let flapping_rank = rank_with(None, Some(&fast_but_flapping), Preference::Latency, now);
        assert!(ranks_before(&to_full(&steady_rank), &to_full(&flapping_rank)));
    }

    #[test]
    fn a_single_sample_says_nothing() {
        let mut window = Window::default();
        window.note(30);
        assert_eq!(window.summary(), None);
        assert_eq!(window.stability(), None);
    }

    #[test]
    fn loss_moves_a_route_out_of_stable() {
        let mut window = steady(100);
        for _ in 0..WINDOW {
            window.note_loss();
        }
        assert!(window.loss_rate() >= 0.5);
        assert_eq!(window.stability(), Some(Stability::Poor));
    }

    #[test]
    fn a_server_that_does_not_know_the_peer_sinks() {
        let now = now_secs();
        let entry = Entry {
            class: Stability::Stable,
            p50: 30,
            miss: 3,
            found: 1,
            at: now,
            ..Default::default()
        };
        let (class, _) = rank_with(Some(&entry), Some(&steady(30)), Preference::Stability, now);
        assert_eq!(class, Stability::Poor as u8);
    }

    /// A server remembered as having found the peer, but with no quality measured yet,
    /// must not be ranked below one that was never asked: fall back to the live window.
    #[test]
    fn a_found_server_without_measurements_uses_the_live_class() {
        let now = now_secs();
        let fresh = Entry {
            found: 1,
            miss: 0,
            p50: 0,
            class: Stability::Poor,
            at: now,
            ..Default::default()
        };
        let (class, score) = rank_with(Some(&fresh), Some(&steady(120)), Preference::Stability, now);
        assert_eq!(class, Stability::Stable as u8);
        assert!(score > 0 && score < NEUTRAL_SCORE);
    }

    /// A remembered route with a measured quality keeps using it.
    #[test]
    fn remembered_quality_outranks_an_unmeasured_live_window() {
        let now = now_secs();
        let entry = Entry {
            class: Stability::Stable,
            p50: 40,
            spread: 0.1,
            found: 2,
            at: now,
            ..Default::default()
        };
        let (class, score) = rank_with(Some(&entry), None, Preference::Stability, now);
        assert_eq!(class, Stability::Stable as u8);
        assert_eq!(score, 44);
    }

    /// The caller's own first server is only a tie-breaker: a server remembered as
    /// stable for this peer, or one whose live window is clearly steadier, is asked
    /// first even when the configuration would have started elsewhere.
    #[test]
    fn the_better_server_is_promoted_over_the_configured_first() {
        let now = now_secs();
        let mut memory = HashMap::new();
        memory.insert(
            key_of("peer", "s2:21116"),
            Entry {
                class: Stability::Poor,
                miss: 9,
                at: now,
                ..Default::default()
            },
        );
        memory.insert(
            key_of("peer", "hk:21116"),
            Entry {
                class: Stability::Stable,
                p50: 40,
                spread: 0.1,
                found: 5,
                at: now,
                ..Default::default()
            },
        );
        let (first, rest) = order_with(
            &memory,
            &HashMap::new(),
            "peer",
            "s2:21116".to_owned(),
            vec!["hk:21116".to_owned()],
            Preference::Stability,
            now,
        );
        assert_eq!(first, "hk:21116");
        assert_eq!(rest, vec!["s2:21116".to_owned()]);

        // Same conclusion from live windows alone, with no memory at all.
        let mut windows = HashMap::new();
        windows.insert("hk:21116".to_owned(), steady(60));
        windows.insert("s2:21116".to_owned(), flapping(100, 2000));
        let (first, _) = order_with(
            &HashMap::new(),
            &windows,
            "peer",
            "s2:21116".to_owned(),
            vec!["hk:21116".to_owned()],
            Preference::Stability,
            now,
        );
        assert_eq!(first, "hk:21116");
    }

    #[test]
    fn a_peer_that_moved_is_forgotten() {
        let now = now_secs();
        let stale = Entry {
            class: Stability::Stable,
            p50: 30,
            at: now - ENTRY_TTL_SECS - 1,
            ..Default::default()
        };
        assert!(stale.at + ENTRY_TTL_SECS < now);
        // Expired entries fall back to the live window's class.
        let (class, _) = rank_with(Some(&stale), Some(&steady(120)), Preference::Stability, now);
        assert_eq!(class, Stability::Stable as u8);
    }

    #[test]
    fn preference_defaults_to_stability() {
        assert_eq!(Preference::from_option(), Preference::Stability);
    }

    #[test]
    fn order_keeps_the_caller_order_when_nothing_is_known() {
        // A peer id no run ever stores memory for, so the test does not depend on
        // whatever config the machine running it happens to have.
        let (first, rest) = order(
            "route-selector-test-peer",
            "a:21116".to_owned(),
            vec!["b:21116".to_owned(), "c:21116".to_owned()],
            Preference::Stability,
        );
        assert_eq!(first, "a:21116");
        assert_eq!(rest, vec!["b:21116".to_owned(), "c:21116".to_owned()]);
    }

    #[test]
    fn entry_survives_a_json_round_trip() {
        let entry = Entry {
            class: Stability::Fair,
            p50: 123,
            spread: 0.25,
            found: 4,
            miss: 1,
            fail: 2,
            at: 42,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.class, entry.class);
        assert_eq!(back.p50, 123);
        assert_eq!(back.found, 4);
    }
}
