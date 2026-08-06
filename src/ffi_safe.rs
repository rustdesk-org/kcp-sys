use crate::{error::Error, ffi::*};
use std::time::Instant;

use bytes::BytesMut;

const MTU_SIZE: i32 = 1200;

#[derive(Debug, Clone, Copy)]
pub struct KcpConfig {
    pub conv: IUINT32,
    pub mtu: Option<i32>,

    pub sndwnd: Option<i32>,
    pub rcvwnd: Option<i32>,

    pub nodelay: Option<i32>,
    pub interval: Option<i32>,
    pub resend: Option<i32>,
    pub nc: Option<i32>,
}

impl KcpConfig {
    pub fn new(conv: IUINT32) -> Self {
        Self {
            conv,
            mtu: None,
            sndwnd: None,
            rcvwnd: None,
            nodelay: None,
            interval: None,
            resend: None,
            nc: None,
        }
    }

    pub fn new_turbo(conv: IUINT32) -> Self {
        Self {
            conv,
            mtu: Some(MTU_SIZE),
            sndwnd: Some(1024),
            rcvwnd: Some(1024),
            nodelay: Some(1),
            interval: Some(10),
            resend: Some(2),
            nc: Some(1),
        }
    }
}

pub type OutputCb = Box<dyn Fn(u32, BytesMut) -> Result<(), Error>>;

pub struct Kcp {
    kcp: *mut ikcpcb,
    config: KcpConfig,
    now: Instant,
    output_cb: Option<OutputCb>,

    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

unsafe impl Send for Kcp {}

unsafe extern "C" fn ikcp_output(
    buf: *const ::std::os::raw::c_char,
    len: ::std::os::raw::c_int,
    kcp: *mut ikcpcb,
    this: *mut ::std::os::raw::c_void,
) -> i32 {
    // convert this to KcpConnection. A shared reborrow, not `&mut`: the flush()/update()
    // caller already holds `&mut Kcp` through the mutex guard, so a second live `&mut`
    // to the same object would be aliasing UB; `handle_output_callback` only needs `&self`.
    let kcp_connection = &*(this as *const Kcp);
    if kcp_connection.kcp != kcp {
        // Defensive: a mismatched callback context means the packet cannot be routed;
        // drop it instead of aborting the process (panic = abort in release builds).
        // Throttled: this runs per packet from inside ikcp_flush, so if the invariant
        // ever did break it would write a line per outgoing datagram.
        crate::log_throttle::throttled_log!(error, "kcp output callback context mismatch");
        return 0;
    }

    let buf = BytesMut::from(std::slice::from_raw_parts(buf as *const u8, len as usize));

    // TODO: handle output error
    let _ = kcp_connection.handle_output_callback(buf);

    // kcp doesn't care about the return value
    0
}

impl Kcp {
    pub fn new(config: KcpConfig) -> Result<Box<Self>, Error> {
        unsafe {
            let conv = config.conv;
            let mut ret = Box::new(Self {
                kcp: std::ptr::null_mut(),
                config,
                now: Instant::now(),
                output_cb: None,
                _marker: core::marker::PhantomData,
            });

            let kcp = ikcp_create(conv, &mut *ret as *mut Kcp as *mut ::std::os::raw::c_void);
            if kcp.is_null() {
                return Err(Error::CreateConnectionFailed);
            }

            (*kcp).stream = 1;

            ret.kcp = kcp;

            ikcp_setoutput(kcp, Some(ikcp_output));

            ret.apply_config()?;

            Ok(ret)
        }
    }

    pub fn set_output_cb(&mut self, output_cb: OutputCb) {
        self.output_cb = Some(output_cb);
    }

    /// Installs a KCP 2.0 congestion control implementation.
    ///
    /// # Safety
    ///
    /// KCP stores the provided operations pointer and calls it from C. The caller must ensure the
    /// `IKCPOPS` value outlives every `Kcp` using it, and that its callbacks preserve KCP's aliasing
    /// and thread-safety requirements.
    pub unsafe fn set_congestion_control(
        &mut self,
        ops: Option<&'static IKCPOPS>,
    ) -> Result<(), Error> {
        let ops = ops.map_or(std::ptr::null(), |ops| ops as *const IKCPOPS);
        let ret = unsafe { ikcp_setcc(self.kcp, ops) };
        if ret < 0 {
            Err(anyhow::anyhow!("setcc failed, return: {}", ret).into())
        } else {
            Ok(())
        }
    }

    pub fn reset_congestion_control(&mut self) -> Result<(), Error> {
        let ret = unsafe { ikcp_setcc(self.kcp, std::ptr::null()) };
        if ret < 0 {
            Err(anyhow::anyhow!("reset setcc failed, return: {}", ret).into())
        } else {
            Ok(())
        }
    }

    pub fn handle_input(&mut self, data: &[u8]) -> Result<(), Error> {
        let ret = unsafe { ikcp_input(self.kcp, data.as_ptr() as *const _, data.len() as _) };
        if ret < 0 {
            Err(anyhow::anyhow!("input failed, return: {}", ret).into())
        } else {
            Ok(())
        }
    }

    pub fn update(&mut self) {
        unsafe {
            ikcp_update(self.kcp, self.now.elapsed().as_millis() as IUINT32);
        }
    }

    pub fn next_update_delay_ms(&mut self) -> IUINT32 {
        let current = self.now.elapsed().as_millis() as IUINT32;
        let next = unsafe { ikcp_check(self.kcp, current) };
        // KCP timestamps are modular u32; ikcp_check returns `current + minimal`, which wraps
        // below `current` once elapsed-ms crosses the u32 boundary (~49.7 days uptime). A plain
        // subtraction would panic there under overflow checks; wrapping_sub yields the correct
        // delay in every build mode.
        next.wrapping_sub(current)
    }

    pub fn send(&mut self, data: &[u8]) -> Result<usize, Error> {
        let ret = unsafe { ikcp_send(self.kcp, data.as_ptr() as *const _, data.len() as _) };
        if ret < 0 {
            Err(anyhow::anyhow!("send failed, return: {}", ret).into())
        } else {
            Ok(ret as usize)
        }
    }

    pub fn flush(&mut self) {
        unsafe {
            ikcp_flush(self.kcp);
        }
    }

    pub fn peeksize(&self) -> i32 {
        unsafe { ikcp_peeksize(self.kcp) }
    }

    pub fn recv(&mut self, buf: &mut BytesMut) -> Result<(), Error> {
        let ret = unsafe { ikcp_recv(self.kcp, buf.as_mut_ptr() as *mut _, buf.capacity() as _) };
        if ret < 0 {
            Err(anyhow::anyhow!("recv failed, return: {}", ret).into())
        } else {
            unsafe {
                buf.set_len(ret as usize);
            }
            Ok(())
        }
    }

    pub fn waitsnd(&self) -> i32 {
        unsafe { ikcp_waitsnd(self.kcp) }
    }

    pub fn sendwnd(&self) -> i32 {
        // KCP's actual window (IKCP_WND_SND = 32 when unset), not the raw config
        // value: ikcp_wndsize silently ignores non-positive values and keeps its
        // default, so echoing the config would let sndwnd Some(-1)/Some(0) make
        // the flow-control comparison `waitsnd() > 2 * sendwnd()` permanently
        // true and stall sending forever.
        unsafe { (*self.kcp).snd_wnd as i32 }
    }

    fn handle_output_callback(&self, buf: BytesMut) -> Result<(), Error> {
        if let Some(output_cb) = self.output_cb.as_ref() {
            output_cb(self.config.conv, buf)
        } else {
            Err(anyhow::anyhow!("no output callback set").into())
        }
    }

    pub fn max_chunk_size(&self) -> usize {
        // https://github.com/skywind3000/kcp/blob/f4f3a89cc632647dabdcb146932d2afd5591e62e/ikcp.c#L512
        // https://github.com/skywind3000/kcp/issues/428
        const IKCP_WND_RCV: u32 = 128; // kcp uses IKCP_WND_RCV -1 as the max count of mss
        const MIN_CHUNK_SIZE: usize = 1024;
        let mss = unsafe { (*self.kcp).mss };
        let sndwnd = unsafe { (*self.kcp).snd_wnd };
        let rcvwnd = unsafe { (*self.kcp).rcv_wnd };
        let max_count = std::cmp::min(IKCP_WND_RCV - 1, std::cmp::min(sndwnd, rcvwnd));
        let truncate_size = (max_count as usize) * (mss as usize);
        truncate_size.max(MIN_CHUNK_SIZE)
    }

    fn apply_config(&mut self) -> Result<(), Error> {
        unsafe {
            let ret = ikcp_setmtu(self.kcp, self.config.mtu.unwrap_or(MTU_SIZE));
            if ret < 0 {
                return Err(anyhow::anyhow!("setmtu failed, return: {}", ret).into());
            }

            let ret = ikcp_wndsize(
                self.kcp,
                self.config.sndwnd.unwrap_or(-1),
                self.config.rcvwnd.unwrap_or(-1),
            );
            if ret < 0 {
                return Err(anyhow::anyhow!("wndsize failed, return: {}", ret).into());
            }

            let ret = ikcp_nodelay(
                self.kcp,
                self.config.nodelay.unwrap_or(-1),
                self.config.interval.unwrap_or(-1),
                self.config.resend.unwrap_or(-1),
                self.config.nc.unwrap_or(-1),
            );
            if ret < 0 {
                return Err(anyhow::anyhow!("nodelay failed, return: {}", ret).into());
            }

            if let Some(interval) = self.config.interval {
                if interval > 0 {
                    (*self.kcp).interval = interval as _;
                }
            }
        }

        Ok(())
    }
}

impl Drop for Kcp {
    fn drop(&mut self) {
        unsafe {
            ikcp_release(self.kcp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_congestion_control_uses_builtin_algorithm() {
        let mut kcp = Kcp::new(KcpConfig::new(1)).unwrap();

        kcp.reset_congestion_control().unwrap();
    }

    #[test]
    fn sendwnd_reports_the_effective_window() {
        // ikcp_wndsize accepts non-positive values as "keep the default", so a
        // factory config of Some(-1)/Some(0) passes apply_config while KCP runs
        // on IKCP_WND_SND = 32. sendwnd() must report what KCP actually uses:
        // echoing the raw config made `waitsnd() > 2 * sendwnd()` permanently
        // true and stalled sending forever.
        for bad in [Some(-1), Some(0), None] {
            let mut config = KcpConfig::new_turbo(1);
            config.sndwnd = bad;
            let kcp = Kcp::new(config).expect("non-positive sndwnd is accepted");
            assert_eq!(kcp.sendwnd(), 32, "effective default for {bad:?}");
            assert!(
                kcp.waitsnd() <= 2 * kcp.sendwnd(),
                "flow control must be satisfiable on an idle conn ({bad:?})"
            );
        }

        let mut config = KcpConfig::new_turbo(1);
        config.sndwnd = Some(1024);
        assert_eq!(Kcp::new(config).unwrap().sendwnd(), 1024);
    }
}
