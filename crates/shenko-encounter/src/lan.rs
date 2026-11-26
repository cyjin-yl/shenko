//! LAN multicast encounters: each subject periodically sends its signed card
//! to a link-local multicast group and verifies the cards it receives.

use crate::card::{Card, CardError, MAX_DATAGRAM_BYTES, SignedCard};
use crate::key::PublicIdentity;
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

/// Administratively scoped (RFC 2365) group; TTL 1 keeps beacons on the link.
pub const LAN_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 77);
pub const LAN_PORT: u16 = 47077;
pub const LAN_TRANSPORT: &str = "lan.multicast";

/// A verified encounter: who (by key), what they published, and how we met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encounter {
    pub transport: &'static str,
    pub peer: PublicIdentity,
    pub card: Card,
    /// SHA-256 of the exact signed card text.
    pub card_digest: String,
    pub source: SocketAddr,
    pub observed_at: i64,
}

/// What one received datagram turned out to be.
#[derive(Debug)]
pub enum Received {
    Encounter(Encounter),
    /// Our own beacon looped back.
    Own,
    /// A datagram on the group that is not a valid fresh card.
    Rejected {
        source: SocketAddr,
        error: CardError,
    },
}

pub struct LanBeacon {
    socket: UdpSocket,
    group: SocketAddrV4,
    own: PublicIdentity,
}

impl LanBeacon {
    /// Join the encounter group on `port`. Several processes on one host may
    /// join the same port (SO_REUSEADDR/SO_REUSEPORT), as two subjects on one
    /// machine legitimately can.
    pub fn bind(own: PublicIdentity, port: u16) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
        socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
        socket.join_multicast_v4(&LAN_GROUP, &Ipv4Addr::UNSPECIFIED)?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_multicast_ttl_v4(1)?;
        Ok(Self {
            socket: socket.into(),
            group: SocketAddrV4::new(LAN_GROUP, port),
            own,
        })
    }

    pub fn announce(&self, card: &SignedCard) -> io::Result<()> {
        let bytes = card
            .to_bytes()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if bytes.len() > MAX_DATAGRAM_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "signed card exceeds one datagram",
            ));
        }
        self.socket.send_to(&bytes, self.group)?;
        Ok(())
    }

    /// Wait up to `timeout` for one datagram. `Ok(None)` means nothing arrived.
    pub fn receive(&self, timeout: Duration, now: i64) -> io::Result<Option<Received>> {
        self.socket
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        let mut buffer = [0u8; MAX_DATAGRAM_BYTES + 1];
        let (length, source) = match self.socket.recv_from(&mut buffer) {
            Ok(received) => received,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        Ok(Some(match SignedCard::verify(&buffer[..length], now) {
            Ok((card, _)) if card.identity == self.own => Received::Own,
            Ok((card, card_digest)) => Received::Encounter(Encounter {
                transport: LAN_TRANSPORT,
                peer: card.identity.clone(),
                card,
                card_digest,
                source,
                observed_at: now,
            }),
            Err(error) => Received::Rejected { source, error },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::SubjectKey;

    fn key(name: &str) -> SubjectKey {
        let dir = std::env::temp_dir().join(format!("shenko-lan-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        SubjectKey::load_or_create(&dir.join("k.pk8")).unwrap()
    }

    fn card(key: &SubjectKey, name: &str, now: i64) -> SignedCard {
        Card {
            v: 1,
            identity: key.identity().clone(),
            display_name: name.into(),
            capabilities: vec![],
            endpoint: None,
            issued_at: now,
            nonce: name.into(),
        }
        .sign(key)
        .unwrap()
    }

    /// Drain until `want` or timeout, so stray datagrams do not fail the test.
    fn next_encounter(beacon: &LanBeacon, now: i64) -> Option<Encounter> {
        for _ in 0..20 {
            match beacon.receive(Duration::from_millis(200), now).unwrap() {
                Some(Received::Encounter(encounter)) => return Some(encounter),
                Some(_) => continue,
                None => return None,
            }
        }
        None
    }

    #[test]
    fn two_subjects_on_one_link_meet_and_ignore_their_own_beacon() {
        let port = 47077 + (std::process::id() % 1000) as u16;
        let now = 1_800_000_000;
        let (alice, bob) = (key("alice"), key("bob"));
        let a = LanBeacon::bind(alice.identity().clone(), port).unwrap();
        let b = LanBeacon::bind(bob.identity().clone(), port).unwrap();

        a.announce(&card(&alice, "alice", now)).unwrap();
        let met = next_encounter(&b, now).expect("bob receives alice's card");
        assert_eq!(met.peer, *alice.identity());
        assert_eq!(met.card.display_name, "alice");
        assert_eq!(met.transport, LAN_TRANSPORT);

        // Alice hears her own looped-back beacon but does not "meet" herself.
        assert!(next_encounter(&a, now).is_none());

        b.announce(&card(&bob, "bob", now)).unwrap();
        assert_eq!(
            next_encounter(&a, now).expect("alice receives bob").peer,
            *bob.identity()
        );

        // Unsigned noise on the group is reported, never turned into an encounter.
        a.socket
            .send_to(b"{\"card\":\"x\",\"signature\":\"00\"}", a.group)
            .unwrap();
        let mut rejected = false;
        while let Some(received) = b.receive(Duration::from_millis(200), now).unwrap() {
            if let Received::Rejected { .. } = received {
                rejected = true;
                break;
            }
        }
        assert!(rejected);
    }
}
