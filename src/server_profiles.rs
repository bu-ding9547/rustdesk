use hbb_common::log;
use serde_derive::Deserialize;

/// Local option holding the user's server list. Every enabled entry is
/// connected at the same time; `primary` marks the entry whose API server
/// (login, address book, devices) is used.
pub const OPTION_SERVER_PROFILES: &str = "server-profiles";

#[derive(Clone, Debug, Deserialize)]
struct RawProfile {
    #[serde(default)]
    title: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    relay: String,
    #[serde(default)]
    api: String,
    #[serde(default)]
    key: String,
    /// Absent means enabled, so a hand-written entry keeps working.
    enabled: Option<bool>,
    #[serde(default)]
    primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerProfile {
    pub title: String,
    pub id: String,
    pub relay: String,
    pub api: String,
    pub key: String,
    pub enabled: bool,
    pub primary: bool,
}

impl From<RawProfile> for ServerProfile {
    fn from(raw: RawProfile) -> Self {
        Self {
            title: raw.title.trim().to_owned(),
            id: raw.id.trim().to_owned(),
            relay: raw.relay.trim().to_owned(),
            api: raw.api.trim().to_owned(),
            key: raw.key.trim().to_owned(),
            enabled: raw.enabled.unwrap_or(true),
            primary: raw.primary,
        }
    }
}

/// All saved entries, in list order; empty when nothing is saved or the JSON is broken.
///
/// The list lives in the shared options rather than the local ones: the
/// rendezvous mediator runs in the service process on Windows, whose local
/// config file belongs to a different account.
pub fn all() -> Vec<ServerProfile> {
    let raw = hbb_common::config::Config::get_option(OPTION_SERVER_PROFILES);
    if raw.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<RawProfile>>(&raw) {
        Ok(v) => v.into_iter().map(ServerProfile::from).collect(),
        Err(err) => {
            log::warn!("invalid {OPTION_SERVER_PROFILES}: {err}");
            Vec::new()
        }
    }
}

/// Entries the client should connect to: enabled and carrying an ID server.
pub fn enabled() -> Vec<ServerProfile> {
    all()
        .into_iter()
        .filter(|p| p.enabled && !p.id.is_empty())
        .collect()
}

/// True when the user saved a list with at least one enabled entry. Callers use
/// this to keep the old single-server behaviour while the feature is unused.
pub fn is_configured() -> bool {
    !enabled().is_empty()
}

/// Rendezvous servers to run concurrently.
pub fn hosts() -> Vec<String> {
    enabled().into_iter().map(|p| p.id).collect()
}

fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((head, port)) if !head.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => head,
        _ => host,
    }
}

fn same_host(a: &str, b: &str) -> bool {
    let normalize = |s: &str| s.trim().trim_end_matches('.').to_ascii_lowercase();
    let (a, b) = (normalize(a), normalize(b));
    a == b || strip_port(&a) == strip_port(&b)
}

/// The enabled entry a host belongs to: either its ID server or its relay.
fn entry_for_host<'a>(entries: &'a [ServerProfile], host: &str) -> Option<&'a ServerProfile> {
    entries
        .iter()
        .filter(|p| p.enabled && !p.id.is_empty())
        .find(|p| same_host(&p.id, host) || (!p.relay.is_empty() && same_host(&p.relay, host)))
}

/// Key saved for the server `host` belongs to, if that entry has one.
pub fn key_for_host(host: &str) -> Option<String> {
    entry_for_host(&all(), host)
        .map(|p| p.key.clone())
        .filter(|key| !key.is_empty())
}

/// Relay saved for the server `host` belongs to, if that entry has one.
pub fn relay_for_host(host: &str) -> Option<String> {
    entry_for_host(&all(), host)
        .map(|p| p.relay.clone())
        .filter(|relay| !relay.is_empty())
}

/// API server of the primary entry, else of the first enabled entry with one.
pub fn primary_api() -> String {
    let entries = enabled();
    entries
        .iter()
        .find(|p| p.primary && !p.api.is_empty())
        .or_else(|| entries.iter().find(|p| !p.api.is_empty()))
        .map(|p| p.api.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, key: &str, relay: &str, enabled: bool) -> ServerProfile {
        ServerProfile {
            title: id.to_owned(),
            id: id.to_owned(),
            relay: relay.to_owned(),
            api: String::new(),
            key: key.to_owned(),
            enabled,
            primary: false,
        }
    }

    #[test]
    fn host_matching_ignores_port_case_and_trailing_dot() {
        assert!(same_host("Example.COM:21116", "example.com"));
        assert!(same_host("1.2.3.4:21116", "1.2.3.4"));
        assert!(same_host("1.2.3.4", "1.2.3.4:21116"));
        assert!(!same_host("1.2.3.4", "1.2.3.5"));
        // An IPv6 literal keeps its own colons instead of looking like a port.
        assert!(same_host("[::1]:21116", "[::1]"));
        assert!(!same_host("[::1]", "[::2]"));
    }

    #[test]
    fn a_profile_without_the_enabled_field_counts_as_enabled() {
        let parsed: Vec<RawProfile> =
            serde_json::from_str(r#"[{"title":"a","id":"1.2.3.4","key":"k"}]"#).unwrap();
        let p: ServerProfile = parsed.into_iter().next().unwrap().into();
        assert!(p.enabled);
        assert!(!p.primary);
        assert_eq!(p.key, "k");
    }

    #[test]
    fn a_host_matches_an_entry_by_id_or_by_relay() {
        let entries = [
            profile("a.com", "ak", "", true),
            profile("b.com:21116", "bk", "relay.example:21117", true),
            profile("c.com", "ck", "", false),
        ];
        assert_eq!(
            entry_for_host(&entries, "a.com:21116").map(|p| p.key.as_str()),
            Some("ak")
        );
        assert_eq!(
            entry_for_host(&entries, "B.COM").map(|p| p.key.as_str()),
            Some("bk")
        );
        // reached through that entry's relay host
        assert_eq!(
            entry_for_host(&entries, "relay.example").map(|p| p.key.as_str()),
            Some("bk")
        );
        // a disabled entry is ignored even though its host matches
        assert!(entry_for_host(&entries, "c.com").is_none());
        assert!(entry_for_host(&entries, "d.com").is_none());
    }
}
