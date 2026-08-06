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
    pub fn new(conn_id: ConnId, config: KcpConfig) -> Result<Self, Error> {
        let kcp = Kcp::new(config)?;

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
                match output_sender.try_send(kcp_packet) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // Dropped here = self-inflicted loss KCP will re-pay with a
                        // retransmit; with the enlarged channel this should not happen.
                        log::warn!(
                            "kcp output channel full, packet dropped, conn: {:?}",
                            conn_id
                        );
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        // Normal during endpoint teardown.
                        tracing::debug!(?conn_id, "kcp output channel closed");
                    }
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
        let Some(mut send_receiver) = self.send_receiver.take() else {
            log::error!("send receiver is not set");
            return;
        };
        let send_close_notifier = self.send_close_notifier.clone();
        self.tasks.spawn(
            async move {
                while let Some(data) = send_receiver.recv().await {
                    let data = data.freeze();
                    let max_send = kcp.lock().max_chunk_size();

                    for chunk in data.chunks(max_send) {
                        // flow control wait
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

                        if let Err(e) = kcp.lock().send(chunk) {
                            log::error!("send data failed: {:?}, len: {}", e, chunk.len());
                            break;
                        }
                        kcp.lock().flush();
                        inner.update_notifier.notify_one();
                    }
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
        let Some(recv_sender) = self.recv_sender.take() else {
            log::error!("recv sender is not set");
            return;
        };
        let recv_closed = self.recv_closed.clone();
        self.tasks.spawn(
            async move {
                let mut buf = BytesMut::new();
                while !recv_closed.load(std::sync::atomic::Ordering::Relaxed) {
                    let peeksize = kcp.lock().peeksize();
                    if peeksize < 0 {
                        tracing::trace!("recv nothing, wait for next update");
                        inner.recv_notifier.notified().await;
                        continue;
                    };

                    if buf.capacity() < std::cmp::max(peeksize as usize, 1) {
                        buf.reserve(std::cmp::max(peeksize as usize, 4096));
                    }
                    if let Err(e) = kcp.lock().recv(&mut buf) {
                        log::error!("recv data failed: {:?}", e);
                        continue;
                    }
                    tracing::trace!("recv data ({}): {:?}", buf.len(), buf);
                    if buf.is_empty() {
                        // A zero-length segment (only produced by crafted input) must be
                        // drained, not asserted on: leaving it queued would wedge the
                        // stream, and panicking would abort the whole process.
                        continue;
                    }
                    let send_ret = recv_sender.send(buf.split()).await;
                    if send_ret.is_err() {
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

    fn send_sender(&mut self) -> Option<KcpStreamSender> {
        self.send_sender.take()
    }

    fn recv_receiver(&mut self) -> Option<KcpStreamReceiver> {
        self.recv_receiver.take()
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

// Cap on timer-driven SYN-ACK retransmits per conn: bounds the work a
// half-open entry costs and the reflection a spoofed SYN can buy (at most
// 1 immediate + MAX_SYN_ACK_RETRIES timed replies). The reactive
// duplicate-SYN path is uncapped - it is 1:1 with packets the peer
// actually sends.
const MAX_SYN_ACK_RETRIES: u32 = 10;

struct KcpConnectionState {
    fsm: KcpConnectionFSM,
    notify: Arc<Notify>,
    conn_data: Bytes,
    last_pong: std::time::Instant,
    // Timer-driven SYN-ACK retransmits consumed so far (see the liveness
    // task). Atomic so the liveness scan can consume budget under DashMap's
    // shard read lock; that task is the only writer. The budget is never
    // replenished - once exhausted, only the 1:1 reactive duplicate-SYN path
    // remains - and it is deliberately NOT reset on duplicate SYNs: resetting
    // would let an attacker replenish the timed-amplification budget by
    // spraying SYNs.
    syn_ack_retries: AtomicU32,
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
            syn_ack_retries: AtomicU32::new(0),
        }
    }

    fn handle_packet(&mut self, packet: &KcpPacket) -> Result<Option<KcpPacket>, Error> {
        self.notify_pong();
        let mut out_packet = None;
        let old_state = self.fsm;
        let res = self.fsm.handle_packet(packet, &mut out_packet);
        if old_state != self.fsm {
            self.notify.notify_one();
            return Ok(out_packet);
        }
        // State unchanged: forward a response only when the FSM produced one on success
        // (an idempotent handshake retransmit, e.g. a re-sent SYN-ACK). Error-triggered
        // RSTs are suppressed here so a stray or duplicated steady-state packet cannot tear
        // down a healthy connection.
        match res {
            Ok(()) => Ok(out_packet),
            Err(_) => Ok(None),
        }
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

impl Default for KcpEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KcpEndpoint {
    pub fn new() -> Self {
        // The output callback runs synchronously inside ikcp_flush and can only try_send:
        // a full channel means self-inflicted packet loss. A flush burst can emit up to a
        // whole send window (turbo sndwnd = 1024 segments) plus ACKs, so the channel must
        // be comfortably larger than the window to never shed packets at full load. The
        // input side gets the same headroom so a briefly stalled endpoint task does not
        // backpressure the socket reader into dropping datagrams.
        let (input_sender, input_receiver) = tokio::sync::mpsc::channel(4096);
        let (output_sender, output_receiver) = tokio::sync::mpsc::channel(4096);
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

            kcp_config_factory: Box::new(KcpConfig::new_turbo),

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
            // Answer pings until the connection is FULLY closed: LocalClosed only shut our
            // send side, our receive side still expects data, and replying RST here would
            // make the peer tear down a half-open connection that is still delivering.
            let need_send_pong = data
                .state_map
                .get_mut(&conn_id)
                .map(|x| !x.is_closed())
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
                log::error!("send pong packet failed: {:?}", e);
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
        let Some(mut input_receiver) = self.input_receiver.take() else {
            log::error!("input receiver is not set");
            return;
        };
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
                    if packet.header().is_data() && !packet.payload().is_empty() {
                        if let Some(mut conn) = data.conn_map.get_mut(&conv) {
                            if let Err(e) = conn.handle_input(&packet) {
                                log::warn!(
                                    "handle input on connection failed: {:?}, conv: {:?}",
                                    e,
                                    conv
                                );
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
                    if let Some(state) = state {
                        let prev_established = state.is_established();
                        let ret = state.handle_packet(&packet);
                        tracing::trace!(?conv, ?state, "handle packet for conn, ret: {:?}", ret);
                        if let Ok(pkt) = ret {
                            out_packet = pkt;
                        }

                        if !prev_established && state.is_established() {
                            let _ = new_conn_sender.try_send(conv);
                        }

                        if state.is_peer_closed() {
                            tracing::debug!(?conv, "peer half closed, close recv");
                            if let Some(conn) = data.conn_map.get_mut(&conv) {
                                conn.close_recv()
                            }
                        }

                        if state.is_closed() {
                            // state map will be cleaned by periodic task.
                            // Log the close cause at info: session-death reasons are the
                            // first thing needed when users report drops.
                            log::info!(
                                "kcp conn closed by peer packet (rst: {}, fin: {}), conv: {:?}",
                                packet.header().is_rst(),
                                packet.header().is_fin(),
                                conv
                            );
                            data.conn_map.remove(&conv);
                        }
                    } else {
                        if packet.header().is_rst() {
                            tracing::debug!(?conv, "reset packet for conn, but no state");
                            continue;
                        }
                        let mut tmp_fsm = KcpConnectionFSM::listen();
                        let res = tmp_fsm.handle_packet(&packet, &mut out_packet);
                        tracing::trace!(
                            ?conv,
                            ?out_packet,
                            "handle first packet for conn, ret: {:?}",
                            res
                        );
                        if res.is_ok() {
                            let mut conn_state = KcpConnectionState::new(tmp_fsm);
                            conn_state.set_data(packet.payload().to_vec().into());
                            data.state_map.insert(conv, conn_state);
                        }
                    }

                    drop(state_ref);
                    if let Some(mut out_packet) = out_packet {
                        conv.fill_packet_header(&mut out_packet);
                        tracing::trace!(?conv, ?out_packet, "sending output packet");
                        let ret = output_sender.send(out_packet).await;
                        if let Err(e) = ret {
                            log::warn!("send output packet failed: {:?}", e);
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
                // Collect first, log after: retain holds a shard write lock, and a
                // blocking logger backend would stall packet handling for that shard.
                let mut reaped = Vec::new();
                data.state_map.retain(|conn_id, state| {
                    let closed = matches!(state.fsm, KcpConnectionFSM::Closed);
                    let timed_out = state.is_pong_timeout();
                    if timed_out && !closed {
                        reaped.push((*conn_id, state.last_pong.elapsed()));
                    }
                    !closed && !timed_out
                });
                for (conn_id, since) in reaped {
                    // A silent reap was indistinguishable from every other way a
                    // session can end; name the reason for field diagnosis.
                    log::info!(
                        "kcp conn reaped by pong timeout ({:?} since last packet), conv: {:?}",
                        since,
                        conn_id
                    );
                }
                data.conn_map
                    .retain(|conn_id, _| data.state_map.contains_key(conn_id));
                data.state_map.shrink_to_fit();
                data.conn_map.shrink_to_fit();
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });

        // conn liveness task: pings established conns and retransmits the
        // SYN-ACK for conns stuck in SynReceived. Scheduled so that no backlog
        // of either kind can stretch the cadence:
        // - pings are sharded by conv into PING_SLOTS slots and each 1s tick
        //   serves one slot, so every conn is pinged about every PING_SLOTS
        //   seconds and per-tick work stays ~N/PING_SLOTS packets;
        // - SYN-ACK retransmits run every tick, budget-capped per conn;
        // - everything is try_send with no pacing sleeps, keeping the loop
        //   free of awaits under the scan. A full channel skips the packet
        //   until the next round - channel pressure means traffic is flowing,
        //   which already keeps the peer's pong timer fresh.
        let data = self.data.clone();
        let output_sender = self.output_sender.clone();
        self.tasks.spawn(async move {
            const TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
            const PING_SLOTS: u32 = 10;
            let mut slot = 0u32;
            loop {
                let mut packets = Vec::new();
                for item in data.state_map.iter() {
                    let (conn_id, state) = item.pair();
                    match state.fsm {
                        // The server has no other retransmit path: if the client's
                        // ACK+data reply was lost, the client is already Established
                        // and will never send another SYN, so the client's
                        // duplicate-SYN-ACK re-ACK branch is the only way left to
                        // finish the handshake.
                        KcpConnectionFSM::SynReceived => {
                            // Consume retransmit budget; this task is the only
                            // writer, so Relaxed suffices.
                            let granted = state
                                .syn_ack_retries
                                .fetch_update(
                                    std::sync::atomic::Ordering::Relaxed,
                                    std::sync::atomic::Ordering::Relaxed,
                                    |v| (v < MAX_SYN_ACK_RETRIES).then_some(v + 1),
                                )
                                .is_ok();
                            if granted {
                                let mut out_packet = KcpPacket::new(0);
                                out_packet.mut_header().set_syn(true);
                                out_packet.mut_header().set_ack(true);
                                conn_id.fill_packet_header(&mut out_packet);
                                packets.push(out_packet);
                            }
                        }
                        // Never ping a handshaking conn: connect()'s SYN retransmit
                        // loop owns client-side liveness, and pinging a SynSent conn
                        // makes a server that never saw the SYN answer RST, killing
                        // that loop after a single SYN.
                        KcpConnectionFSM::Established
                        | KcpConnectionFSM::LocalClosed
                        | KcpConnectionFSM::PeerClosed
                            if conn_id.conv % PING_SLOTS == slot =>
                        {
                            let mut out_packet = KcpPacket::new(0);
                            out_packet.mut_header().set_ping(true);
                            conn_id.fill_packet_header(&mut out_packet);
                            packets.push(out_packet);
                        }
                        _ => {}
                    }
                }
                slot = (slot + 1) % PING_SLOTS;

                for packet in packets {
                    // Best-effort: Full waits for the next round; Closed means
                    // the endpoint is tearing down.
                    let _ = output_sender.try_send(packet);
                }

                tokio::time::sleep(TICK_INTERVAL).await;
            }
        });
    }

    fn add_conn(&self, conn_id: ConnId) -> Result<(), Error> {
        // The factory was previously never consulted (KcpConnection hardcoded new_turbo),
        // which made set_kcp_config_factory a silent no-op.
        let config = (self.kcp_config_factory)(conn_id.conv);
        let mut conn = KcpConnection::new(conn_id, config)?;
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
            let cur_state = state.fsm;
            let is_closed = state.is_closed();
            drop(state);
            match close_ret {
                Ok(_) => {
                    conn_id.fill_packet_header(&mut out_packet);
                    if let Err(e) = output_sender.send(out_packet).await {
                        log::error!("send close packet failed: {:?}, conv: {:?}", e, conn_id);
                    }
                }
                Err(e) => {
                    log::warn!("close connection failed: {:?}, conv: {:?}", e, conn_id);
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
        let Some(send_sender) = conn.send_sender() else {
            log::error!("send sender is not set");
            return None;
        };
        let Some(recv_receiver) = conn.recv_receiver() else {
            log::error!("recv receiver is not set");
            return None;
        };
        Some((send_sender, recv_receiver))
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

        // Retransmit the SYN until the handshake advances or the overall timeout elapses.
        // KCP's connect packet is a single datagram; right after UDP hole punching the first
        // one (or the peer's SYN-ACK) is easily lost, and without retransmission that single
        // loss would sink the entire transport. The peer treats duplicate SYNs idempotently.
        let notified = notify.notified();
        tokio::pin!(notified);
        let deadline = tokio::time::Instant::now() + timeout_dur;
        let mut interval = std::time::Duration::from_millis(250);
        const MAX_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);
        let mut notified_ok = false;
        loop {
            // Check before sending: a backpressured output channel could otherwise
            // stretch this loop well past timeout_dur and emit a SYN after the deadline.
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            self.output_sender
                .send(out_packet.clone())
                .await
                .with_context(|| "send connect packet failed")?;

            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let wait = std::cmp::min(interval, deadline - now);
            if timeout(wait, &mut notified).await.is_ok() {
                notified_ok = true;
                break;
            }
            interval = std::cmp::min(interval.mul_f64(1.5), MAX_INTERVAL);
        }

        if !notified_ok {
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

    // Same wiring as prepare_test, but with flag-targeted handshake loss: drops the
    // first `syn_drop` SYN packets and the first `ack_drop` handshake ACK+data replies
    // on the client->server path, and the first `syn_ack_drop` SYN-ACK packets on the
    // server->client path. Targeted by handshake flags, not by position: the endpoint
    // interleaves other traffic (pings, retransmits), so a positional drop budget gets
    // absorbed by unrelated packets and the handshake packet sails through untouched.
    async fn prepare_test_lossy(
        syn_drop: usize,
        syn_ack_drop: usize,
        ack_drop: usize,
    ) -> (KcpEndpoint, KcpEndpoint, JoinSet<()>) {
        let mut client_endpoint = KcpEndpoint::new();
        let mut server_endpoint = KcpEndpoint::new();
        let mut t = JoinSet::new();

        client_endpoint.run().await;
        server_endpoint.run().await;

        let client_input_sender = client_endpoint.input_sender();
        let mut server_output_receiver = server_endpoint.output_receiver().unwrap();
        t.spawn(async move {
            let mut dropped = 0usize;
            while let Some(packet) = server_output_receiver.recv().await {
                let h = packet.header();
                if h.is_syn() && h.is_ack() && dropped < syn_ack_drop {
                    dropped += 1;
                    continue;
                }
                let _ = client_input_sender.send(packet).await;
            }
        });

        let server_input_sender = server_endpoint.input_sender();
        let mut client_output_receiver = client_endpoint.output_receiver().unwrap();
        t.spawn(async move {
            let mut syn_dropped = 0usize;
            let mut ack_dropped = 0usize;
            while let Some(packet) = client_output_receiver.recv().await {
                let h = packet.header();
                if h.is_syn() && !h.is_ack() && syn_dropped < syn_drop {
                    syn_dropped += 1;
                    continue;
                }
                // The handshake completion is the only ACK+data packet with an empty
                // payload; KCP data segments always carry bytes.
                if h.is_ack()
                    && h.is_data()
                    && packet.payload().is_empty()
                    && ack_dropped < ack_drop
                {
                    ack_dropped += 1;
                    continue;
                }
                let _ = server_input_sender.send(packet).await;
            }
        });

        (client_endpoint, server_endpoint, t)
    }

    #[tokio::test]
    async fn test_kcp_connect_with_handshake_loss() {
        // Drop the client's first SYN and the server's first SYN-ACK. Recovery needs
        // connect()'s SYN retransmission (undisturbed by pings) plus the SynReceived
        // duplicate-SYN branch re-emitting the SYN-ACK. accept() is bounded so a
        // regression fails the test instead of hanging the job.
        let (client_endpoint, server_endpoint, t) = prepare_test_lossy(1, 1, 0).await;

        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(5), 1, 3, Bytes::from("conn")),
            tokio::time::timeout(std::time::Duration::from_secs(10), server_endpoint.accept())
        );

        let conv = connect_ret.expect("connect should recover from handshake loss");
        assert_eq!(
            conv,
            accept_ret
                .expect("accept should not outlive connect's deadline")
                .expect("accept should recover from handshake loss")
        );

        // Data must still flow over the recovered handshake.
        let (client_sender, _client_receiver) = client_endpoint.conn_sender_receiver(conv).unwrap();
        let (_server_sender, mut server_receiver) =
            server_endpoint.conn_sender_receiver(conv).unwrap();

        client_sender.send(BytesMut::from("hello")).await.unwrap();
        let data = server_receiver.recv().await.unwrap();
        assert_eq!("hello", String::from_utf8_lossy(&data));

        drop(client_endpoint);
        drop(server_endpoint);
        t.join_all().await;
    }

    #[tokio::test]
    async fn test_kcp_connect_with_ack_loss() {
        // Drop the client's handshake ACK+data reply (third packet). The client is
        // already Established and will never send another SYN, so only the server's
        // periodic SYN-ACK retransmit plus the client's duplicate-SYN-ACK re-ACK can
        // finish the handshake and unblock accept().
        let (client_endpoint, server_endpoint, t) = prepare_test_lossy(0, 0, 1).await;

        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(5), 1, 3, Bytes::from("conn")),
            tokio::time::timeout(std::time::Duration::from_secs(10), server_endpoint.accept())
        );

        let conv = connect_ret.expect("connect side is unaffected by the lost ACK");
        assert_eq!(
            conv,
            accept_ret
                .expect("accept must be unblocked by the SYN-ACK retransmit")
                .expect("accept should recover from ACK loss")
        );

        // Data must flow over the recovered handshake.
        let (client_sender, _client_receiver) = client_endpoint.conn_sender_receiver(conv).unwrap();
        let (_server_sender, mut server_receiver) =
            server_endpoint.conn_sender_receiver(conv).unwrap();

        client_sender.send(BytesMut::from("hello")).await.unwrap();
        let data = server_receiver.recv().await.unwrap();
        assert_eq!("hello", String::from_utf8_lossy(&data));

        drop(client_endpoint);
        drop(server_endpoint);
        t.join_all().await;
    }

    #[tokio::test(start_paused = true)]
    async fn test_kcp_syn_ack_retransmit_is_capped() {
        // A half-open conn (SYN received, ACK never arrives) must retransmit the
        // SYN-ACK often enough to ride out loss, but with a hard cap: a spoofed SYN
        // may not buy an unbounded packet stream, and a flood of half-open entries
        // may not keep the liveness pass busy indefinitely.
        let mut server = KcpEndpoint::new();
        server.run().await;

        let input = server.input_sender();
        let mut output = server.output_receiver().unwrap();

        let mut syn = KcpPacket::new(0);
        syn.mut_header()
            .set_conv(7)
            .set_src_session_id(1)
            .set_dst_session_id(3);
        syn.mut_header().set_syn(true);
        input.send(syn).await.unwrap();

        let mut syn_acks = 0usize;
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(30), output.recv()).await {
                Ok(Some(p)) => {
                    let h = p.header();
                    if h.is_syn() && h.is_ack() {
                        syn_acks += 1;
                    }
                }
                Ok(None) => break,
                // 30 virtual seconds of silence: the retransmit timer has given up.
                Err(_) => break,
            }
        }

        // 1 immediate SYN-ACK from the Listen transition + MAX_SYN_ACK_RETRIES
        // timer-driven retransmits, then nothing.
        assert_eq!(
            syn_acks,
            1 + MAX_SYN_ACK_RETRIES as usize,
            "timer retransmits must be capped"
        );

        drop(server);
    }

    #[tokio::test]
    async fn test_kcp_config_factory_is_used_and_cc_profile_transports_data() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut client_endpoint = KcpEndpoint::new();
        let mut server_endpoint = KcpEndpoint::new();

        // Congestion-controlled turbo profile (nc=0 engages KCP's built-in algorithm);
        // the counters prove add_conn consults the factory instead of hardcoding turbo.
        static CLIENT_CALLS: AtomicUsize = AtomicUsize::new(0);
        static SERVER_CALLS: AtomicUsize = AtomicUsize::new(0);
        client_endpoint.set_kcp_config_factory(Box::new(|conv| {
            CLIENT_CALLS.fetch_add(1, Ordering::SeqCst);
            let mut c = KcpConfig::new_turbo(conv);
            c.nc = Some(0);
            c
        }));
        server_endpoint.set_kcp_config_factory(Box::new(|conv| {
            SERVER_CALLS.fetch_add(1, Ordering::SeqCst);
            let mut c = KcpConfig::new_turbo(conv);
            c.nc = Some(0);
            c
        }));

        client_endpoint.run().await;
        server_endpoint.run().await;

        let mut t = JoinSet::new();
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

        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(5), 1, 3, Bytes::new()),
            server_endpoint.accept()
        );
        let conv = connect_ret.unwrap();
        assert_eq!(conv, accept_ret.unwrap());
        assert_eq!(1, CLIENT_CALLS.load(Ordering::SeqCst));
        assert_eq!(1, SERVER_CALLS.load(Ordering::SeqCst));

        // Data flows under the congestion-controlled profile (cwnd slow-starts from 1,
        // it must not deadlock at zero).
        let (client_sender, _cr) = client_endpoint.conn_sender_receiver(conv).unwrap();
        let (_ss, mut server_receiver) = server_endpoint.conn_sender_receiver(conv).unwrap();
        let payload = vec![7u8; 256 * 1024];
        client_sender
            .send(BytesMut::from(&payload[..]))
            .await
            .unwrap();
        let mut got = 0usize;
        while got < payload.len() {
            let data = server_receiver.recv().await.unwrap();
            assert!(data.iter().all(|&b| b == 7));
            got += data.len();
        }
        assert_eq!(got, payload.len());

        drop(client_endpoint);
        drop(server_endpoint);
        t.join_all().await;
    }

    #[tokio::test]
    async fn test_kcp_bulk_transfer_survives_sustained_loss() {
        // Deterministic sustained loss on both paths (every 7th packet client->server,
        // every 5th server->client, retransmits included). The transfer must complete
        // with byte-exact content — this is the regression net for the stability work:
        // handshake retransmit, KCP retransmission, ping/pong liveness all under loss.
        let mut client_endpoint = KcpEndpoint::new();
        let mut server_endpoint = KcpEndpoint::new();
        let mut t = JoinSet::new();

        client_endpoint.run().await;
        server_endpoint.run().await;

        let client_input_sender = client_endpoint.input_sender();
        let mut server_output_receiver = server_endpoint.output_receiver().unwrap();
        t.spawn(async move {
            let mut seq = 0usize;
            while let Some(packet) = server_output_receiver.recv().await {
                seq += 1;
                if seq % 5 == 0 {
                    continue;
                }
                let _ = client_input_sender.send(packet).await;
            }
        });
        let server_input_sender = server_endpoint.input_sender();
        let mut client_output_receiver = client_endpoint.output_receiver().unwrap();
        t.spawn(async move {
            let mut seq = 0usize;
            while let Some(packet) = client_output_receiver.recv().await {
                seq += 1;
                if seq % 7 == 0 {
                    continue;
                }
                let _ = server_input_sender.send(packet).await;
            }
        });

        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(10), 1, 3, Bytes::new()),
            tokio::time::timeout(std::time::Duration::from_secs(20), server_endpoint.accept())
        );
        let conv = connect_ret.expect("connect must survive lossy handshake");
        assert_eq!(
            conv,
            accept_ret
                .expect("accept timed out under sustained loss")
                .unwrap()
        );

        let (client_sender, _cr) = client_endpoint.conn_sender_receiver(conv).unwrap();
        let (_ss, mut server_receiver) = server_endpoint.conn_sender_receiver(conv).unwrap();

        let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        let send_task = tokio::spawn(async move {
            for chunk in payload.chunks(64 * 1024) {
                client_sender.send(BytesMut::from(chunk)).await.unwrap();
            }
            client_sender
        });

        let recv_all = async {
            let mut got = Vec::with_capacity(expected.len());
            while got.len() < expected.len() {
                let data = server_receiver.recv().await.expect("stream ended early");
                got.extend_from_slice(&data);
            }
            got
        };
        let got = tokio::time::timeout(std::time::Duration::from_secs(60), recv_all)
            .await
            .expect("lossy transfer did not complete in time");
        assert_eq!(got.len(), expected.len());
        assert!(got == expected, "payload corrupted in lossy transfer");

        let _client_sender = send_task.await.unwrap();
        drop(client_endpoint);
        drop(server_endpoint);
        t.join_all().await;
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
