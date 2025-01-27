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
    fn from(inner: BytesMut) -> Self {
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

impl Into<BytesMut> for KcpPacket {
    fn into(self) -> BytesMut {
        self.inner
    }
}

impl Into<Bytes> for KcpPacket {
    fn into(self) -> Bytes {
        self.inner.freeze()
    }
}

impl KcpPacket {
    pub fn new(body_size: usize) -> Self {
        let mut inner = BytesMut::with_capacity(std::mem::size_of::<KcpPacketHeader>() + body_size);
        inner.resize(inner.capacity(), 0);
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
}
