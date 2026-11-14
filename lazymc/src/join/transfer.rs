use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::{select, time};

use crate::config::*;
use crate::mc::uuid;
use crate::net;
use crate::proto::client::{Client, ClientInfo, ClientState};
use crate::proto::packet::{self, RawPacket};
use crate::server::{Server, State};
use crate::types;

use super::MethodResult;

/// Minimum protocol version that supports the transfer packet (1.20.5).
const MIN_PROTOCOL_TRANSFER: u32 = 766;

/// First protocol version (1.21.2) in which the login success packet no longer
/// has the trailing strict error handling flag.
const PROTOCOL_NO_STRICT_ERROR_HANDLING: u32 = 768;

/// Default Minecraft port, used when a configured address has no port.
const DEFAULT_PORT: u16 = 25565;

/// Time the client gets to acknowledge the login.
const LOGIN_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Clientbound login success packet ID, login state.
const PACKET_LOGIN_SUCCESS: u8 = 0x02;

/// Serverbound login acknowledged packet ID, login state.
const PACKET_LOGIN_ACKNOWLEDGED: u8 = 0x03;

/// Keep alive packet ID, configuration state, both directions.
const PACKET_CONFIG_KEEP_ALIVE: u8 = 0x04;

/// Clientbound transfer packet ID, configuration state.
const PACKET_CONFIG_TRANSFER: u8 = 0x0b;

/// Hold the client in the configuration state, then transfer it to the server.
///
/// Unlike the hold method, this is not bound by the 30 second client timeout,
/// because the client is kept alive with keep alive packets. Once the server is
/// ready the client is told to reconnect to it with a transfer packet, which
/// makes it go through a full handshake with the real server. This keeps modded
/// servers working, because no registry or mod data has to be emulated here.
pub async fn occupy(
    client: &Client,
    client_info: ClientInfo,
    config: Arc<Config>,
    server: Arc<Server>,
    mut inbound: TcpStream,
) -> Result<MethodResult, ()> {
    trace!(target: "lazymc", "Using transfer method to occupy joining client");

    // Server must be starting
    if server.state() != State::Starting {
        return Ok(MethodResult::Continue(inbound));
    }

    // Client must support the transfer packet
    let protocol = client_info.protocol().unwrap_or(0);
    if protocol < MIN_PROTOCOL_TRANSFER {
        warn!(
            target: "lazymc",
            "Client protocol {} is too old for transfer method, needs {} (1.20.5) or above",
            protocol, MIN_PROTOCOL_TRANSFER,
        );
        return Ok(MethodResult::Continue(inbound));
    }

    let username = match client_info.username.clone() {
        Some(username) => username,
        None => {
            error!(target: "lazymc", "No username for transfer method, disconnecting");
            return Ok(MethodResult::Continue(inbound));
        }
    };

    // Decide where to send the client before we commit to this method
    let (host, port) = transfer_target(
        config.join.transfer.address.as_deref(),
        client_info
            .handshake
            .as_ref()
            .map(|h| (h.server_addr.as_str(), h.server_port)),
        config.public.address,
    );

    let (mut reader, mut writer) = inbound.split();

    // Send login success, moving the client towards the configuration state
    let payload = encode_login_success(&username, protocol)?;
    let packet = RawPacket::new(PACKET_LOGIN_SUCCESS, payload).encode_with_len(client)?;
    writer.write_all(&packet).await.map_err(|_| ())?;
    trace!(target: "lazymc", "Sent login success to client");

    // Await login acknowledged, client enters the configuration state
    let mut buf = BytesMut::new();
    let acknowledged = time::timeout(LOGIN_ACK_TIMEOUT, async {
        loop {
            match packet::read_packet(client, &mut buf, &mut reader).await {
                Ok(Some((packet, _raw))) if packet.id == PACKET_LOGIN_ACKNOWLEDGED => break true,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break false,
            }
        }
    })
    .await
    .unwrap_or(false);
    if !acknowledged {
        debug!(target: "lazymc", "Client did not acknowledge login, disconnecting");
        return Ok(MethodResult::Consumed);
    }

    client.set_state(ClientState::Configuration);
    debug!(target: "lazymc", "Client entered configuration state, holding until server is ready");

    // Keep the client alive until the server is started
    let interval = Duration::from_secs(config.join.transfer.keep_alive_interval.max(1) as u64);
    let timeout = Duration::from_secs(config.join.transfer.timeout as u64);

    let task_keep_alive = async {
        let mut tick = time::interval(interval);
        let mut id: i64 = 0;
        loop {
            tick.tick().await;
            id += 1;

            let packet = match RawPacket::new(PACKET_CONFIG_KEEP_ALIVE, id.to_be_bytes().to_vec())
                .encode_with_len(client)
            {
                Ok(packet) => packet,
                Err(_) => return,
            };
            if writer.write_all(&packet).await.is_err() {
                trace!(target: "lazymc", "Client gone while keeping alive");
                return;
            }
            trace!(target: "lazymc", "Sent keep alive to held client");
        }
    };

    // Discard anything the client sends (client information, brand, keep alive
    // responses), this also notices a disconnecting client right away
    let task_drain = async {
        while let Ok(Some(_)) = packet::read_packet(client, &mut buf, &mut reader).await {}
    };

    let task_wait = async {
        let mut state = server.state_receiver();
        loop {
            // Check current state first, server may have started in the meantime
            match state.borrow_and_update().deref() {
                State::Starting => {}
                State::Started => break true,
                State::Stopping | State::Stopped => break false,
            }
            if state.changed().await.is_err() {
                break false;
            }
        }
    };

    let ready = select! {
        ready = time::timeout(timeout, task_wait) => match ready {
            Ok(ready) => ready,
            Err(_) => {
                warn!(target: "lazymc", "Transferred client reached timeout of {}s", config.join.transfer.timeout);
                false
            }
        },
        _ = task_keep_alive => {
            trace!(target: "lazymc", "Keep alive loop ended, client probably disconnected");
            return Ok(MethodResult::Consumed);
        }
        _ = task_drain => {
            debug!(target: "lazymc", "Held client disconnected before server was ready");
            return Ok(MethodResult::Consumed);
        }
    };

    if !ready {
        return Ok(MethodResult::Consumed);
    }

    // Server is ready, transfer the client to it
    info!(target: "lazymc", "Server ready, transferring client to {}:{}", host, port);

    let mut payload = encode_string(&host)?;
    payload.append(&mut types::encode_var_int(port as i32)?);
    let packet = RawPacket::new(PACKET_CONFIG_TRANSFER, payload).encode_with_len(client)?;
    writer.write_all(&packet).await.map_err(|_| ())?;

    // Give the client a moment to act on it, then close
    time::sleep(Duration::from_millis(500)).await;
    let _ = net::close_tcp_stream(inbound).await;

    Ok(MethodResult::Consumed)
}

/// Address the client should be transferred to.
///
/// This must be reachable by the client. In order of preference:
///
/// 1. the configured address,
/// 2. the address the client itself used to reach lazymc, from its handshake,
/// 3. the public address lazymc listens on.
///
/// The handshake address is preferred over the public address, because the
/// latter is often a wildcard such as `0.0.0.0`, or an address internal to a
/// container, neither of which a client can connect to.
fn transfer_target(
    configured: Option<&str>,
    handshake: Option<(&str, u16)>,
    public: SocketAddr,
) -> (String, u16) {
    if let Some(address) = configured.map(str::trim).filter(|a| !a.is_empty()) {
        return parse_address(address);
    }

    if let Some((addr, port)) = handshake {
        // Forge and proxies append extra data to the host after a null byte, and
        // hosts resolved through SRV records may have a trailing dot
        let host = addr
            .split('\0')
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches('.');
        if !host.is_empty() && port != 0 {
            return (host.to_string(), port);
        }
    }

    if public.ip().is_unspecified() {
        warn!(
            target: "lazymc",
            "Could not determine address to transfer client to, falling back to {}, set join.transfer.address",
            public,
        );
    }
    (public.ip().to_string(), public.port())
}

/// Parse a `host`, `host:port` or `[ipv6]:port` address.
fn parse_address(address: &str) -> (String, u16) {
    // Bracketed IPv6, optionally with port
    if let Some(rest) = address.strip_prefix('[') {
        if let Some((host, port)) = rest.split_once(']') {
            let port = port
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(DEFAULT_PORT);
            return (host.to_string(), port);
        }
    }

    // Host with port, a bare IPv6 address has multiple colons and no port
    if let Some((host, port)) = address.rsplit_once(':') {
        if !host.contains(':') {
            if let Ok(port) = port.parse() {
                return (host.to_string(), port);
            }
        }
    }

    (address.to_string(), DEFAULT_PORT)
}

/// Encode a login success packet body for the given protocol version.
///
/// Layout: UUID, username, property count, and for protocol versions before
/// 1.21.2 a strict error handling flag.
fn encode_login_success(username: &str, protocol: u32) -> Result<Vec<u8>, ()> {
    let mut data = uuid::offline_player_uuid(username).as_bytes().to_vec();
    data.append(&mut encode_string(username)?);
    data.append(&mut types::encode_var_int(0)?);
    if protocol < PROTOCOL_NO_STRICT_ERROR_HANDLING {
        data.push(0x00);
    }
    Ok(data)
}

/// Encode a length prefixed UTF-8 string.
fn encode_string(value: &str) -> Result<Vec<u8>, ()> {
    let bytes = value.as_bytes();
    let mut data = types::encode_var_int(bytes.len() as i32)?;
    data.extend_from_slice(bytes);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(addr: &str) -> SocketAddr {
        addr.parse().unwrap()
    }

    #[test]
    fn target_prefers_configured_address() {
        let target = transfer_target(
            Some("mc.example.com:25570"),
            Some(("other.example.com", 25565)),
            public("0.0.0.0:25565"),
        );
        assert_eq!(target, ("mc.example.com".to_string(), 25570));
    }

    #[test]
    fn target_uses_handshake_address() {
        let target = transfer_target(
            None,
            Some(("play.example.com", 25566)),
            public("0.0.0.0:25565"),
        );
        assert_eq!(target, ("play.example.com".to_string(), 25566));
    }

    #[test]
    fn target_strips_handshake_extras() {
        let target = transfer_target(
            None,
            Some(("play.example.com.\0FML3\0", 25565)),
            public("0.0.0.0:25565"),
        );
        assert_eq!(target, ("play.example.com".to_string(), 25565));
    }

    #[test]
    fn target_falls_back_to_public_address() {
        let target = transfer_target(None, Some(("", 25565)), public("192.168.1.5:25577"));
        assert_eq!(target, ("192.168.1.5".to_string(), 25577));

        let target = transfer_target(Some("  "), None, public("192.168.1.5:25577"));
        assert_eq!(target, ("192.168.1.5".to_string(), 25577));
    }

    #[test]
    fn parses_addresses() {
        assert_eq!(parse_address("example.com"), ("example.com".into(), 25565));
        assert_eq!(
            parse_address("example.com:1234"),
            ("example.com".into(), 1234)
        );
        assert_eq!(parse_address("10.0.0.1:1234"), ("10.0.0.1".into(), 1234));
        assert_eq!(parse_address("[::1]:1234"), ("::1".into(), 1234));
        assert_eq!(parse_address("[::1]"), ("::1".into(), 25565));
        assert_eq!(parse_address("fe80::1"), ("fe80::1".into(), 25565));
    }

    #[test]
    fn login_success_layout_depends_on_protocol() {
        // 16 byte UUID, 1 byte length + 5 byte name, 1 byte property count
        let new = encode_login_success("Steve", 768).unwrap();
        assert_eq!(new.len(), 16 + 6 + 1);
        assert_eq!(&new[16..22], b"\x05Steve");

        // Trailing strict error handling flag before 1.21.2
        let old = encode_login_success("Steve", 767).unwrap();
        assert_eq!(old.len(), new.len() + 1);
        assert_eq!(old[..new.len()], new[..]);
        assert_eq!(old.last(), Some(&0x00));
    }
}
