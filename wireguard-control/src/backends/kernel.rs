use crate::{
    device::AllowedIp, Backend, Device, DeviceUpdate, InterfaceName, Key, PeerConfig,
    PeerConfigBuilder, PeerInfo, PeerStats,
};
use netlink_packet_core::{
    NetlinkMessage, NetlinkPayload, NLM_F_ACK, NLM_F_CREATE, NLM_F_EXCL, NLM_F_REQUEST,
};
use netlink_packet_generic::GenlMessage;
use netlink_packet_route::{
    constants::*,
    link::{
        self,
        nlas::{Info, InfoKind},
    },
    traits::Emitable,
    LinkMessage, RtnlMessage,
};
use netlink_packet_wireguard::{
    self,
    constants::{WGDEVICE_F_REPLACE_PEERS, WGPEER_F_REMOVE_ME, WGPEER_F_REPLACE_ALLOWEDIPS},
    nlas::{WgAllowedIpAttrs, WgDeviceAttrs, WgPeerAttrs},
    Wireguard, WireguardCmd,
};
use netlink_request::{netlink_request_genl, netlink_request_rtnl, MAX_GENL_PAYLOAD_LENGTH};

use std::{convert::TryFrom, io};

macro_rules! get_nla_value {
    ($nlas:expr, $e:ident, $v:ident) => {
        $nlas.iter().find_map(|attr| match attr {
            $e::$v(value) => Some(value),
            _ => None,
        })
    };
}

impl<'a> TryFrom<Vec<WgAllowedIpAttrs>> for AllowedIp {
    type Error = io::Error;

    fn try_from(attrs: Vec<WgAllowedIpAttrs>) -> Result<Self, Self::Error> {
        let address = *get_nla_value!(attrs, WgAllowedIpAttrs, IpAddr)
            .ok_or_else(|| io::ErrorKind::NotFound)?;
        let cidr = *get_nla_value!(attrs, WgAllowedIpAttrs, Cidr)
            .ok_or_else(|| io::ErrorKind::NotFound)?;
        Ok(AllowedIp { address, cidr })
    }
}

impl AllowedIp {
    fn to_attrs(&self) -> Vec<WgAllowedIpAttrs> {
        vec![
            WgAllowedIpAttrs::Family(if self.address.is_ipv4() {
                AF_INET
            } else {
                AF_INET6
            }),
            WgAllowedIpAttrs::IpAddr(self.address),
            WgAllowedIpAttrs::Cidr(self.cidr),
        ]
    }
}

impl PeerConfigBuilder {
    fn to_attrs(&self) -> Vec<WgPeerAttrs> {
        let mut attrs = vec![WgPeerAttrs::PublicKey(self.public_key.0)];
        let mut flags = 0u32;
        if let Some(endpoint) = self.endpoint {
            attrs.push(WgPeerAttrs::Endpoint(endpoint));
        }
        if let Some(ref key) = self.preshared_key {
            attrs.push(WgPeerAttrs::PresharedKey(key.0));
        }
        if let Some(i) = self.persistent_keepalive_interval {
            attrs.push(WgPeerAttrs::PersistentKeepalive(i));
        }
        let allowed_ips: Vec<_> = self.allowed_ips.iter().map(AllowedIp::to_attrs).collect();
        attrs.push(WgPeerAttrs::AllowedIps(allowed_ips));
        if self.remove_me {
            flags |= WGPEER_F_REMOVE_ME;
        }
        if self.replace_allowed_ips {
            flags |= WGPEER_F_REPLACE_ALLOWEDIPS;
        }
        if flags != 0 {
            attrs.push(WgPeerAttrs::Flags(flags));
        }
        attrs
    }
}

impl<'a> TryFrom<Vec<WgPeerAttrs>> for PeerInfo {
    type Error = io::Error;

    fn try_from(attrs: Vec<WgPeerAttrs>) -> Result<Self, Self::Error> {
        let public_key = get_nla_value!(attrs, WgPeerAttrs, PublicKey)
            .map(|key| Key(*key))
            .ok_or(io::ErrorKind::NotFound)?;
        let preshared_key = get_nla_value!(attrs, WgPeerAttrs, PresharedKey).map(|key| Key(*key));
        let endpoint = get_nla_value!(attrs, WgPeerAttrs, Endpoint).cloned();
        let persistent_keepalive_interval =
            get_nla_value!(attrs, WgPeerAttrs, PersistentKeepalive).cloned();
        let allowed_ips = get_nla_value!(attrs, WgPeerAttrs, AllowedIps)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(AllowedIp::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let last_handshake_time = get_nla_value!(attrs, WgPeerAttrs, LastHandshake).cloned();
        let rx_bytes = get_nla_value!(attrs, WgPeerAttrs, RxBytes)
            .cloned()
            .unwrap_or_default();
        let tx_bytes = get_nla_value!(attrs, WgPeerAttrs, TxBytes)
            .cloned()
            .unwrap_or_default();
        Ok(PeerInfo {
            config: PeerConfig {
                public_key,
                preshared_key,
                endpoint,
                persistent_keepalive_interval,
                allowed_ips,
                __cant_construct_me: (),
            },
            stats: PeerStats {
                last_handshake_time,
                rx_bytes,
                tx_bytes,
            },
        })
    }
}

impl<'a> TryFrom<&'a Wireguard> for Device {
    type Error = io::Error;

    fn try_from(wg: &'a Wireguard) -> Result<Self, Self::Error> {
        let name = get_nla_value!(wg.nlas, WgDeviceAttrs, IfName)
            .ok_or_else(|| io::ErrorKind::NotFound)?
            .parse()?;
        let public_key = get_nla_value!(wg.nlas, WgDeviceAttrs, PublicKey).map(|key| Key(*key));
        let private_key = get_nla_value!(wg.nlas, WgDeviceAttrs, PrivateKey).map(|key| Key(*key));
        let listen_port = get_nla_value!(wg.nlas, WgDeviceAttrs, ListenPort).cloned();
        let fwmark = get_nla_value!(wg.nlas, WgDeviceAttrs, Fwmark).cloned();
        let peers = get_nla_value!(wg.nlas, WgDeviceAttrs, Peers)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(PeerInfo::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Device {
            name,
            public_key,
            private_key,
            listen_port,
            fwmark,
            peers,
            linked_name: None,
            backend: Backend::Kernel,
            __cant_construct_me: (),
        })
    }
}

pub fn enumerate() -> Result<Vec<InterfaceName>, io::Error> {
    let link_responses = netlink_request_rtnl(
        RtnlMessage::GetLink(LinkMessage::default()),
        Some(NLM_F_DUMP | NLM_F_REQUEST),
    )?;
    let links = link_responses
        .into_iter()
        // Filter out non-link messages
        .filter_map(|response| match response {
            NetlinkMessage {
                payload: NetlinkPayload::InnerMessage(RtnlMessage::NewLink(link)),
                ..
            } => Some(link),
            _ => None,
        })
        .filter(|link| {
            for nla in link.nlas.iter() {
                if let link::nlas::Nla::Info(infos) = nla {
                    return infos.iter().any(|info| info == &Info::Kind(InfoKind::Wireguard))
                }
            }
            false
        })
        .filter_map(|link| link.nlas.iter().find_map(|nla| match nla {
            link::nlas::Nla::IfName(name) => Some(name.clone()),
            _ => None,
        }))
        .filter_map(|name| name.parse().ok())
        .collect::<Vec<_>>();

    Ok(links)
}

fn add_del(iface: &InterfaceName, add: bool) -> io::Result<()> {
    let mut message = LinkMessage::default();
    message
        .nlas
        .push(link::nlas::Nla::IfName(iface.as_str_lossy().to_string()));
    message.nlas.push(link::nlas::Nla::Info(vec![Info::Kind(
        link::nlas::InfoKind::Wireguard,
    )]));
    let extra_flags = if add { NLM_F_CREATE | NLM_F_EXCL } else { 0 };
    let rtnl_message = if add {
        RtnlMessage::NewLink(message)
    } else {
        RtnlMessage::DelLink(message)
    };
    let result = netlink_request_rtnl(rtnl_message, Some(NLM_F_REQUEST | NLM_F_ACK | extra_flags));
    match result {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

pub fn apply(builder: &DeviceUpdate, iface: &InterfaceName) -> io::Result<()> {
    add_del(iface, true)?;
    let mut payload = ApplyPayload::new(iface);
    if let Some(Key(k)) = builder.private_key {
        payload.push(WgDeviceAttrs::PrivateKey(k));
    }
    if let Some(f) = builder.fwmark {
        payload.push(WgDeviceAttrs::Fwmark(f));
    }
    if let Some(f) = builder.listen_port {
        payload.push(WgDeviceAttrs::ListenPort(f));
    }
    if builder.replace_peers {
        payload.push(WgDeviceAttrs::Flags(WGDEVICE_F_REPLACE_PEERS));
    }

    builder
        .peers
        .iter()
        .for_each(|peer| payload.push_peer(peer.to_attrs()));

    for message in payload.finish() {
        netlink_request_genl(message, Some(NLM_F_REQUEST | NLM_F_ACK))?;
    }
    Ok(())
}

struct ApplyPayload {
    iface: String,
    nlas: Vec<WgDeviceAttrs>,
    current_buffer_len: usize,
    messages: Vec<GenlMessage<Wireguard>>,
}

impl ApplyPayload {
    fn new(iface: &InterfaceName) -> Self {
        Self {
            iface: iface.as_str_lossy().to_string(),
            nlas: vec![],
            messages: vec![],
            current_buffer_len: 0,
        }
    }

    fn flush_nlas(&mut self) {
        // cleanup: clear out any empty peer lists.
        self.nlas
            .retain(|nla| !matches!(nla, WgDeviceAttrs::Peers(peers) if peers.len() == 0));

        let name = WgDeviceAttrs::IfName(self.iface.clone());
        self.current_buffer_len = name.buffer_len();

        if !self.nlas.is_empty() {
            self.messages.push(GenlMessage::from_payload(Wireguard {
                cmd: WireguardCmd::SetDevice,
                nlas: std::mem::replace(&mut self.nlas, vec![name]),
            }));
        }
    }

    /// Push a device attribute which will be optimally packed into 1 or more netlink messages
    pub fn push(&mut self, nla: WgDeviceAttrs) {
        let nla_buffer_len = nla.buffer_len();
        if (self.current_buffer_len + nla_buffer_len) > MAX_GENL_PAYLOAD_LENGTH {
            self.flush_nlas();
        }
        self.nlas.push(nla);
        self.current_buffer_len += nla_buffer_len;
    }

    /// A helper function to assist in breaking up large peer lists across multiple netlink messages
    pub fn push_peer(&mut self, peer: Vec<WgPeerAttrs>) {
        const EMPTY_PEERS: WgDeviceAttrs = WgDeviceAttrs::Peers(vec![]);
        let mut needs_peer_nla = !self
            .nlas
            .iter()
            .any(|nla| matches!(nla, WgDeviceAttrs::Peers(_)));
        let peer_buffer_len = peer.as_slice().buffer_len() + 4;
        let additional_buffer_len = peer_buffer_len
            + if needs_peer_nla {
                EMPTY_PEERS.buffer_len()
            } else {
                0
            };
        if (self.current_buffer_len + additional_buffer_len) > MAX_GENL_PAYLOAD_LENGTH {
            self.flush_nlas();
            needs_peer_nla = true;
        }

        if needs_peer_nla {
            self.push(EMPTY_PEERS);
        }
        let peers_nla = self
            .nlas
            .iter_mut()
            .find_map(|nla| match nla {
                WgDeviceAttrs::Peers(peers) => Some(peers),
                _ => None,
            })
            .expect("WgDeviceAttrs::Peers missing from NLAs when it should exist.");

        peers_nla.push(peer);
        self.current_buffer_len += peer_buffer_len;
    }

    pub fn finish(mut self) -> Vec<GenlMessage<Wireguard>> {
        self.flush_nlas();
        self.messages
    }
}

pub fn get_by_name(name: &InterfaceName) -> Result<Device, io::Error> {
    let genlmsg: GenlMessage<Wireguard> = GenlMessage::from_payload(Wireguard {
        cmd: WireguardCmd::GetDevice,
        nlas: vec![WgDeviceAttrs::IfName(name.as_str_lossy().to_string())],
    });
    let responses = netlink_request_genl(genlmsg, Some(NLM_F_REQUEST | NLM_F_DUMP | NLM_F_ACK))?;

    match responses.get(0) {
        Some(NetlinkMessage {
            payload: NetlinkPayload::InnerMessage(message),
            ..
        }) => Device::try_from(&message.payload),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unexpected netlink payload",
        )),
    }
}

pub fn delete_interface(iface: &InterfaceName) -> io::Result<()> {
    add_del(iface, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use netlink_request::MAX_NETLINK_BUFFER_LENGTH;
    use std::str::FromStr;

    #[test]
    fn test_simple_payload() {
        let mut payload = ApplyPayload::new(&InterfaceName::from_str("wg0").unwrap());
        payload.push(WgDeviceAttrs::PrivateKey([1u8; 32]));
        payload.push(WgDeviceAttrs::Fwmark(111));
        payload.push(WgDeviceAttrs::ListenPort(12345));
        payload.push(WgDeviceAttrs::Flags(WGDEVICE_F_REPLACE_PEERS));
        payload.push_peer(vec![
            WgPeerAttrs::PublicKey([2u8; 32]),
            WgPeerAttrs::PersistentKeepalive(25),
            WgPeerAttrs::Endpoint("1.1.1.1:51820".parse().unwrap()),
            WgPeerAttrs::AllowedIps(vec![vec![
                WgAllowedIpAttrs::Family(AF_INET),
                WgAllowedIpAttrs::IpAddr([10, 1, 1, 1].into()),
                WgAllowedIpAttrs::Cidr(24),
            ]]),
        ]);
        assert_eq!(payload.finish().len(), 1);
    }

    #[test]
    fn test_massive_payload() {
        let mut payload = ApplyPayload::new(&InterfaceName::from_str("wg0").unwrap());
        payload.push(WgDeviceAttrs::PrivateKey([1u8; 32]));
        payload.push(WgDeviceAttrs::Fwmark(111));
        payload.push(WgDeviceAttrs::ListenPort(12345));
        payload.push(WgDeviceAttrs::Flags(WGDEVICE_F_REPLACE_PEERS));
        for _ in 0..10_000 {
            payload.push_peer(vec![
                WgPeerAttrs::PublicKey([2u8; 32]),
                WgPeerAttrs::PersistentKeepalive(25),
                WgPeerAttrs::Endpoint("1.1.1.1:51820".parse().unwrap()),
                WgPeerAttrs::AllowedIps(vec![vec![
                    WgAllowedIpAttrs::Family(AF_INET),
                    WgAllowedIpAttrs::IpAddr([10, 1, 1, 1].into()),
                    WgAllowedIpAttrs::Cidr(24),
                ]]),
            ]);
        }

        let messages = payload.finish();
        assert!(messages.len() > 1);
        for message in messages {
            assert!(message.buffer_len() < MAX_NETLINK_BUFFER_LENGTH);
        }
    }
}
