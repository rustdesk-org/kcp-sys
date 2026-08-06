use std::sync::{
    atomic::{AtomicBool, AtomicU32},
    Arc,
};

use anyhow::Context;
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::{select, sync::Notify, task::JoinSet, time::timeout};

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

// Logging rule for this module: a site whose call rate a peer or the network
// controls must either sit at `trace` (consumers write debug-and-up to disk, so
// trace costs them nothing) or go through `throttled_log!`. Only sites bounded
// by our own code - once per conn, once per endpoint - may log unthrottled at
// debug and above.
use crate::log_throttle::throttled_log;

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
                log::trace!("sending output data, conv {}: {:?}", conv, kcp_packet);
                match output_sender.try_send(kcp_packet) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // Dropped here = self-inflicted loss KCP will re-pay with a
                        // retransmit; with the enlarged channel this should not happen.
                        // Throttled: this callback runs per packet inside ikcp_flush
                        // under the kcp lock, and a stalled consumer would otherwise
                        // write hundreds of lines per second from under it.
                        throttled_log!(
                            warn,
                            "kcp output channel full, packet dropped, conn: {:?}",
                            conn_id
                        );
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        // Normal during endpoint teardown; also fires per remaining
                        // flush packet, so keep it at trace.
                        log::trace!("kcp output channel closed, conn: {:?}", conn_id);
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
        self.tasks.spawn(async move {
            let mut send_failed = false;
            while let Some(data) = send_receiver.recv().await {
                let data = data.freeze();
                let max_send = kcp.lock().max_chunk_size();

                for chunk in data.chunks(max_send) {
                    loop {
                        // Probe and send under one guard; per-chunk this loop used
                        // to take the kcp mutex four times, serializing against
                        // the 10ms updater on the hot path for no benefit.
                        let sent = {
                            let mut kcp = kcp.lock();
                            if kcp.waitsnd() > 2 * kcp.sendwnd() {
                                None
                            } else {
                                Some(kcp.send(chunk))
                            }
                        };
                        match sent {
                            // flow control wait
                            None => {
                                inner
                                    .waiting_new_send_window
                                    .store(true, std::sync::atomic::Ordering::SeqCst);
                                inner.send_notifier.notified().await;
                            }
                            Some(Ok(n)) if n == chunk.len() => break,
                            Some(ret) => {
                                // A failed (or short) ikcp_send leaves a hole in the
                                // byte stream; consuming further messages would splice
                                // later bytes after the gap and silently desynchronize
                                // the peer's framed stream. Stop consuming instead: the
                                // waitsnd drain below still runs, so the peer sees a
                                // clean prefix and then FIN.
                                log::error!(
                                    "send data failed: {:?}, len: {}, closing conn: {:?}",
                                    ret,
                                    chunk.len(),
                                    conn_id
                                );
                                send_failed = true;
                                break;
                            }
                        }
                    }
                    if send_failed {
                        break;
                    }
                }

                // One flush per message, not per chunk: the updater's own cadence
                // already moves mid-message chunks, and the end-of-message flush
                // bounds delivery latency.
                kcp.lock().flush();
                inner.update_notifier.notify_one();

                if send_failed {
                    break;
                }
            }

            log::debug!(
                "connection packet sender close, waiting for waitsnd to be 0, conn: {:?}",
                conn_id
            );

            // waiting for waitsnd to be 0
            while kcp.lock().waitsnd() > 0 {
                inner
                    .waiting_new_send_window
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                inner.send_notifier.notified().await;
            }

            send_close_notifier.notify_one();
            log::debug!("connection packet send task done, conn: {:?}", conn_id);
        });

        // handle packet recv
        let kcp = self.kcp.clone();
        let inner = self.inner.clone();
        let conn_id = self.conn_id;
        let Some(recv_sender) = self.recv_sender.take() else {
            log::error!("recv sender is not set");
            return;
        };
        let recv_closed = self.recv_closed.clone();
        self.tasks.spawn(async move {
            let mut buf = BytesMut::new();
            loop {
                let peeksize = kcp.lock().peeksize();
                if peeksize < 0 {
                    // Only an empty queue lets a peer close end the task: the
                    // peer's FIN goes out after everything it sent was ACKed,
                    // i.e. that data already sits in rcv_queue, and exiting on
                    // the flag alone would truncate the tail of the stream
                    // whenever the reader is slower than the network.
                    if recv_closed.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    log::trace!("recv nothing, wait for next update");
                    inner.recv_notifier.notified().await;
                    continue;
                };

                if buf.capacity() < std::cmp::max(peeksize as usize, 1) {
                    buf.reserve(std::cmp::max(peeksize as usize, 4096));
                }
                // Bind first: an `if let` scrutinee's lock guard would live across
                // the yield await below.
                let recv_ret = kcp.lock().recv(&mut buf);
                if let Err(e) = recv_ret {
                    // Throttled: the retry below yields rather than parking, so a
                    // persistently failing ikcp_recv would otherwise write this line
                    // as fast as the scheduler can turn the loop over.
                    throttled_log!(error, "recv data failed: {:?}, conn: {:?}", e, conn_id);
                    // Every known recv error self-heals on retry (-3 re-reads
                    // peeksize and grows the buffer above), but a persistent one
                    // must not busy-spin the worker. Yield instead of parking on
                    // the notifier: data is already queued, so no new input may
                    // ever arrive to fire it.
                    tokio::task::yield_now().await;
                    continue;
                }
                log::trace!("recv data ({}): {:?}", buf.len(), buf);
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

            log::debug!("connection packet recv task done, conn: {:?}", conn_id);
        });
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
        //
        // 4096 budgets the default turbo window with 4x headroom. It is a cap that a
        // custom kcp_config_factory must respect: the configured send windows - summed
        // over every conn sharing this endpoint - have to stay comfortably under it, or
        // synchronized flush bursts shed the excess as self-inflicted loss right at
        // peak throughput.
        let (input_sender, input_receiver) = tokio::sync::mpsc::channel(4096);
        let (output_sender, output_receiver) = tokio::sync::mpsc::channel(4096);
        // 64 pending accepts: ConnId is 12 bytes, so the headroom is free, and a
        // handshake burst between accept() polls should not hit the reset path in
        // the packet loop.
        let (new_conn_sender, new_conn_receiver) = tokio::sync::mpsc::channel(64);

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

            log::trace!("sending pong packet: {:?}", out_packet);
            let ret = output_sender.send(out_packet).await;
            if let Err(e) = ret {
                // A blocking send only errors on a closed channel, i.e. endpoint
                // teardown - and this runs once per incoming ping, a rate the peer
                // sets. Trace, not error.
                log::trace!("send pong packet failed: {:?}", e);
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

        self.tasks.spawn(async move {
            while let Some(packet) = input_receiver.recv().await {
                log::trace!("recv packet: {:?}", packet);
                if Self::try_handle_pingpong(&data, &packet, &output_sender).await {
                    continue;
                }

                let conv = ConnId::from(&packet);
                if packet.header().is_data() && !packet.payload().is_empty() {
                    if let Some(mut conn) = data.conn_map.get_mut(&conv) {
                        if let Err(e) = conn.handle_input(&packet) {
                            // Malformed input arrives at the peer's rate; throttle.
                            throttled_log!(
                                warn,
                                "handle input on connection failed, last: {:?}, conv: {:?}",
                                e,
                                conv
                            );
                        } else {
                            log::trace!("handle input on connection done, conv: {:?}", conv);
                        }
                    } else {
                        // Fires per packet (e.g. retransmits racing conn teardown
                        // or arriving before accept()), so trace, not debug.
                        log::trace!(
                            "no conn for conv when handling data packet, conv: {:?}, packet: {:?}",
                            conv,
                            packet
                        );
                    }
                }

                let mut state_ref = data.state_map.get_mut(&conv);
                let state = state_ref.as_deref_mut();
                let mut out_packet: Option<KcpPacket> = None;
                // Decisions recorded under the state guard, acted on after it drops:
                // conn_map must never be locked while a state_map guard is held, or
                // this loop deadlocks against any task nesting the other way (see
                // the lock-order note on the clean task below).
                let mut peer_closed = false;
                let mut closed = false;
                let mut peer_closed_now = false;
                let mut closed_now = false;
                let mut established_now = false;
                let mut from_syn_received = false;
                if let Some(state) = state {
                    let prev_established = state.is_established();
                    let prev_peer_closed = state.is_peer_closed();
                    let prev_closed = state.is_closed();
                    from_syn_received = matches!(state.fsm, KcpConnectionFSM::SynReceived);
                    let ret = state.handle_packet(&packet);
                    log::trace!(
                        "handle packet for conn, conv: {:?}, state: {:?}, ret: {:?}",
                        conv,
                        state,
                        ret
                    );
                    if let Ok(pkt) = ret {
                        out_packet = pkt;
                    }

                    established_now = !prev_established && state.is_established();
                    peer_closed = state.is_peer_closed();
                    closed = state.is_closed();
                    peer_closed_now = !prev_peer_closed && peer_closed;
                    closed_now = !prev_closed && closed;
                } else {
                    if packet.header().is_rst() {
                        // Per stray packet, peer-controlled rate: trace.
                        log::trace!("reset packet for conn, but no state, conv: {:?}", conv);
                        continue;
                    }
                    let mut tmp_fsm = KcpConnectionFSM::listen();
                    let res = tmp_fsm.handle_packet(&packet, &mut out_packet);
                    log::trace!(
                        "handle first packet for conn, conv: {:?}, out: {:?}, ret: {:?}",
                        conv,
                        out_packet,
                        res
                    );
                    if res.is_ok() {
                        let mut conn_state = KcpConnectionState::new(tmp_fsm);
                        conn_state.set_data(packet.payload().to_vec().into());
                        data.state_map.insert(conv, conn_state);
                    }
                }

                drop(state_ref);

                if established_now && new_conn_sender.try_send(conv).is_err() && from_syn_received {
                    // The Established transition fires exactly once; on a full
                    // accept backlog it used to be silently discarded, stranding
                    // a live conn no accept() can ever return while the peer
                    // sends into it forever (its pings keep the reaper away).
                    // Refuse it visibly instead. Only for server-side conns: a
                    // client's own connect() is delivered through its notify, and
                    // its channel entry going unconsumed is the normal case.
                    // Throttled: a peer opening handshakes faster than accept()
                    // consumes them drives this line.
                    throttled_log!(warn, "accept backlog full, resetting conn: {:?}", conv);
                    data.state_map.remove(&conv);
                    let mut rst = KcpPacket::new(0);
                    rst.mut_header().set_rst(true);
                    conv.fill_packet_header(&mut rst);
                    if let Err(e) = output_sender.send(rst).await {
                        // Teardown-only, same peer-driven rate. Trace.
                        log::trace!("send reset packet failed: {:?}, conv: {:?}", e, conv);
                    }
                    continue;
                }

                if peer_closed {
                    if peer_closed_now {
                        log::debug!("peer half closed, close recv, conv: {:?}", conv);
                    }
                    if let Some(conn) = data.conn_map.get_mut(&conv) {
                        conn.close_recv()
                    }
                }

                if closed {
                    // state map will be cleaned by periodic task.
                    // Log the close cause at info - session-death reasons are the
                    // first thing needed when users report drops - but only on the
                    // transition: until the <=10s reap, every retransmitted packet
                    // reaching the Closed state would re-log the same line.
                    // Throttled on top of that: how many conns a peer opens and
                    // tears down is the peer's choice.
                    if closed_now {
                        throttled_log!(
                            info,
                            "kcp conn closed by peer packet (rst: {}, fin: {}), conv: {:?}",
                            packet.header().is_rst(),
                            packet.header().is_fin(),
                            conv
                        );
                    }
                    data.conn_map.remove(&conv);
                }

                if let Some(mut out_packet) = out_packet {
                    conv.fill_packet_header(&mut out_packet);
                    log::trace!("sending output packet, conv: {:?}: {:?}", conv, out_packet);
                    let ret = output_sender.send(out_packet).await;
                    if let Err(e) = ret {
                        // Teardown-only (blocking send), once per FSM response, so at
                        // the peer's packet rate. Trace.
                        log::trace!("send output packet failed: {:?}", e);
                    }
                }
            }
        });

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
                    // session can end; name the reason for field diagnosis. Throttled:
                    // a peer that opens many conns and walks away has them all reaped
                    // in one sweep.
                    throttled_log!(
                        info,
                        "kcp conn reaped by pong timeout ({:?} since last packet), conv: {:?}",
                        since,
                        conn_id
                    );
                }
                // Lock order: never touch one map while holding a guard on the other.
                // retain() runs its predicate under the conn_map shard write lock, and
                // probing state_map from in there formed an ABBA deadlock with the
                // packet loop (which held a state guard, then took conn_map locks).
                // Collect the keys first, then check and remove without nesting.
                let conn_ids: Vec<ConnId> = data.conn_map.iter().map(|item| *item.key()).collect();
                for conn_id in conn_ids {
                    if !data.state_map.contains_key(&conn_id) {
                        data.conn_map.remove(&conn_id);
                    }
                }
                data.state_map.shrink_to_fit();
                data.conn_map.shrink_to_fit();
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });

        // conn liveness task: pings established conns, retransmits the SYN-ACK
        // for conns stuck in SynReceived and the FIN for conns in LocalClosed.
        // Scheduled so that no backlog of either kind can stretch the cadence:
        // - pings are sharded by conv into PING_SLOTS slots and each 1s tick
        //   serves one slot, so every conn is pinged about every PING_SLOTS
        //   seconds and per-tick work stays ~N/PING_SLOTS packets (LocalClosed
        //   conns re-send their FIN in their slot instead of a ping);
        // - SYN-ACK retransmits run every tick, budget-capped per conn, the
        //   budget consumed only when the output channel accepts the packet;
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
                            // Consume retransmit budget only when the output channel
                            // actually accepts the packet: burning it at scan time
                            // would let ~10 congested ticks spend every capped
                            // retransmit without one reaching the wire, and this
                            // timer is the only recovery left once the client is
                            // Established. This task is the only writer, so a plain
                            // load/store suffices.
                            let spent = state
                                .syn_ack_retries
                                .load(std::sync::atomic::Ordering::Relaxed);
                            if spent < MAX_SYN_ACK_RETRIES {
                                let mut out_packet = KcpPacket::new(0);
                                out_packet.mut_header().set_syn(true);
                                out_packet.mut_header().set_ack(true);
                                conn_id.fill_packet_header(&mut out_packet);
                                if output_sender.try_send(out_packet).is_ok() {
                                    state
                                        .syn_ack_retries
                                        .store(spent + 1, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        }
                        // Never ping a handshaking conn: connect()'s SYN retransmit
                        // loop owns client-side liveness, and pinging a SynSent conn
                        // makes a server that never saw the SYN answer RST, killing
                        // that loop after a single SYN.
                        KcpConnectionFSM::Established | KcpConnectionFSM::PeerClosed
                            if conn_id.conv % PING_SLOTS == slot =>
                        {
                            let mut out_packet = KcpPacket::new(0);
                            out_packet.mut_header().set_ping(true);
                            conn_id.fill_packet_header(&mut out_packet);
                            packets.push(out_packet);
                        }
                        // The close watcher emits the FIN exactly once and nothing
                        // below this layer retransmits it, so a single lost FIN
                        // datagram would leave the peer half-open forever: both sides
                        // keep answering pings, the pong reaper never fires, and the
                        // peer's reader blocks indefinitely. Re-emit the FIN on the
                        // ping cadence until the peer's FIN/RST moves us to Closed -
                        // a duplicate FIN is idempotent in every peer state, and it
                        // refreshes the peer's pong timer just like a ping would.
                        KcpConnectionFSM::LocalClosed if conn_id.conv % PING_SLOTS == slot => {
                            let mut out_packet = KcpPacket::new(0);
                            out_packet.mut_header().set_fin(true);
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
        // NOTE: callers must not hold a state_map guard across this call - the
        // failure path below takes the same shard's write lock.
        let mut conn = match KcpConnection::new(conn_id, config) {
            Ok(conn) => conn,
            Err(e) => {
                // A rejected config must not leave a phantom Established entry:
                // liveness would ping it forever, the peer's pongs keep the
                // reaper away, and the peer's data is silently discarded while
                // the accept notification is already consumed. Remove the state
                // and tell the peer to tear down instead of waiting.
                self.data.state_map.remove(&conn_id);
                let mut rst = KcpPacket::new(0);
                rst.mut_header().set_rst(true);
                conn_id.fill_packet_header(&mut rst);
                let _ = self.output_sender.try_send(rst);
                return Err(e);
            }
        };
        conn.run(self.output_sender.clone());

        let data = self.data.clone();
        let close_notifier = conn.send_close_notifier();

        data.conn_map.insert(conn_id, conn);

        // Callers release their state_map guard before calling us (our failure paths
        // take that shard's write lock), which opens a window: a peer FIN/RST landing
        // between the caller's Established check and the insert above is processed by
        // the packet loop while conn_map has nothing to act on, so its close is lost -
        // a missed FIN leaves this receiver open forever (PeerClosed is not reaped and
        // the peer's pongs keep the pong timer fresh), a missed RST hands back a conn
        // that is already dead. Re-apply the close state now that the conn is
        // installed; anything after this point the packet loop sees for itself.
        let state = data.state_map.get(&conn_id);
        let (peer_closed, closed) = state
            .as_ref()
            .map(|state| (state.is_peer_closed(), state.is_closed()))
            // No state at all: reaped, or refused by the accept-backlog path.
            .unwrap_or((true, true));
        // Lock order: never hold a guard on one map while touching the other.
        drop(state);

        if closed {
            data.conn_map.remove(&conn_id);
            return Err(Error::ConnectioinReset);
        }
        if peer_closed {
            if let Some(conn) = data.conn_map.get_mut(&conn_id) {
                conn.close_recv();
            }
        }

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

            log::debug!(
                "connection close watcher done, conn: {:?}, state: {:?}",
                conn_id,
                cur_state
            );
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

        log::trace!("connect packet, conn: {:?}: {:?}", conn_id, out_packet);

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
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match timeout(deadline - now, self.output_sender.send(out_packet.clone())).await {
                Ok(ret) => ret.with_context(|| "send connect packet failed")?,
                // Deadline hit while backpressured: fall out to the Established
                // re-check below instead of blocking here and emitting a stale SYN.
                Err(_) => break,
            }

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
            // The deadline can land in the gap after the peer's SYN-ACK advanced the
            // FSM but before this task polled `notified` again. The state is the
            // truth: removing an Established entry here would return a spurious
            // timeout for a handshake the server has already accepted (and answered),
            // stranding an orphaned conn on its side.
            let established = self
                .data
                .state_map
                .get(&conn_id)
                .map(|state| matches!(state.fsm, KcpConnectionFSM::Established))
                .unwrap_or(false);
            if !established {
                self.data.state_map.remove(&conn_id);
                return Err(Error::ConnectTimeout);
            }
        }

        if let Some(state) = self.data.state_map.get(&conn_id) {
            log::debug!(
                "connect done, checking state, conn: {:?}, state: {:?}",
                conn_id,
                *state
            );
            let established = matches!(state.fsm, KcpConnectionFSM::Established);
            // add_conn's failure path removes from state_map; the read guard
            // must be gone first or the same-shard write would self-deadlock.
            drop(state);
            if established {
                self.add_conn(conn_id)?;
                return Ok(conn_id);
            } else {
                self.data.state_map.remove(&conn_id);
            }
            // if task aborted, the state map will be cleaned by periodic task
        }

        Err(anyhow::anyhow!("connect failed").into())
    }

    pub async fn accept(&self) -> Result<ConnId, Error> {
        let conn_receiver = self.new_conn_receiver.clone();

        loop {
            let Some(conn_id) = conn_receiver.lock().await.recv().await else {
                return Err(Error::Shutdown);
            };

            let Some(state) = self.data.state_map.get(&conn_id) else {
                log::debug!("no state for conn, ignore, conn: {:?}", conn_id);
                continue;
            };
            let established = matches!(state.fsm, KcpConnectionFSM::Established);
            // add_conn's failure path removes from state_map; the read guard
            // must be gone first or the same-shard write would self-deadlock.
            drop(state);

            if established {
                match self.add_conn(conn_id) {
                    Ok(()) => return Ok(conn_id),
                    // Raced with a peer close: that conn is gone, so wait for the
                    // next one rather than killing the caller's accept loop. Every
                    // other error is deterministic (a config the factory keeps
                    // producing) and must fail-stop, not spin.
                    Err(Error::ConnectioinReset) => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn bad_kcp_config_factory() -> KcpConfigFactory {
        Box::new(|conv| {
            let mut c = KcpConfig::new_turbo(conv);
            c.mtu = Some(1); // rejected by ikcp_setmtu (< 50)
            c
        })
    }

    // Drives add_conn against a state that already moved on, which is what a peer
    // FIN/RST landing between the caller's Established check and conn_map insertion
    // produces: the packet loop finds no conn to act on, so add_conn must re-apply
    // the close itself.
    async fn established_pair() -> (KcpEndpoint, KcpEndpoint, ConnId, JoinSet<()>) {
        let (client_endpoint, server_endpoint, t) = prepare_test().await;
        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(5), 1, 3, Bytes::new()),
            server_endpoint.accept()
        );
        let conv = connect_ret.unwrap();
        assert_eq!(conv, accept_ret.unwrap());
        (client_endpoint, server_endpoint, conv, t)
    }

    #[tokio::test]
    async fn test_peer_close_racing_conn_setup_is_not_lost() {
        let (client_endpoint, server_endpoint, conv, t) = established_pair().await;

        // FIN processed while conn_map was still empty.
        client_endpoint.data.state_map.get_mut(&conv).unwrap().fsm = KcpConnectionFSM::PeerClosed;
        client_endpoint
            .add_conn(conv)
            .expect("half-close is not a setup failure");
        let (_s, mut receiver) = client_endpoint.conn_sender_receiver(conv).unwrap();
        let eof = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("a FIN lost in the setup window leaves the receiver open forever");
        assert!(eof.is_none(), "receiver must end after the missed FIN");

        // RST processed while conn_map was still empty.
        client_endpoint.data.state_map.get_mut(&conv).unwrap().fsm = KcpConnectionFSM::Closed;
        assert!(
            matches!(client_endpoint.add_conn(conv), Err(Error::ConnectioinReset)),
            "a conn closed during setup must not be handed back as live"
        );
        assert!(!client_endpoint.data.conn_map.contains_key(&conv));

        drop(client_endpoint);
        drop(server_endpoint);
        t.join_all().await;
    }

    #[tokio::test]
    async fn test_invalid_config_on_connect_cleans_up_and_resets_peer() {
        // The handshake reaches Established, then KcpConnection::new rejects the
        // config. connect must surface the error, leave no phantom state behind,
        // and reset the peer instead of letting it hold a half-dead session.
        let mut client_endpoint = KcpEndpoint::new();
        let mut server_endpoint = KcpEndpoint::new();
        client_endpoint.set_kcp_config_factory(bad_kcp_config_factory());

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
        let (rst_tx, mut rst_rx) = tokio::sync::mpsc::channel::<()>(1);
        let server_input_sender = server_endpoint.input_sender();
        let mut client_output_receiver = client_endpoint.output_receiver().unwrap();
        t.spawn(async move {
            while let Some(packet) = client_output_receiver.recv().await {
                if packet.header().is_rst() {
                    let _ = rst_tx.try_send(());
                }
                let _ = server_input_sender.send(packet).await;
            }
        });

        let connect_ret = client_endpoint
            .connect(std::time::Duration::from_secs(5), 1, 3, Bytes::from("conn"))
            .await;
        assert!(
            connect_ret.is_err(),
            "connect must surface the config error"
        );

        // No phantom conn on the failing side.
        assert!(client_endpoint.data.state_map.is_empty());
        assert!(client_endpoint.data.conn_map.is_empty());

        // The peer must be told to tear down.
        tokio::time::timeout(std::time::Duration::from_secs(5), rst_rx.recv())
            .await
            .expect("client must emit a RST after the config failure");

        drop(client_endpoint);
        drop(server_endpoint);
        t.join_all().await;
    }

    #[tokio::test]
    async fn test_invalid_config_on_accept_cleans_up_and_resets_peer() {
        // Mirror case: the accepting side's factory is broken. accept must fail
        // (a broken factory is deterministic - fail-stop, not retry), leave no
        // state behind, and the already-established client must be torn down.
        let mut client_endpoint = KcpEndpoint::new();
        let mut server_endpoint = KcpEndpoint::new();
        server_endpoint.set_kcp_config_factory(bad_kcp_config_factory());

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
            client_endpoint.connect(std::time::Duration::from_secs(5), 1, 3, Bytes::from("conn")),
            tokio::time::timeout(std::time::Duration::from_secs(10), server_endpoint.accept())
        );

        let conv = connect_ret.expect("client side is healthy");
        assert!(
            accept_ret.expect("accept must not hang").is_err(),
            "accept must surface the config error"
        );
        assert!(server_endpoint.data.state_map.is_empty());
        assert!(server_endpoint.data.conn_map.is_empty());

        // The server's RST must tear down the client's established conn.
        let (_client_sender, mut client_receiver) =
            client_endpoint.conn_sender_receiver(conv).unwrap();
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), client_receiver.recv())
            .await
            .expect("client must observe the reset");
        assert!(end.is_none(), "client receiver must end after the RST");

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

    #[tokio::test(start_paused = true)]
    async fn test_kcp_lost_fin_is_retransmitted() {
        // Drop the client's first FIN. The close watcher sends it exactly once, so
        // only the liveness task's FIN retransmit can tell the server the stream
        // ended; without it the server's reader blocks forever while pings keep
        // both pong timers fresh on each side.
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
            let mut fin_dropped = false;
            while let Some(packet) = client_output_receiver.recv().await {
                if packet.header().is_fin() && !fin_dropped {
                    fin_dropped = true;
                    continue;
                }
                let _ = server_input_sender.send(packet).await;
            }
        });

        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(5), 1, 3, Bytes::new()),
            tokio::time::timeout(std::time::Duration::from_secs(30), server_endpoint.accept())
        );
        let conv = connect_ret.unwrap();
        assert_eq!(conv, accept_ret.expect("accept must not hang").unwrap());

        let (client_sender, _cr) = client_endpoint.conn_sender_receiver(conv).unwrap();
        let (_ss, mut server_receiver) = server_endpoint.conn_sender_receiver(conv).unwrap();

        client_sender.send(BytesMut::from("hello")).await.unwrap();
        let data = server_receiver.recv().await.unwrap();
        assert_eq!("hello", String::from_utf8_lossy(&data));

        drop(client_sender);
        // The reader must see end-of-stream via the retransmitted FIN (ping-slot
        // cadence, at most PING_SLOTS + 1 virtual seconds away), not hang forever.
        let eof = tokio::time::timeout(std::time::Duration::from_secs(30), server_receiver.recv())
            .await
            .expect("lost FIN was never retransmitted; reader hung");
        assert!(eof.is_none(), "expected end-of-stream after close");

        drop(client_endpoint);
        drop(server_endpoint);
        t.join_all().await;
    }

    #[tokio::test]
    async fn test_kcp_graceful_close_delivers_tail() {
        // The peer's FIN goes out only after everything it sent was ACKed, i.e. the
        // data already sits in our rcv_queue when the close is processed. A reader
        // slower than the network must still get every byte before end-of-stream,
        // not a truncated tail.
        let (client_endpoint, server_endpoint, t) = prepare_test().await;

        let (connect_ret, accept_ret) = tokio::join!(
            client_endpoint.connect(std::time::Duration::from_secs(1), 1, 3, Bytes::new()),
            tokio::time::timeout(std::time::Duration::from_secs(30), server_endpoint.accept())
        );
        let conv = connect_ret.unwrap();
        assert_eq!(conv, accept_ret.expect("accept must not hang").unwrap());

        let (client_sender, _cr) = client_endpoint.conn_sender_receiver(conv).unwrap();
        let (_ss, mut server_receiver) = server_endpoint.conn_sender_receiver(conv).unwrap();

        // Fill well past the recv channel's buffering while the reader is idle, then
        // close; the FIN is processed long before the reader starts draining.
        let payload: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        for chunk in payload.chunks(64 * 1024) {
            client_sender.send(BytesMut::from(chunk)).await.unwrap();
        }
        drop(client_sender);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let mut got = Vec::with_capacity(payload.len());
        while let Some(data) =
            tokio::time::timeout(std::time::Duration::from_secs(10), server_receiver.recv())
                .await
                .expect("reader starved waiting for the close tail")
        {
            got.extend_from_slice(&data);
        }
        assert_eq!(
            got.len(),
            payload.len(),
            "graceful close truncated the tail"
        );
        assert!(got == payload, "tail bytes corrupted");

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
