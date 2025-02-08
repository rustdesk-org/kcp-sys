use std::sync::{
    atomic::{AtomicBool, AtomicU32},
    Arc,
};

use anyhow::Context;
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::{select, sync::Notify, task::JoinSet, time::timeout};
use tracing::Instrument;

use crate::{
    error::Error,
    ffi_safe::{Kcp, KcpConfig},
    packet_def::KcpPacket,
    state::{KcpConnectionFSM, PacketHeaderFlagManipulator},
};

pub type Sender<T> = tokio::sync::mpsc::Sender<T>;
pub type Receiver<T> = tokio::sync::mpsc::Receiver<T>;

pub type KcpPakcetSender = Sender<KcpPacket>;
pub type KcpPacketReceiver = Receiver<KcpPacket>;

pub type KcpStreamSender = Sender<BytesMut>;
pub type KcpStreamReceiver = Receiver<BytesMut>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnId {
    conv: u32,
    src_session_id: u32,
    dst_session_id: u32,
}

impl From<&KcpPacket> for ConnId {
    fn from(packet: &KcpPacket) -> Self {
        Self {
            conv: packet.header().conv(),
            src_session_id: packet.header().src_session_id(),
            dst_session_id: packet.header().dst_session_id(),
        }
    }
}

impl ConnId {
    fn fill_packet_header(&self, packet: &mut KcpPacket) {
        packet
            .mut_header()
            .set_conv(self.conv)
            .set_src_session_id(self.src_session_id)
            .set_dst_session_id(self.dst_session_id);
    }
}

struct KcpConnectionInner {
    update_notifier: Notify,
    recv_notifier: Notify,
    send_notifier: Notify,

    has_new_input: AtomicBool,
    waiting_new_send_window: AtomicBool,
}

struct KcpConnection {
    conn_id: ConnId,
    kcp: Arc<Mutex<Box<Kcp>>>,

    inner: Arc<KcpConnectionInner>,

    send_sender: Option<Sender<BytesMut>>,
    send_receiver: Option<Receiver<BytesMut>>,

    recv_sender: Option<Sender<BytesMut>>,
    recv_receiver: Option<Receiver<BytesMut>>,

    send_close_notifier: Arc<Notify>,
    recv_closed: Arc<AtomicBool>,

    tasks: JoinSet<()>,
}

impl KcpConnection {
    pub fn new(conn_id: ConnId) -> Result<Self, Error> {
        let kcp = Kcp::new(KcpConfig::new_turbo(conn_id.conv))?;

        let (send_sender, send_receiver) = tokio::sync::mpsc::channel(128);
        let (recv_sender, recv_receiver) = tokio::sync::mpsc::channel(128);

        Ok(Self {
            conn_id,
            kcp: Arc::new(Mutex::new(kcp)),

            inner: Arc::new(KcpConnectionInner {
                update_notifier: Notify::new(),
                recv_notifier: Notify::new(),
                send_notifier: Notify::new(),

                has_new_input: AtomicBool::new(false),
                waiting_new_send_window: AtomicBool::new(false),
            }),

            send_sender: Some(send_sender),
            send_receiver: Some(send_receiver),

            recv_sender: Some(recv_sender),
            recv_receiver: Some(recv_receiver),

            send_close_notifier: Arc::new(Notify::new()),
            recv_closed: Arc::new(AtomicBool::new(false)),

            tasks: JoinSet::new(),
        })
    }

    pub fn run(&mut self, output_sender: KcpPakcetSender) {
        let conn_id = self.conn_id;
        self.kcp
            .lock()
            .set_output_cb(Box::new(move |conv, data: BytesMut| {
                let mut kcp_packet = KcpPacket::new_with_payload(&data);
                conn_id.fill_packet_header(&mut kcp_packet);
                kcp_packet.mut_header().set_data(true).set_ack(true);
                tracing::trace!(?conv, "sending output data: {:?}", kcp_packet);
                if let Err(e) = output_sender.try_send(kcp_packet) {
                    tracing::debug!(?e, ?conn_id, "send output data failed");
                }
                Ok(())
            }));

        // kcp updater
        let inner = self.inner.clone();
        let kcp = self.kcp.clone();
        let recv_closed = self.recv_closed.clone();
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

                if recv_closed.load(std::sync::atomic::Ordering::Relaxed) {
                    inner.recv_notifier.notify_one();
                }
            }
        });

        // handle packet send
        let kcp = self.kcp.clone();
        let inner = self.inner.clone();
        let mut send_receiver = self.send_receiver.take().unwrap();
        let send_close_notifier = self.send_close_notifier.clone();
        self.tasks.spawn(
            async move {
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
                    kcp.lock().flush();
                    inner.update_notifier.notify_one();
                }

                tracing::debug!(
                    ?conn_id,
                    "connection packet sender close, waiting for waitsnd to be 0"
                );

                // waiting for waitsnd to be 0
                while kcp.lock().waitsnd() > 0 {
                    inner
                        .waiting_new_send_window
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    inner.send_notifier.notified().await;
                }

                send_close_notifier.notify_one();
                tracing::debug!(?conn_id, "connection packet send task done");
            }
            .instrument(tracing::trace_span!("send_task", conn = ?conn_id)),
        );

        // handle packet recv
        let kcp = self.kcp.clone();
        let inner = self.inner.clone();
        let conn_id = self.conn_id;
        let recv_sender = self.recv_sender.take().unwrap();
        let recv_closed = self.recv_closed.clone();
        self.tasks.spawn(
            async move {
                let mut buf = BytesMut::new();
                while !recv_closed.load(std::sync::atomic::Ordering::Relaxed) {
                    let peeksize = kcp.lock().peeksize();
                    if peeksize <= 0 {
                        tracing::trace!("recv nothing, wait for next update");
                        inner.recv_notifier.notified().await;
                        continue;
                    };

                    if buf.capacity() < peeksize as usize {
                        buf.reserve(std::cmp::max(peeksize as usize, 4096));
                    }
                    kcp.lock().recv(&mut buf).unwrap();
                    tracing::trace!("recv data ({}): {:?}", buf.len(), buf);
                    assert_ne!(0, buf.len());
                    let send_ret = recv_sender.send(buf.split()).await;
                    if let Err(_) = send_ret {
                        break;
                    }
                }

                tracing::debug!(?conn_id, "connection packet recv task done");
            }
            .instrument(tracing::trace_span!("recv_task", conn = ?conn_id)),
        );
    }

    fn handle_input(&mut self, packet: &KcpPacket) -> Result<(), Error> {
        self.kcp.lock().handle_input(packet.payload())?;
        self.inner
            .has_new_input
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.update_notifier.notify_one();
        Ok(())
    }

    fn send_sender(&mut self) -> KcpStreamSender {
        self.send_sender.take().unwrap()
    }

    fn recv_receiver(&mut self) -> KcpStreamReceiver {
        self.recv_receiver.take().unwrap()
    }

    fn send_close_notifier(&self) -> Arc<Notify> {
        self.send_close_notifier.clone()
    }

    fn close_recv(&self) {
        self.recv_closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.recv_notifier.notify_one();
    }
}

impl Drop for KcpConnection {
    fn drop(&mut self) {
        self.send_close_notifier.notify_one();
    }
}

impl PacketHeaderFlagManipulator for KcpPacket {
    fn has_syn(&self) -> bool {
        self.header().is_syn()
    }

    fn has_ack(&self) -> bool {
        self.header().is_ack()
    }

    fn has_fin(&self) -> bool {
        self.header().is_fin()
    }

    fn has_rst(&self) -> bool {
        self.header().is_rst()
    }

    fn has_data(&self) -> bool {
        self.header().is_data()
    }

    fn set_syn(&mut self, value: bool) {
        self.mut_header().set_syn(value);
    }

    fn set_ack(&mut self, value: bool) {
        self.mut_header().set_ack(value);
    }

    fn set_fin(&mut self, value: bool) {
        self.mut_header().set_fin(value);
    }

    fn set_rst(&mut self, value: bool) {
        self.mut_header().set_rst(value);
    }

    fn set_data(&mut self, value: bool) {
        self.mut_header().set_data(value);
    }
}

struct KcpConnectionState {
    fsm: KcpConnectionFSM,
    notify: Arc<Notify>,
    conn_data: Bytes,
    last_pong: std::time::Instant,
}

impl std::fmt::Debug for KcpConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KcpConnectionState")
            .field("fsm", &self.fsm)
            .finish()
    }
}

impl KcpConnectionState {
    fn new(fsm: KcpConnectionFSM) -> Self {
        Self {
            fsm,
            notify: Arc::new(Notify::new()),
            conn_data: Bytes::new(),
            last_pong: std::time::Instant::now(),
        }
    }

    fn handle_packet(&mut self, packet: &KcpPacket) -> Result<Option<KcpPacket>, Error> {
        self.notify_pong();
        let mut out_packet = None;
        let old_state = self.fsm.clone();
        let _ = self.fsm.handle_packet(packet, &mut out_packet);
        if old_state != self.fsm {
            self.notify.notify_one();
            return Ok(out_packet);
        }
        Ok(None)
    }

    fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    fn is_established(&self) -> bool {
        matches!(self.fsm, KcpConnectionFSM::Established)
    }

    fn is_peer_closed(&self) -> bool {
        matches!(
            self.fsm,
            KcpConnectionFSM::PeerClosed | KcpConnectionFSM::Closed
        )
    }

    fn is_local_closed(&self) -> bool {
        matches!(
            self.fsm,
            KcpConnectionFSM::LocalClosed | KcpConnectionFSM::Closed
        )
    }

    fn is_closed(&self) -> bool {
        matches!(self.fsm, KcpConnectionFSM::Closed)
    }

    fn set_data(&mut self, data: Bytes) {
        self.conn_data = data;
    }

    fn notify_pong(&mut self) {
        self.last_pong = std::time::Instant::now();
    }

    fn is_pong_timeout(&self) -> bool {
        self.last_pong.elapsed() > std::time::Duration::from_secs(60)
    }
}

struct KcpEndpointData {
    cur_conv: AtomicU32,
    conn_map: DashMap<ConnId, KcpConnection>,
    state_map: DashMap<ConnId, KcpConnectionState>,
}

impl KcpEndpointData {
    fn new() -> Self {
        Self {
            cur_conv: AtomicU32::new(rand::random()),
            conn_map: DashMap::new(),
            state_map: DashMap::new(),
        }
    }
}

pub type KcpConfigFactory = Box<dyn Fn(u32) -> KcpConfig + Send + Sync>;

pub struct KcpEndpoint {
    id: u64,
    data: Arc<KcpEndpointData>,

    input_sender: KcpPakcetSender,
    input_receiver: Option<KcpPacketReceiver>,

    output_sender: KcpPakcetSender,
    output_receiver: Option<KcpPacketReceiver>,

    new_conn_sender: tokio::sync::mpsc::Sender<ConnId>,
    new_conn_receiver: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ConnId>>>,

    kcp_config_factory: KcpConfigFactory,

    tasks: JoinSet<()>,
}

impl std::fmt::Debug for KcpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KcpEndpoint").field("id", &self.id).finish()
    }
}

impl KcpEndpoint {
    pub fn new() -> Self {
        let (input_sender, input_receiver) = tokio::sync::mpsc::channel(1024);
        let (output_sender, output_receiver) = tokio::sync::mpsc::channel(1024);
        let (new_conn_sender, new_conn_receiver) = tokio::sync::mpsc::channel(4);

        Self {
            id: rand::random(),
            data: Arc::new(KcpEndpointData::new()),

            input_sender,
            input_receiver: Some(input_receiver),

            output_sender,
            output_receiver: Some(output_receiver),

            new_conn_sender,
            new_conn_receiver: Arc::new(tokio::sync::Mutex::new(new_conn_receiver)),

            kcp_config_factory: Box::new(|conv| KcpConfig::new_turbo(conv)),

            tasks: JoinSet::new(),
        }
    }

    pub fn set_kcp_config_factory(&mut self, factory: KcpConfigFactory) {
        self.kcp_config_factory = factory;
    }

    async fn try_handle_pingpong(
        data: &KcpEndpointData,
        packet: &KcpPacket,
        output_sender: &KcpPakcetSender,
    ) -> bool {
        let hdr = packet.header();

        if hdr.is_ping() && !hdr.is_pong() {
            let conn_id = ConnId::from(packet);
            let need_send_pong = data
                .state_map
                .get_mut(&conn_id)
                .map(|x| !x.is_local_closed())
                .unwrap_or(false);

            let mut out_packet = packet.clone();
            if need_send_pong {
                out_packet.mut_header().set_pong(true);
            } else {
                out_packet.mut_header().set_ping(false);
                out_packet.mut_header().set_rst(true);
            };

            tracing::trace!("sending pong packet: {:?}", out_packet);
            let ret = output_sender.send(out_packet).await;
            if let Err(e) = ret {
                tracing::error!(?e, "send pong packet failed");
            }
        }

        // all incoming packet should update pong time
        let conv = ConnId::from(packet);
        if let Some(mut state) = data.state_map.get_mut(&conv) {
            state.notify_pong();
        }

        packet.header().is_ping()
    }

    pub async fn run(&mut self) {
        let mut input_receiver = self.input_receiver.take().unwrap();
        let data = self.data.clone();
        let output_sender = self.output_sender.clone();
        let new_conn_sender = self.new_conn_sender.clone();

        self.tasks.spawn(
            async move {
                while let Some(packet) = input_receiver.recv().await {
                    tracing::trace!("recv packet: {:?}", packet);
                    if Self::try_handle_pingpong(&data, &packet, &output_sender).await {
                        continue;
                    }

                    let conv = ConnId::from(&packet);
                    if packet.header().is_data() && packet.payload().len() > 0 {
                        if let Some(mut conn) = data.conn_map.get_mut(&conv) {
                            if let Err(e) = conn.handle_input(&packet) {
                                tracing::error!(?e, ?conv, "handle input on connection failed");
                            } else {
                                tracing::trace!(?conv, "handle input on connection done");
                            }
                        } else {
                            tracing::debug!(
                                ?conv,
                                ?packet,
                                "no conn for conv when handling data packet"
                            );
                        }
                    }

                    let mut state_ref = data.state_map.get_mut(&conv);
                    let state = state_ref.as_deref_mut();
                    let mut out_packet: Option<KcpPacket> = None;
                    if state.is_none() {
                        if packet.header().is_rst() {
                            tracing::debug!(?conv, "reset packet for conn, but no state");
                            continue;
                        }
                        let mut tmp_fsm = KcpConnectionFSM::listen();
                        let res = tmp_fsm.handle_packet(&packet, &mut out_packet);
                        tracing::trace!(
                            ?conv,
                            ?state,
                            ?out_packet,
                            "handle first packet for conn, ret: {:?}",
                            res
                        );
                        if res.is_ok() {
                            let mut conn_state = KcpConnectionState::new(tmp_fsm);
                            conn_state.set_data(packet.payload().to_vec().into());
                            data.state_map.insert(conv, conn_state);
                        }
                    } else {
                        let state = state.unwrap();
                        let prev_established = state.is_established();
                        let ret = state.handle_packet(&packet);
                        tracing::trace!(?conv, ?state, "handle packet for conn, ret: {:?}", ret);
                        if ret.is_ok() {
                            out_packet = ret.unwrap();
                        }

                        if !prev_established && state.is_established() {
                            let _ = new_conn_sender.try_send(conv);
                        }

                        if state.is_peer_closed() {
                            tracing::debug!(?conv, "peer half closed, close recv");
                            data.conn_map.get_mut(&conv).map(|conn| conn.close_recv());
                        }

                        if state.is_closed() {
                            // state map will be cleaned by periodic task
                            tracing::debug!(?conv, "connection closed, remove state");
                            data.conn_map.remove(&conv);
                        }
                    }

                    drop(state_ref);
                    if let Some(mut out_packet) = out_packet {
                        conv.fill_packet_header(&mut out_packet);
                        tracing::trace!(?conv, ?out_packet, "sending output packet");
                        let ret = output_sender.send(out_packet).await;
                        if let Err(e) = ret {
                            tracing::error!(?e, "send output packet failed");
                        }
                    }
                }
            }
            .instrument(tracing::trace_span!("recv_task", id = self.id)),
        );

        // conn clean task
        let data = self.data.clone();
        self.tasks.spawn(async move {
            loop {
                data.state_map.retain(|_, state| {
                    !matches!(state.fsm, KcpConnectionFSM::Closed) && !state.is_pong_timeout()
                });
                data.conn_map
                    .retain(|conn_id, _| data.state_map.contains_key(conn_id));
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });

        // conn ping task
        let data = self.data.clone();
        let output_sender = self.output_sender.clone();
        self.tasks.spawn(async move {
            loop {
                let packets = data
                    .state_map
                    .iter()
                    .filter_map(|item| {
                        let (conn_id, state) = item.pair();
                        if state.is_closed() {
                            return None;
                        }
                        let mut out_packet = KcpPacket::new(0);
                        conn_id.fill_packet_header(&mut out_packet);
                        out_packet.mut_header().set_ping(true);
                        Some(out_packet)
                    })
                    .collect::<Vec<_>>();

                for packet in packets {
                    let ret = output_sender.send(packet).await;
                    if let Err(e) = ret {
                        tracing::error!(?e, "send ping packet failed");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }

                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }

    fn add_conn(&self, conn_id: ConnId) -> Result<(), Error> {
        let mut conn = KcpConnection::new(conn_id)?;
        conn.run(self.output_sender.clone());

        let data = self.data.clone();
        let close_notifier = conn.send_close_notifier();

        data.conn_map.insert(conn_id, conn);

        let output_sender = self.output_sender.clone();
        let data = Arc::downgrade(&data);
        tokio::spawn(async move {
            close_notifier.notified().await;
            let Some(data) = data.upgrade() else {
                return;
            };
            let mut out_packet = KcpPacket::new(0);
            let Some(mut state) = data.state_map.get_mut(&conn_id) else {
                return;
            };

            let close_ret = state.fsm.close(&mut out_packet);
            let cur_state = state.fsm.clone();
            let is_closed = state.is_closed();
            drop(state);
            match close_ret {
                Ok(_) => {
                    conn_id.fill_packet_header(&mut out_packet);
                    output_sender.send(out_packet).await.unwrap();
                }
                Err(e) => {
                    tracing::error!(?e, ?conn_id, "close connection failed");
                }
            }

            if is_closed {
                data.conn_map.remove(&conn_id);
            }

            tracing::debug!(?conn_id, ?cur_state, "connection close watcher done");
        });

        Ok(())
    }

    pub fn output_receiver(&mut self) -> Option<KcpPacketReceiver> {
        self.output_receiver.take()
    }

    pub fn input_sender(&self) -> KcpPakcetSender {
        self.input_sender.clone()
    }

    pub fn input_sender_ref(&self) -> &KcpPakcetSender {
        &self.input_sender
    }

    pub fn conn_sender_receiver(
        &self,
        conn_id: ConnId,
    ) -> Option<(KcpStreamSender, KcpStreamReceiver)> {
        let mut conn = self.data.conn_map.get_mut(&conn_id)?;
        Some((conn.send_sender(), conn.recv_receiver()))
    }

    pub fn conn_data(&self, conn_id: &ConnId) -> Option<Bytes> {
        let state = self.data.state_map.get(conn_id)?;
        Some(state.conn_data.clone())
    }

    #[tracing::instrument(ret)]
    pub async fn connect(
        &self,
        timeout_dur: std::time::Duration,
        src_session_id: u32,
        dst_session_id: u32,
        conn_data: Bytes,
    ) -> Result<ConnId, Error> {
        let mut out_packet = KcpPacket::new_with_payload(&conn_data);
        let conn_id = loop {
            let conv_cand = self
                .data
                .cur_conv
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let conn_id = ConnId {
                conv: conv_cand,
                src_session_id,
                dst_session_id,
            };
            if !self.data.state_map.contains_key(&conn_id) {
                break conn_id;
            }
        };

        let fsm = KcpConnectionFSM::connect(&mut out_packet);
        let mut state = KcpConnectionState::new(fsm);
        state.set_data(conn_data);
        let notify = state.notify();
        self.data.state_map.insert(conn_id, state);

        conn_id.fill_packet_header(&mut out_packet);

        tracing::trace!(?conn_id, "connect packet: {:?}", out_packet);
        self.output_sender
            .send(out_packet)
            .await
            .with_context(|| "send connect packet failed")?;

        if timeout(timeout_dur, notify.notified()).await.is_err() {
            self.data.state_map.remove(&conn_id);
            return Err(Error::ConnectTimeout);
        }

        if let Some(state) = self.data.state_map.get(&conn_id) {
            tracing::debug!(?conn_id, ?state, "connect done, checkin state");
            if matches!(state.fsm, KcpConnectionFSM::Established) {
                self.add_conn(conn_id)?;
                return Ok(conn_id);
            } else {
                drop(state);
                self.data.state_map.remove(&conn_id);
            }
            // if task aborted, the state map will be cleaned by periodic task
        }

        return Err(anyhow::anyhow!("connect failed").into());
    }

    pub async fn accept(&self) -> Result<ConnId, Error> {
        let conn_receiver = self.new_conn_receiver.clone();

        loop {
            let Some(conn_id) = conn_receiver.lock().await.recv().await else {
                return Err(Error::Shutdown);
            };

            let Some(state) = self.data.state_map.get(&conn_id) else {
                tracing::debug!(?conn_id, "no state for conn, ignore");
                continue;
            };

            if matches!(state.fsm, KcpConnectionFSM::Established) {
                self.add_conn(conn_id)?;
                return Ok(conn_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer as _};

    use super::*;

    fn _enable_log() {
        let console_layer = tracing_subscriber::fmt::layer()
            .pretty()
            .with_writer(std::io::stderr)
            .with_filter(LevelFilter::TRACE);

        tracing_subscriber::Registry::default()
            .with(console_layer)
            .init();
    }

    async fn prepare_test() -> (KcpEndpoint, KcpEndpoint, JoinSet<()>) {
        let mut client_endpoint = KcpEndpoint::new();
        let mut server_endpoint = KcpEndpoint::new();
        let mut t = JoinSet::new();

        client_endpoint.run().await;
        server_endpoint.run().await;

        let client_input_sender = client_endpoint.input_sender();
        let mut server_output_receiver = server_endpoint.output_receiver().unwrap();
        t.spawn(async move {
            while let Some(packet) = server_output_receiver.recv().await {
                let _ = client_input_sender.send(packet).await;
            }
        });

        let server_input_sender = server_endpoint.input_sender();
        let mut client_output_receiver = client_endpoint.output_receiver().unwrap();
        t.spawn(async move {
            while let Some(packet) = client_output_receiver.recv().await {
                let _ = server_input_sender.send(packet).await;
            }
        });

        (client_endpoint, server_endpoint, t)
    }

    #[tokio::test]
    async fn test_kcp_connect_and_close() {
        let mut p = KcpPacket::new(0);
        let _ = p.mut_header().conv();

        let (client_endpoint, server_endpoint, t) = prepare_test().await;

        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(1), 1, 3, Bytes::from("conn")),
            server_endpoint.accept()
        );

        assert_eq!(*connect_ret.as_ref().unwrap(), accept_ret.unwrap());

        let conv = connect_ret.unwrap();

        let client_conn_data = client_endpoint.conn_data(&conv).unwrap();
        assert_eq!("conn", String::from_utf8_lossy(&client_conn_data));

        let server_conn_data = server_endpoint.conn_data(&conv).unwrap();
        assert_eq!("conn", String::from_utf8_lossy(&server_conn_data));

        let (client_sender, mut client_receiver) =
            client_endpoint.conn_sender_receiver(conv).unwrap();
        let (server_sender, mut server_receiver) =
            server_endpoint.conn_sender_receiver(conv).unwrap();

        client_sender.send(BytesMut::from("hello")).await.unwrap();
        let data = server_receiver.recv().await.unwrap();
        assert_eq!("hello", String::from_utf8_lossy(&data));

        server_sender.send(BytesMut::from("world")).await.unwrap();
        let data = client_receiver.recv().await.unwrap();
        assert_eq!("world", String::from_utf8_lossy(&data));

        // test half close
        drop(client_sender);
        assert!(server_receiver.recv().await.is_none());
        // server can still send data
        server_sender.send(BytesMut::from("world")).await.unwrap();
        let data = client_receiver.recv().await.unwrap();
        assert_eq!("world", String::from_utf8_lossy(&data));

        // full close
        drop(server_sender);
        assert!(client_receiver.recv().await.is_none());

        drop(client_endpoint);
        drop(server_endpoint);

        t.join_all().await;
    }
}
