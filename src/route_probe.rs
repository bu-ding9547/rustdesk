use hbb_common::{
    anyhow::anyhow,
    bail,
    config::{Config, CONNECT_TIMEOUT, RENDEZVOUS_PORT},
    log,
    protobuf::Message as _,
    rendezvous_proto::*,
    socket_client::connect_tcp,
    timeout,
    tokio,
    ResultType,
};

use crate::check_port;

/// How long the online query gets before the punch takes over. A server that does not
/// answer this port — closed, or an hbbs without it — must not hold the switch up.
const ONLINE_QUERY_BUDGET: u64 = 1_500;
/// How long one read of a punch waits: the server has to locate the peer and tell it.
const PROBE_READ_TIMEOUT: u64 = 3_000;

/// Verdict of a probe, in the terms the route menu needs: may this server carry a
/// session to the peer, and when not, why not.
pub struct ProbeResult {
    pub online: bool,
    /// `ok`, `not_registered` or `unreachable`; the menu turns it into text.
    pub code: &'static str,
    /// Server address and, when the server could not be asked, the reason it gave.
    pub detail: String,
}

/// Asks one server whether the peer is registered on it, so a route can be checked
/// before the running session is given up for it.
///
/// The online query answers without telling the peer anything, and gets a short budget:
/// a server that does not answer it is asked with a punch hole request instead.
#[tokio::main(flavor = "current_thread")]
pub async fn probe(peer_id: &str, server: &str, key: &str) -> ProbeResult {
    let host = check_port(server, RENDEZVOUS_PORT);
    match timeout(ONLINE_QUERY_BUDGET, online_states(peer_id, &host)).await {
        Ok(Ok(true)) => ProbeResult {
            online: true,
            code: "ok",
            detail: format!("{} knows {}", host, peer_id),
        },
        Ok(Ok(false)) => ProbeResult {
            online: false,
            code: "not_registered",
            detail: format!("{} does not know {}", host, peer_id),
        },
        Ok(Err(err)) => {
            let why = format!("online query failed: {}", err);
            punch_fallback(peer_id, &host, key, &why).await
        }
        Err(_) => punch_fallback(peer_id, &host, key, "online query timed out").await,
    }
}

/// The server is asked with a punch hole request instead, which is what a real connection
/// starts with. That does reach the peer, but it is the only way to learn the answer from
/// a server that does not answer the online query.
async fn punch_fallback(peer_id: &str, host: &str, key: &str, why: &str) -> ProbeResult {
    log::info!("{} for {} on {}, asking for a punch instead", why, peer_id, host);
    match punch_hole(peer_id, host, key).await {
        Ok(true) => ProbeResult {
            online: true,
            code: "ok",
            detail: format!("{} answers for {}", host, peer_id),
        },
        Ok(false) => ProbeResult {
            online: false,
            code: "not_registered",
            detail: format!("{} does not know {}", host, peer_id),
        },
        Err(err) => ProbeResult {
            online: false,
            code: "unreachable",
            detail: format!("{}: {}", host, err),
        },
    }
}

/// The port next to the rendezvous one answers online queries, exactly as the client's
/// own peer list query does it.
fn online_query_addr(host: &str) -> ResultType<String> {
    let (addr, port) = host
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("Invalid server address: {}", host))?;
    let port: u16 = port.parse()?;
    if port == 0 {
        bail!("Invalid server address: {}", host);
    }
    Ok(format!("{}:{}", addr, port - 1))
}

/// `true` when that server has the peer registered, `false` when it answered without it.
async fn online_states(peer_id: &str, host: &str) -> ResultType<bool> {
    let addr = online_query_addr(host)?;
    let mut socket = connect_tcp(&*addr, CONNECT_TIMEOUT).await?;
    let mut msg_out = RendezvousMessage::new();
    msg_out.set_online_request(OnlineRequest {
        id: Config::get_id(),
        peers: vec![peer_id.to_owned()],
        ..Default::default()
    });
    socket.send(&msg_out).await?;
    for _ in 0..2 {
        let Some(msg_in) =
            crate::get_next_nonkeyexchange_msg(&mut socket, Some(PROBE_READ_TIMEOUT)).await
        else {
            break;
        };
        if let Some(rendezvous_message::Union::OnlineResponse(response)) = msg_in.union {
            let Some(states) = response.states.first() else {
                break;
            };
            return Ok((*states & 0x80) == 0x80);
        }
    }
    bail!("no online response from {}", addr)
}

/// `true` when the server hands out an address or a relay for the peer, which it only
/// does for a peer it knows.
async fn punch_hole(peer_id: &str, host: &str, key: &str) -> ResultType<bool> {
    let mut socket = connect_tcp(&*host, CONNECT_TIMEOUT).await?;
    let mut msg_out = RendezvousMessage::new();
    msg_out.set_punch_hole_request(PunchHoleRequest {
        id: peer_id.to_owned(),
        licence_key: key.to_owned(),
        version: crate::VERSION.to_owned(),
        ..Default::default()
    });
    socket.send(&msg_out).await?;
    for _ in 0..2 {
        let Some(msg_in) =
            crate::get_next_nonkeyexchange_msg(&mut socket, Some(PROBE_READ_TIMEOUT)).await
        else {
            break;
        };
        match msg_in.union {
            Some(rendezvous_message::Union::PunchHoleResponse(response)) => {
                return Ok(!response.socket_addr.is_empty() || !response.relay_server.is_empty());
            }
            Some(rendezvous_message::Union::RelayResponse(_)) => return Ok(true),
            _ => {}
        }
    }
    bail!("no punch response from {}", host)
}
