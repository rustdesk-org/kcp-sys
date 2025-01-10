use bytes::{Bytes, BytesMut};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

bitflags::bitflags! {
    struct KcpPacketHeaderFlags: u8 {
        const SYN = 0b0000_0001;
        const ACK = 0b0000_0010;
        const FIN = 0b0000_0100;
        const DATA = 0b0000_1000;

        const _ = !0;
    }
}

#[repr(C, packed)]
#[derive(AsBytes, FromBytes, FromZeroes, Clone, Debug, Default)]
pub struct KcpPacketHeader {
    conv: u32,
    len: u16,
    flag: u8,
    rsv: u8,
}

impl KcpPacketHeader {
    pub fn conv(&self) -> u32 {
        self.conv
    }

    pub fn len(&self) -> u16 {
        self.len
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

    pub fn set_conv(&mut self, conv: u32) -> &mut Self {
        self.conv = conv;
        self
    }

    pub fn set_len(&mut self, len: u16) -> &mut Self {
        self.len = len;
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
}

#[derive(Debug)]
pub struct KcpPacket {
    inner: BytesMut,
}

impl From<BytesMut> for KcpPacket {
    fn from(inner: BytesMut) -> Self {
        Self { inner }
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
    pub fn new(cap: Option<usize>) -> Self {
        Self {
            inner: BytesMut::with_capacity(cap.unwrap_or(std::mem::size_of::<KcpPacketHeader>())),
        }
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
}
