use crate::{error::Error, ffi::*};
use std::time::Instant;

use bytes::{Bytes, BytesMut};

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
            mtu: Some(1200),
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
    output_cb: Option<Box<dyn Fn(u32, BytesMut) -> Result<(), Error>>>,

    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

unsafe impl Send for Kcp {}

unsafe extern "C" fn ikcp_output(
    buf: *const ::std::os::raw::c_char,
    len: ::std::os::raw::c_int,
    kcp: *mut ikcpcb,
    this: *mut ::std::os::raw::c_void,
) -> i32 {
    // convert this to KcpConnection
    let kcp_connection = &mut *(this as *mut Kcp);
    assert_eq!(kcp_connection.kcp, kcp);

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

            return Ok(ret);
        }
    }

    pub fn set_output_cb(&mut self, output_cb: OutputCb) {
        self.output_cb = Some(output_cb);
    }

    pub fn handle_input(&mut self, data: &[u8]) -> Result<(), Error> {
        let ret = unsafe { ikcp_input(self.kcp, data.as_ptr() as *const _, data.len() as _) };
        if ret < 0 {
            return Err(anyhow::anyhow!("input failed, return: {}", ret).into());
        } else {
            return Ok(());
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
        next - current
    }

    pub fn send(&mut self, data: Bytes) -> Result<usize, Error> {
        let ret = unsafe { ikcp_send(self.kcp, data.as_ptr() as *const _, data.len() as _) };
        if ret < 0 {
            return Err(anyhow::anyhow!("send failed, return: {}", ret).into());
        } else {
            return Ok(ret as usize);
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
            return Err(anyhow::anyhow!("recv failed, return: {}", ret).into());
        } else {
            unsafe {
                buf.set_len(ret as usize);
            }
            return Ok(());
        }
    }

    pub fn waitsnd(&self) -> i32 {
        unsafe { ikcp_waitsnd(self.kcp) }
    }

    pub fn sendwnd(&self) -> i32 {
        // see IKCP_WND_SND
        self.config.sndwnd.unwrap_or(32)
    }

    fn handle_output_callback(&self, buf: BytesMut) -> Result<(), Error> {
        (self.output_cb.as_ref().unwrap())(self.config.conv, buf)
    }

    fn apply_config(&mut self) -> Result<(), Error> {
        unsafe {
            let ret = ikcp_setmtu(self.kcp, self.config.mtu.unwrap_or(1200));
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

        return Ok(());
    }
}

impl Drop for Kcp {
    fn drop(&mut self) {
        unsafe {
            ikcp_release(self.kcp);
        }
    }
}
