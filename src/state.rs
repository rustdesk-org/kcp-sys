use crate::error::Error;

#[auto_impl::auto_impl(&mut)]
pub trait PacketHeaderFlagManipulator: Default {
    fn has_syn(&self) -> bool;
    fn has_ack(&self) -> bool;
    fn has_fin(&self) -> bool;
    fn has_rst(&self) -> bool;
    fn has_data(&self) -> bool;

    fn set_syn(&mut self, value: bool);
    fn set_ack(&mut self, value: bool);
    fn set_fin(&mut self, value: bool);
    fn set_rst(&mut self, value: bool);
    fn set_data(&mut self, value: bool);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KcpConnectionFSM {
    Closed,

    // server start state
    Listen,
    SynReceived,

    // client start state
    SynSent,

    // common states
    Established,

    // common active closing states
    LocalClosed,

    // common passive closing states
    PeerClosed,
}

impl KcpConnectionFSM {
    pub fn listen() -> Self {
        KcpConnectionFSM::Listen
    }

    pub fn connect<P: PacketHeaderFlagManipulator>(out_packet: &mut P) -> Self {
        out_packet.set_syn(true);
        KcpConnectionFSM::SynSent
    }

    pub fn close<P: PacketHeaderFlagManipulator>(
        &mut self,
        out_packet: &mut P,
    ) -> Result<(), Error> {
        out_packet.set_fin(true);
        if matches!(self, KcpConnectionFSM::Established) {
            *self = KcpConnectionFSM::LocalClosed;
        } else {
            *self = KcpConnectionFSM::Closed;
        }

        Ok(())
    }

    fn check_packet_flag<P: PacketHeaderFlagManipulator>(
        packet: &P,
        has_syn: bool,
        has_ack: bool,
        has_fin: bool,
        has_rst: bool,
        has_data: bool,
    ) -> bool {
        packet.has_syn() == has_syn
            && packet.has_ack() == has_ack
            && packet.has_fin() == has_fin
            && packet.has_rst() == has_rst
            && packet.has_data() == has_data
    }

    fn do_handle_packet<P: PacketHeaderFlagManipulator>(
        &mut self,
        packet: &P,
        out_packet: &mut Option<P>,
    ) -> Result<(), Error> {
        let mut p = P::default();
        match self {
            KcpConnectionFSM::Closed => Err(Error::InvalidStateNeedRst),
            KcpConnectionFSM::Listen => {
                if Self::check_packet_flag(packet, true, false, false, false, false) {
                    p.set_syn(true);
                    p.set_ack(true);
                    out_packet.replace(p);
                    *self = KcpConnectionFSM::SynReceived;
                    Ok(())
                } else {
                    Err(Error::InvalidStateNeedRst)
                }
            }
            KcpConnectionFSM::SynReceived => {
                // when client receives the syn-ack packet, all following packets should have ack+data flag
                if Self::check_packet_flag(packet, false, true, false, false, true) {
                    *self = KcpConnectionFSM::Established;
                    Ok(())
                } else if packet.has_rst() {
                    *self = KcpConnectionFSM::Closed;
                    Err(Error::InvalidState)
                } else if packet.has_fin() {
                    *self = KcpConnectionFSM::Closed;
                    Err(Error::InvalidStateNeedRst)
                } else {
                    Err(Error::InvalidStateNeedRst)
                }
            }
            KcpConnectionFSM::SynSent => {
                if Self::check_packet_flag(packet, true, true, false, false, false) {
                    p.set_ack(true);
                    p.set_data(true);
                    out_packet.replace(p);
                    *self = KcpConnectionFSM::Established;
                    Ok(())
                } else if packet.has_rst() {
                    *self = KcpConnectionFSM::Closed;
                    Err(Error::InvalidState)
                } else if packet.has_fin() {
                    *self = KcpConnectionFSM::Closed;
                    Err(Error::InvalidStateNeedRst)
                } else {
                    Err(Error::InvalidStateNeedRst)
                }
            }
            KcpConnectionFSM::Established => {
                if Self::check_packet_flag(packet, false, true, false, false, true) {
                    Ok(())
                } else if packet.has_rst() {
                    *self = KcpConnectionFSM::Closed;
                    Err(Error::InvalidState)
                } else if packet.has_fin() {
                    *self = KcpConnectionFSM::PeerClosed;
                    Ok(())
                } else {
                    Err(Error::InvalidStateNeedRst)
                }
            }
            KcpConnectionFSM::LocalClosed => {
                if packet.has_fin() {
                    *self = KcpConnectionFSM::Closed;
                    Ok(())
                } else if packet.has_rst() {
                    *self = KcpConnectionFSM::Closed;
                    Err(Error::InvalidState)
                } else if packet.has_data() {
                    Ok(())
                } else {
                    Err(Error::InvalidState)
                }
            }
            KcpConnectionFSM::PeerClosed => {
                if packet.has_rst() {
                    *self = KcpConnectionFSM::Closed;
                    Err(Error::InvalidState)
                } else {
                    Err(Error::InvalidState)
                }
            }
        }
    }

    pub fn handle_packet<P: PacketHeaderFlagManipulator>(
        &mut self,
        packet: &P,
        out_packet: &mut Option<P>,
    ) -> Result<(), Error> {
        let ret = self.do_handle_packet(packet, out_packet);
        if matches!(ret, Err(Error::InvalidStateNeedRst)) {
            let mut p = P::default();
            p.set_rst(true);
            out_packet.replace(p);
        }
        ret
    }
}
