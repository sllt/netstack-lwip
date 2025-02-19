use std::{cmp::min, io, net::SocketAddr, os::raw, pin::Pin, sync::Arc};

use anyhow::Result;
use bytes::BytesMut;
use futures::task::{Context, Poll};
use log::*;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::mpsc::unbounded_channel,
};

use super::lwip::*;
use super::tcp_stream_context::{TcpStreamContext, TcpStreamContextInner};
use super::util;
use super::LWIPMutex;

#[allow(unused_variables)]
pub unsafe extern "C" fn tcp_recv_cb(
    arg: *mut raw::c_void,
    tpcb: *mut tcp_pcb,
    p: *mut pbuf,
    err: err_t,
) -> err_t {
    // SAFETY: tcp_recv_cb is called from tcp_input or sys_check_timeouts only when
    // a data packet or previously refused data is received. Thus lwip_mutex must be locked.
    // See also `<NetStackImpl as AsyncWrite>::poll_write`.
    let ctx = &mut *TcpStreamContext::assume_locked(arg as *const TcpStreamContext);

    if p.is_null() {
        trace!("netstack tcp eof {}", ctx.local_addr);
        ctx.eof = true;
        return err_enum_t_ERR_OK as err_t;
    }

    let pbuflen = (*p).tot_len;
    let buflen = pbuflen as usize;
    let mut buf = Vec::with_capacity(buflen);
    pbuf_copy_partial(p, buf.as_mut_ptr() as _, pbuflen, 0);
    buf.set_len(pbuflen as usize);

    if let Some(Err(err)) = ctx.read_tx.as_ref().map(|tx| tx.send(buf)) {
        // rx is closed
        // call recvd?
        log::trace!("rx is closed");
        return tcp_shutdown(tpcb, 1, 0);
    }

    pbuf_free(p);
    err_enum_t_ERR_OK as err_t
}

#[allow(unused_variables)]
pub extern "C" fn tcp_sent_cb(arg: *mut raw::c_void, tpcb: *mut tcp_pcb, len: u16_t) -> err_t {
    // SAFETY: tcp_sent_cb is called from tcp_input only when
    // an ACK packet is received. Thus lwip_mutex must be locked.
    // See also `<NetStackImpl as AsyncWrite>::poll_write`.
    let ctx = &*unsafe { TcpStreamContext::assume_locked(arg as *const TcpStreamContext) };
    // trace!("netstack tcp sent {}", &ctx.local_addr);
    if let Some(waker) = ctx.write_waker.as_ref() {
        waker.wake_by_ref();
    }
    err_enum_t_ERR_OK as err_t
}

#[allow(unused_variables)]
pub extern "C" fn tcp_err_cb(arg: *mut ::std::os::raw::c_void, err: err_t) {
    // SAFETY: tcp_err_cb is called from
    // tcp_input, tcp_abandon, tcp_abort, tcp_alloc and tcp_new.
    // Thus lwip_mutex must be locked before calling any of these.
    let ctx = &mut *unsafe { TcpStreamContext::assume_locked(arg as *const TcpStreamContext) };
    trace!("netstack tcp err {} {}", err, ctx.local_addr);
    ctx.errored = true;
    let _ = ctx.read_tx.take();
    if let Some(waker) = ctx.write_waker.as_ref() {
        waker.wake_by_ref();
    }
}

#[allow(unused_variables)]
pub extern "C" fn tcp_poll_cb(arg: *mut ::std::os::raw::c_void, tpcb: *mut tcp_pcb) -> err_t {
    let ctx = &*unsafe { TcpStreamContext::assume_locked(arg as *const TcpStreamContext) };
    trace!("netstack tcp poll {}", &ctx.local_addr);
    if let Some(waker) = ctx.write_waker.as_ref() {
        waker.wake_by_ref();
    }
    err_enum_t_ERR_OK as err_t
}

pub struct TcpStreamImpl {
    lwip_mutex: Arc<LWIPMutex>,
    src_addr: SocketAddr,
    dest_addr: SocketAddr,
    pcb: usize,
    write_buf: BytesMut,
    callback_ctx: TcpStreamContext,
}

impl TcpStreamImpl {
    pub fn new(lwip_mutex: Arc<LWIPMutex>, pcb: *mut tcp_pcb) -> Box<Self> {
        unsafe {
            // Since we have no idea how to deal with a full bounded channel upon receiving
            // data from lwIP, an unbounded channel is used instead.
            //
            // Note that lwIP is in charge of flow control. If reader is slower than writer,
            // lwIP will propagate the pressure back by announcing a decreased window size.
            // Thus our unbounded channel will never be overwhelmed. To achieve this, we must
            // call `tcp_recved` when the data from our internal buffer are consumed.
            let (read_tx, read_rx) = unbounded_channel();
            let src_addr = util::to_socket_addr(&(*pcb).remote_ip, (*pcb).remote_port);
            let dest_addr = util::to_socket_addr(&(*pcb).local_ip, (*pcb).local_port);
            let stream = Box::new(TcpStreamImpl {
                lwip_mutex,
                src_addr,
                dest_addr,
                pcb: pcb as usize,
                write_buf: BytesMut::new(),
                callback_ctx: TcpStreamContext::new(src_addr, dest_addr, read_tx, read_rx),
            });
            let arg = &stream.callback_ctx as *const _;
            tcp_arg(pcb, arg as *mut raw::c_void);
            tcp_recv(pcb, Some(tcp_recv_cb));
            tcp_sent(pcb, Some(tcp_sent_cb));
            tcp_err(pcb, Some(tcp_err_cb));
            tcp_poll(pcb, Some(tcp_poll_cb), 8 as _);
            stream.apply_pcb_opts();
            trace!("netstack tcp new {}", stream.local_addr());
            stream
        }
    }

    #[cfg(not(target_os = "ios"))]
    fn apply_pcb_opts(&self) {}

    #[cfg(target_os = "ios")]
    fn apply_pcb_opts(&self) {
        unsafe { (*(self.pcb as *mut tcp_pcb)).so_options |= SOF_KEEPALIVE as u8 };
    }

    pub fn local_addr(&self) -> &SocketAddr {
        &self.src_addr
    }

    pub fn remote_addr(&self) -> &SocketAddr {
        &self.dest_addr
    }
}

fn broken_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe")
}

impl AsyncRead for TcpStreamImpl {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut ReadBuf,
    ) -> Poll<io::Result<()>> {
        let me = &mut *self;
        let guard = me.lwip_mutex.lock();
        let ctx = &mut *me.callback_ctx.with_lock(&guard);
        if ctx.errored {
            return Poll::Ready(Err(broken_pipe()));
        }
        // handle any previously unsent data
        if !me.write_buf.is_empty() {
            let to_read = min(buf.remaining(), me.write_buf.len());
            let piece = me.write_buf.split_to(to_read);
            buf.put_slice(&piece[..to_read]);
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut ctx.read_rx).poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let to_read = min(buf.remaining(), data.len());
                buf.put_slice(&data[..to_read]);
                if to_read < data.len() {
                    me.write_buf.extend_from_slice(&data[to_read..]);
                }
                unsafe { tcp_recved(me.pcb as *mut tcp_pcb, data.len() as u16_t) };
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Err(broken_pipe())),
            Poll::Pending => {
                // no more buffered data
                if ctx.eof {
                    Poll::Ready(Ok(())) // eof
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl Drop for TcpStreamImpl {
    fn drop(&mut self) {
        let guard = self.lwip_mutex.lock();
        let ctx = &*self.callback_ctx.with_lock(&guard);
        trace!("netstack tcp drop {}", &ctx.local_addr);
        if !ctx.errored {
            unsafe {
                tcp_arg(self.pcb as *mut tcp_pcb, std::ptr::null_mut());
                tcp_recv(self.pcb as *mut tcp_pcb, None);
                tcp_sent(self.pcb as *mut tcp_pcb, None);
                tcp_err(self.pcb as *mut tcp_pcb, None);
                tcp_poll(self.pcb as *mut tcp_pcb, None, 0);
                tcp_abort(self.pcb as *mut tcp_pcb);
            }
        }
    }
}

impl AsyncWrite for TcpStreamImpl {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        let guard = self.lwip_mutex.lock();
        let ctx = &mut *self.callback_ctx.with_lock(&guard);
        if ctx.errored {
            return Poll::Ready(Err(broken_pipe()));
        }
        let to_write = unsafe { min(buf.len(), (*(self.pcb as *mut tcp_pcb)).snd_buf as usize) };
        if to_write == 0 {
            ctx.write_waker.replace(cx.waker().clone());
            return Poll::Pending;
        }
        let err = unsafe {
            tcp_write(
                self.pcb as *mut tcp_pcb,
                buf.as_ptr() as *const raw::c_void,
                to_write as u16_t,
                TCP_WRITE_FLAG_COPY as u8,
            )
        };
        if err == err_enum_t_ERR_OK as err_t {
            // Call output in case of mem err?
            let err = unsafe { tcp_output(self.pcb as *mut tcp_pcb) };
            if err == err_enum_t_ERR_OK as err_t {
                Poll::Ready(Ok(to_write))
            } else {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("netstack tcp_output error {}", err),
                )))
            }
        } else if err == err_enum_t_ERR_MEM as err_t {
            // trace!("netstack tcp err_mem on {}", &local_addr);
            ctx.write_waker.replace(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("netstack tcp_write error {}", err),
            )))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        let guard = self.lwip_mutex.lock();
        if self.callback_ctx.with_lock(&guard).errored {
            return Poll::Ready(Err(broken_pipe()));
        }
        let err = unsafe { tcp_output(self.pcb as *mut tcp_pcb) };
        if err != err_enum_t_ERR_OK as err_t {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("netstack tcp_output error {}", err),
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        let guard = self.lwip_mutex.lock();
        let ctx = &*self.callback_ctx.with_lock(&guard);
        if ctx.errored {
            return Poll::Ready(Err(broken_pipe()));
        }
        trace!("netstack tcp shutdown {}", &ctx.local_addr);
        let err = unsafe { tcp_shutdown(self.pcb as *mut tcp_pcb, 0, 1) };
        if err != err_enum_t_ERR_OK as err_t {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("netstack tcp_shutdown tx error {}", err),
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
