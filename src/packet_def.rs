use std::fmt::Formatter;
use zerocopy::{AsBytes, FromBytes, FromZeroes, LittleEndian, U32};

pub type BytesMut = bytes::BytesMut;
pub type Bytes = bytes::Bytes;

bitflags::bitflags! {
    #[derive(Debug)]
    struct KcpPacketHeaderFlags: u8 {
        const SYN = 0b0000_0001;
        const ACK = 0b0000_0010;
        const FIN = 0b0000_0100;
        const DATA = 0b0000_1000;
        const RST = 0b0001_0000;

        const PING = 0b0010_0000;
        const PONG = 0b0100_0000;

        const _ = !0;
    }
}

#[repr(C, packed)]
#[derive(AsBytes, FromBytes, FromZeroes, Clone, Default)]
pub struct KcpPacketHeader {
    conv: U32<LittleEndian>,
    src_session_id: U32<LittleEndian>,
    dst_session_id: U32<LittleEndian>,
    flag: u8,
    rsv: u8,
}

impl KcpPacketHeader {
    pub fn conv(&self) -> u32 {
        self.conv.into()
    }

    pub fn src_session_id(&self) -> u32 {
        self.src_session_id.into()
    }

    pub fn dst_session_id(&self) -> u32 {
        self.dst_session_id.into()
    }

    pub fn is_syn(&self) -> bool {
        KcpPacketHeaderFlags::from_bits(self.flag)
            .unwrap()
            .contains(KcpPacketHeaderFlags::SYN)
    }

    pub fn is_ack(&self) -> bool {
        KcpPacketHeaderFlags::from_bits(self.flag)
            .unwrap()
            .contains(KcpPacketHeaderFlags::ACK)
    }

    pub fn is_fin(&self) -> bool {
        KcpPacketHeaderFlags::from_bits(self.flag)
            .unwrap()
            .contains(KcpPacketHeaderFlags::FIN)
    }

    pub fn is_data(&self) -> bool {
        KcpPacketHeaderFlags::from_bits(self.flag)
            .unwrap()
            .contains(KcpPacketHeaderFlags::DATA)
    }

    pub fn is_rst(&self) -> bool {
        KcpPacketHeaderFlags::from_bits(self.flag)
            .unwrap()
            .contains(KcpPacketHeaderFlags::RST)
    }

    pub fn is_ping(&self) -> bool {
        KcpPacketHeaderFlags::from_bits(self.flag)
            .unwrap()
            .contains(KcpPacketHeaderFlags::PING)
    }

    pub fn is_pong(&self) -> bool {
        KcpPacketHeaderFlags::from_bits(self.flag)
            .unwrap()
            .contains(KcpPacketHeaderFlags::PONG)
    }

    pub fn set_conv(&mut self, conv: u32) -> &mut Self {
        self.conv = conv.into();
        self
    }

    pub fn set_src_session_id(&mut self, session_id: u32) -> &mut Self {
        self.src_session_id = session_id.into();
        self
    }

    pub fn set_dst_session_id(&mut self, session_id: u32) -> &mut Self {
        self.dst_session_id = session_id.into();
        self
    }

    pub fn set_syn(&mut self, syn: bool) -> &mut Self {
        let mut flags = KcpPacketHeaderFlags::from_bits(self.flag).unwrap();
        if syn {
            flags.insert(KcpPacketHeaderFlags::SYN);
        } else {
            flags.remove(KcpPacketHeaderFlags::SYN);
        }
        self.flag = flags.bits();
        self
    }

    pub fn set_ack(&mut self, ack: bool) -> &mut Self {
        let mut flags = KcpPacketHeaderFlags::from_bits(self.flag).unwrap();
        if ack {
            flags.insert(KcpPacketHeaderFlags::ACK);
        } else {
            flags.remove(KcpPacketHeaderFlags::ACK);
        }
        self.flag = flags.bits();
        self
    }

    pub fn set_fin(&mut self, fin: bool) -> &mut Self {
        let mut flags = KcpPacketHeaderFlags::from_bits(self.flag).unwrap();
        if fin {
            flags.insert(KcpPacketHeaderFlags::FIN);
        } else {
            flags.remove(KcpPacketHeaderFlags::FIN);
        }
        self.flag = flags.bits();
        self
    }

    pub fn set_data(&mut self, data: bool) -> &mut Self {
        let mut flags = KcpPacketHeaderFlags::from_bits(self.flag).unwrap();
        if data {
            flags.insert(KcpPacketHeaderFlags::DATA);
        } else {
            flags.remove(KcpPacketHeaderFlags::DATA);
        }
        self.flag = flags.bits();
        self
    }

    pub fn set_rst(&mut self, rst: bool) -> &mut Self {
        let mut flags = KcpPacketHeaderFlags::from_bits(self.flag).unwrap();
        if rst {
            flags.insert(KcpPacketHeaderFlags::RST);
        } else {
            flags.remove(KcpPacketHeaderFlags::RST);
        }
        self.flag = flags.bits();
        self
    }

    pub fn set_ping(&mut self, ping: bool) -> &mut Self {
        let mut flags = KcpPacketHeaderFlags::from_bits(self.flag).unwrap();
        if ping {
            flags.insert(KcpPacketHeaderFlags::PING);
        } else {
            flags.remove(KcpPacketHeaderFlags::PING);
        }
        self.flag = flags.bits();
        self
    }

    pub fn set_pong(&mut self, pong: bool) -> &mut Self {
        let mut flags = KcpPacketHeaderFlags::from_bits(self.flag).unwrap();
        if pong {
            flags.insert(KcpPacketHeaderFlags::PONG);
        } else {
            flags.remove(KcpPacketHeaderFlags::PONG);
        }
        self.flag = flags.bits();
        self
    }
}

impl std::fmt::Debug for KcpPacketHeader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KcpPacketHeader")
            .field("conv", &self.conv())
            .field("src_session_id", &self.src_session_id())
            .field("dst_session_id", &self.dst_session_id())
            .field("flag", &KcpPacketHeaderFlags::from_bits(self.flag).unwrap())
            .finish()
    }
}

#[derive(Clone)]
pub struct KcpPacket {
    inner: BytesMut,
}

impl Default for KcpPacket {
    fn default() -> Self {
        Self::new(0)
    }
}

impl From<BytesMut> for KcpPacket {
    fn from(mut inner: BytesMut) -> Self {
        // Never construct a packet shorter than its header: header()/mut_header()/payload()
        // parse the fixed-size prefix and would panic on a truncated datagram.
        let header_len = std::mem::size_of::<KcpPacketHeader>();
        if inner.len() < header_len {
            inner.resize(header_len, 0);
        }
        Self { inner }
    }
}

impl std::fmt::Debug for KcpPacket {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KcpPacket")
            .field("header", &self.header())
            .field("payload", &self.payload())
            .finish()
    }
}

impl From<KcpPacket> for BytesMut {
    fn from(val: KcpPacket) -> Self {
        val.inner
    }
}

impl From<KcpPacket> for Bytes {
    fn from(val: KcpPacket) -> Self {
        val.inner.freeze()
    }
}

impl KcpPacket {
    pub fn new(body_size: usize) -> Self {
        // Size to the request, not to capacity(): the allocator may hand back more
        // than was asked for, and resizing to that made every control packet
        // (ping, SYN-ACK, RST, FIN - all built with body_size 0) trail whatever
        // slack the allocation happened to carry, as zero payload on the wire.
        let len = std::mem::size_of::<KcpPacketHeader>() + body_size;
        let mut inner = BytesMut::with_capacity(len);
        inner.resize(len, 0);
        Self { inner }
    }

    pub fn new_with_payload(payload: &[u8]) -> Self {
        let mut inner =
            BytesMut::with_capacity(std::mem::size_of::<KcpPacketHeader>() + payload.len());
        inner.resize(std::mem::size_of::<KcpPacketHeader>(), 0);
        inner.extend_from_slice(payload);
        Self { inner }
    }

    pub fn mut_header(&mut self) -> &mut KcpPacketHeader {
        KcpPacketHeader::mut_from_prefix(&mut self.inner).unwrap()
    }

    pub fn header(&self) -> &KcpPacketHeader {
        KcpPacketHeader::ref_from_prefix(&self.inner).unwrap()
    }

    pub fn payload(&self) -> &[u8] {
        &self.inner[std::mem::size_of::<KcpPacketHeader>()..]
    }

    pub fn inner(self) -> BytesMut {
        self.inner
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER_LEN: usize = std::mem::size_of::<KcpPacketHeader>();

    // Every accessor parses the fixed-size prefix and slices past it, so a packet
    // shorter than the header would panic - and rustdesk builds with panic='abort',
    // making that a process kill rather than a task failure. Nothing may construct
    // one. This pins the invariant at all four construction sites, including the
    // From<BytesMut> that turns arbitrary network bytes into a packet.
    fn assert_accessors_are_safe(mut p: KcpPacket) {
        assert!(p.len() >= HEADER_LEN, "packet shorter than its header");
        let _ = p.header().conv();
        let _ = p.mut_header().set_syn(true);
        let _ = p.payload();
        // Debug walks the same unwrap paths (header(), from_bits) and runs on
        // trace-level packet logging, so it is part of the no-abort surface.
        let _ = format!("{p:?}");
    }

    #[test]
    fn construction_never_yields_a_packet_shorter_than_the_header() {
        assert_accessors_are_safe(KcpPacket::new(0));
        assert_accessors_are_safe(KcpPacket::default());
        assert_accessors_are_safe(KcpPacket::new_with_payload(&[]));
        for short in 0..HEADER_LEN {
            assert_accessors_are_safe(KcpPacket::from(BytesMut::from(&vec![0xABu8; short][..])));
        }
    }

    #[test]
    fn new_sizes_to_the_request() {
        // new() sizes to the request rather than to capacity(), which the allocator
        // is free to round up. Note this test cannot prove that on its own: every
        // BytesMut::with_capacity(n) measured here returns capacity exactly n, so
        // both spellings agree today. The point is not to depend on that.
        for body in [0usize, 1, 7, 64, 1200] {
            let p = KcpPacket::new(body);
            assert_eq!(p.len(), HEADER_LEN + body, "body_size {body}");
            assert_eq!(p.payload().len(), body, "body_size {body}");
        }
        assert!(KcpPacket::new(0).payload().is_empty());
    }

    #[test]
    fn from_bytes_preserves_payload_and_pads_only_short_input() {
        let payload = b"payload";
        let mut bytes = BytesMut::from(&[0u8; HEADER_LEN][..]);
        bytes.extend_from_slice(payload);
        let p = KcpPacket::from(bytes);
        assert_eq!(
            p.payload(),
            payload,
            "a full-length buffer must pass through"
        );

        // Short input is padded to exactly the header, never further.
        let p = KcpPacket::from(BytesMut::from(&[0xABu8; 3][..]));
        assert_eq!(p.len(), HEADER_LEN);
        assert!(p.payload().is_empty());
    }

    #[test]
    fn every_flag_byte_parses() {
        // header() unwraps from_bits(); the bitflags `_ = !0` arm is what makes
        // that infallible for attacker-chosen bytes. Drop the arm and this fails.
        // flag is the second-to-last header field (only rsv: u8 follows), so its
        // offset tracks the layout instead of hardcoding 12: a field inserted
        // before it would silently retarget a literal offset at some other byte
        // and turn this test into a no-op.
        let flag_offset = HEADER_LEN - 2;
        for flag in 0u8..=255 {
            let mut bytes = BytesMut::from(&[0u8; HEADER_LEN][..]);
            bytes[flag_offset] = flag;
            let p = KcpPacket::from(bytes);
            let h = p.header();
            // Proves the byte written above really is the flag field: if the
            // offset ever misses it, SYN reads back wrong for half the values.
            assert_eq!(h.is_syn(), flag & 0b1 != 0);
            let _ = (h.is_ack(), h.is_fin(), h.is_data(), h.is_rst());
            let _ = (h.is_ping(), h.is_pong());
        }
    }
}
