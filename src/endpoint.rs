use std::sync::{
    atomic::{AtomicBool, AtomicU32},
    Arc,
};

use bytes::BytesMut;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::{select, sync::Notify, task::JoinSet};

use crate::{
    error::Error,
    ffi_safe::{Kcp, KcpConfig},
    packet_def::KcpPacket,
};

pub type Sender<T> = tokio::sync::mpsc::Sender<T>;
pub type Receiver<T> = tokio::sync::mpsc::Receiver<T>;

pub type KcpPakcetSender = Sender<KcpPacket>;
pub type KcpPacketReceiver = Receiver<KcpPacket>;

pub type KcpStreamSender = Sender<BytesMut>;
pub type KcpStreamReceiver = Receiver<BytesMut>;

enum KcpClientState {
    SynSent,
    Established,
    Fin,
}

enum KcpServerState {
    SynReceived,
    Established,
    Fin,
}

enum KcpConnectionState {
    Client(KcpClientState),
    Server(KcpServerState),
}

struct KcpConnectionInner {
    conv: u32,

    update_notifier: Notify,
    recv_notifier: Notify,
    send_notifier: Notify,

    has_new_input: AtomicBool,
    waiting_new_send_window: AtomicBool,
}

struct KcpConnection {
    conv: u32,
    kcp: Arc<Mutex<Box<Kcp>>>,

    inner: Arc<KcpConnectionInner>,

    send_sender: Sender<BytesMut>,
    send_receiver: Option<Receiver<BytesMut>>,

    recv_sender: Sender<BytesMut>,
    recv_receiver: Option<Receiver<BytesMut>>,

    tasks: JoinSet<()>,
}

impl KcpConnection {
    fn new(conv: u32) -> Result<Self, Error> {
        let kcp = Kcp::new(KcpConfig::new(conv))?;

        let (send_sender, send_receiver) = tokio::sync::mpsc::channel(16);
        let (recv_sender, recv_receiver) = tokio::sync::mpsc::channel(16);

        Ok(Self {
            conv,
            kcp: Arc::new(Mutex::new(kcp)),

            inner: Arc::new(KcpConnectionInner {
                conv,

                update_notifier: Notify::new(),
                recv_notifier: Notify::new(),
                send_notifier: Notify::new(),

                has_new_input: AtomicBool::new(false),
                waiting_new_send_window: AtomicBool::new(false),
            }),

            send_sender,
            send_receiver: Some(send_receiver),

            recv_sender,
            recv_receiver: Some(recv_receiver),

            tasks: JoinSet::new(),
        })
    }

    fn run(&mut self, output_sender: KcpPakcetSender) {
        self.kcp
            .lock()
            .set_output_cb(Box::new(move |conv, data: BytesMut| {
                let mut kcp_packet = KcpPacket::new_with_payload(&data);
                kcp_packet
                    .mut_header()
                    .set_conv(conv)
                    .set_len(data.len() as u16)
                    .set_data(true);
                println!("conv {} send output data: {:?}", conv, kcp_packet);
                let _ = output_sender.try_send(kcp_packet);
                Ok(())
            }));

        // kcp updater
        let inner = self.inner.clone();
        let kcp = self.kcp.clone();
        self.tasks.spawn(async move {
            loop {
                let next_update_ms = kcp.lock().next_update_delay_ms();
                select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(next_update_ms as u64)) => {}
                    _ = inner.update_notifier.notified() => {}
                }

                kcp.lock().update();

                if inner.has_new_input.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    inner.recv_notifier.notify_one();
                }

                if inner.waiting_new_send_window.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    inner.send_notifier.notify_one();
                }
            }
        });

        // handle packet send
        let kcp = self.kcp.clone();
        let inner = self.inner.clone();
        let mut send_receiver = self.send_receiver.take().unwrap();
        self.tasks.spawn(async move {
            while let Some(data) = send_receiver.recv().await {
                loop {
                    let (waitsnd, sndwnd) = {
                        let kcp = kcp.lock();
                        (kcp.waitsnd(), kcp.sendwnd())
                    };
                    if waitsnd > 2 * sndwnd {
                        inner
                            .waiting_new_send_window
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        inner.send_notifier.notified().await;
                    } else {
                        break;
                    }
                }
                kcp.lock().send(data.freeze()).unwrap();
            }
        });

        // handle packet recv
        let kcp = self.kcp.clone();
        let inner = self.inner.clone();
        let recv_sender = self.recv_sender.clone();
        self.tasks.spawn(async move {
            let mut buf = BytesMut::new();
            loop {
                if buf.capacity() < 1024 {
                    buf.reserve(4096);
                }
                let ret = kcp.lock().recv(&mut buf);
                if let Err(_) = ret {
                    println!("recv error, conv: {}", inner.conv);
                    inner.recv_notifier.notified().await;
                } else {
                    println!("recv data: {:?}", buf);
                    assert_ne!(0, buf.len());
                    let send_ret = recv_sender.send(buf.split()).await;
                    if let Err(_) = send_ret {
                        break;
                    }
                }
            }
        });
    }

    fn handle_input(&mut self, packet: KcpPacket) {
        let _ = self.kcp.lock().handle_input(packet.payload());
        self.inner
            .has_new_input
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.update_notifier.notify_one();
    }

    fn send_sender(&self) -> KcpStreamSender {
        self.send_sender.clone()
    }

    fn recv_receiver(&mut self) -> KcpStreamReceiver {
        self.recv_receiver.take().unwrap()
    }
}

pub struct KcpEndpoint {
    cur_conv: AtomicU32,
    established_map: Arc<DashMap<u32, KcpConnection>>,

    input_sender: KcpPakcetSender,
    input_receiver: Option<KcpPacketReceiver>,

    output_sender: KcpPakcetSender,
    output_receiver: Option<KcpPacketReceiver>,

    tasks: JoinSet<()>,
}

impl KcpEndpoint {
    pub fn new() -> Self {
        let (input_sender, input_receiver) = tokio::sync::mpsc::channel(1024);
        let (output_sender, output_receiver) = tokio::sync::mpsc::channel(1024);

        Self {
            cur_conv: AtomicU32::new(0),
            established_map: Arc::new(DashMap::new()),

            input_sender,
            input_receiver: Some(input_receiver),

            output_sender,
            output_receiver: Some(output_receiver),

            tasks: JoinSet::new(),
        }
    }

    pub async fn run(&mut self) {
        let mut input_receiver = self.input_receiver.take().unwrap();
        let established_map = self.established_map.clone();

        self.tasks.spawn(async move {
            while let Some(packet) = input_receiver.recv().await {
                let conv = packet.header().conv();
                if packet.header().is_data() {
                    let Some(mut conn) = established_map.get_mut(&conv) else {
                        continue;
                    };
                    let _ = conn.handle_input(packet);
                }
            }
        });
    }

    pub fn add_established(&mut self, conv: u32) -> Result<(), Error> {
        let mut conn = KcpConnection::new(conv)?;
        conn.run(self.output_sender.clone());

        self.established_map.insert(conv, conn);

        Ok(())
    }

    pub fn output_receiver(&mut self) -> Option<KcpPacketReceiver> {
        self.output_receiver.take()
    }

    pub fn input_sender(&self) -> KcpPakcetSender {
        self.input_sender.clone()
    }

    pub fn conn_sender_receiver(&self, conv: u32) -> Option<(KcpStreamSender, KcpStreamReceiver)> {
        let mut conn = self.established_map.get_mut(&conv)?;
        Some((conn.send_sender(), conn.recv_receiver()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_kcp_endpoint() {
        let mut client_endpoint = KcpEndpoint::new();
        let mut server_endpoint = KcpEndpoint::new();

        client_endpoint.run().await;
        server_endpoint.run().await;

        let _ = client_endpoint.add_established(1).unwrap();
        let _ = server_endpoint.add_established(1).unwrap();

        let client_input_sender = client_endpoint.input_sender();
        let mut server_output_receiver = server_endpoint.output_receiver().unwrap();
        let t1 = tokio::spawn(async move {
            while let Some(packet) = server_output_receiver.recv().await {
                let _ = client_input_sender.send(packet).await;
            }
        });

        let server_input_sender = server_endpoint.input_sender();
        let mut client_output_receiver = client_endpoint.output_receiver().unwrap();
        let t2 = tokio::spawn(async move {
            while let Some(packet) = client_output_receiver.recv().await {
                let _ = server_input_sender.send(packet).await;
            }
        });

        let (client_sender, mut client_receiver) = client_endpoint.conn_sender_receiver(1).unwrap();
        let (server_sender, mut server_receiver) = server_endpoint.conn_sender_receiver(1).unwrap();

        client_sender.send(BytesMut::from("hello")).await.unwrap();
        let data = server_receiver.recv().await.unwrap();
        assert_eq!("hello", String::from_utf8_lossy(&data));

        server_sender.send(BytesMut::from("world")).await.unwrap();
        let data = client_receiver.recv().await.unwrap();
        assert_eq!("world", String::from_utf8_lossy(&data));

        drop(client_endpoint);
        drop(server_endpoint);

        let _ = tokio::join!(t1, t2);
    }
}
