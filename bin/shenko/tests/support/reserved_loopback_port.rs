//! Keep an IPv4 loopback port owned until the caller launches its child.
use std::{
    io,
    net::{Ipv4Addr, TcpListener},
};

pub struct ReservedLoopbackPort {
    listener: TcpListener,
    port: u16,
}

impl ReservedLoopbackPort {
    pub fn reserve() -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Release immediately before launching the child that will bind this
    /// port. Other reservations in the batch remain owned. The child's bind
    /// still detects any unrelated process racing this handover.
    pub fn release(self) -> u16 {
        drop(self.listener);
        self.port
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn ports_stay_distinct_and_owned_until_individual_release() -> io::Result<()> {
        let first = ReservedLoopbackPort::reserve()?;
        let second = ReservedLoopbackPort::reserve()?;
        let third = ReservedLoopbackPort::reserve()?;
        let ports = [first.port(), second.port(), third.port()];
        assert_eq!(ports.into_iter().collect::<BTreeSet<_>>().len(), 3);
        for port in ports {
            assert!(
                matches!(
                    TcpListener::bind((Ipv4Addr::LOCALHOST, port)),
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse
                ),
                "reserved port {port} was not held",
            );
        }

        let receiver = TcpListener::bind((Ipv4Addr::LOCALHOST, first.release()))?;
        assert_eq!(receiver.local_addr()?.port(), ports[0]);
        for port in [second.port(), third.port()] {
            assert!(
                matches!(
                    TcpListener::bind((Ipv4Addr::LOCALHOST, port)),
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse
                ),
                "releasing one port also released sibling {port}",
            );
        }
        Ok(())
    }
}
