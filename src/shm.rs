
use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicU32, Ordering};
use std::mem::{size_of, transmute};
use std::sync::{Arc, Mutex};
use std::time::{Instant, Duration};

use shared_memory::*;
use crate::{Adaptor, RecvError};

pub use shared_memory::Timeout;

struct Channel {
    data_ty: AtomicU32,
    data_len: AtomicU32,
    sended: AtomicU32,
    recved: AtomicU32,
    buf: [u8; 2048],
}

const FRAME_NONE: u32 = 0;
const FRAME_PING: u32 = 1;
const FRAME_PONG: u32 = 2;
const FRAME_DATA: u32 = 3;

enum Frame {
    None,
    Ping,
    Pong,
    Timeout,
    Data(Vec<u8>),
}

const SYNC_TIME_OUT: Duration = Duration::from_secs(1);

impl Channel {
    fn init(&mut self) {
        self.data_ty = AtomicU32::new(FRAME_NONE);
        self.data_len = AtomicU32::new(0);
        self.sended = AtomicU32::new(0);
        self.recved = AtomicU32::new(0);
    }

    fn wait_recv(&mut self) -> Option<(u32, u32)> {
        let begin = Instant::now();
        loop {
            let sended = self.sended.load(Ordering::Relaxed);
            let recved = self.recved.load(Ordering::Relaxed);
            if recved == sended { return Some((sended, recved)); }

            if begin.elapsed() > SYNC_TIME_OUT { return None; }
        }
    }

    fn send_type(&mut self, frame: &Frame) -> bool {
        let begin = Instant::now();
        while self.data_ty.load(Ordering::Relaxed) != FRAME_NONE {
            if begin.elapsed() > SYNC_TIME_OUT { return false; }
        }

        match frame {
            Frame::Ping => self.data_ty.store(FRAME_PING, Ordering::Relaxed),
            Frame::Pong => self.data_ty.store(FRAME_PONG, Ordering::Relaxed),
            Frame::Data(ref d) => {
                self.data_ty.store(FRAME_DATA, Ordering::Relaxed);
                self.data_len.store(d.len() as u32, Ordering::Relaxed);
            }
            _ => { panic!(); }
        }
        true
    }

    fn send_data(&mut self, data: &[u8]) -> bool {
        let (mut sended, mut _recved) = (0u32, 0u32);
        loop {
            // Copy data slice
            {
                let sended = sended as usize;
                let rest_size = data.len() - sended;
                let size = self.buf.len().min(rest_size);
                (self.buf[..size]).copy_from_slice(&data[sended .. sended + size]);
                // Update sended
                let after_size = sended as usize + size;
                self.sended.store(after_size as u32, Ordering::Relaxed);
                if after_size >= data.len() { break; }
            }
            // Wait recv
            match self.wait_recv() {
                Some(r) => { sended = r.0; _recved = r.1; }
                None => return false,
            }
        }
        true
    }

    fn wait_send(&mut self) -> Option<(u32, u32)> {
        let begin = Instant::now();
        loop {
            let sended = self.sended.load(Ordering::Relaxed);
            let recved = self.recved.load(Ordering::Relaxed);
            if sended > recved { return Some((sended, recved)); }

            if begin.elapsed() > SYNC_TIME_OUT { return None; }
        }
    }

    fn recv(&mut self) -> Frame {
        let result = match self.data_ty.load(Ordering::Relaxed) {
            FRAME_NONE => Frame::None,
            FRAME_PING => Frame::Ping,
            FRAME_PONG => Frame::Pong,
            FRAME_DATA => {
                let data_size = self.data_len.load(Ordering::Relaxed) as usize;
                if data_size == 0 { return Frame::None; }

                let mut data: Vec<u8> = Vec::with_capacity(data_size);
                let mut timeout = false;
                loop {
                    match self.wait_send() {
                        Some((sended, recved)) => {
                            // Push data slice
                            let size = (sended - recved) as usize;
                            data.extend_from_slice(&self.buf[..size]);
                            // Update recved
                            self.recved.store(sended, Ordering::Relaxed);
                            if sended as usize >= data_size { break; }
                        }
                        None => { timeout = true; break; }
                    }
                }
                self.sended.store(0, Ordering::Relaxed);
                self.recved.store(0, Ordering::Relaxed);
                self.data_len.store(0, Ordering::Relaxed);
                if timeout { Frame::Timeout } else { Frame::Data(data) }
            }
            _ => { panic!(); }
        };
        self.data_ty.store(FRAME_NONE, Ordering::Relaxed); result
    }
}

struct Communicator {
    ch1: Channel,
    ch2: Channel,
}

pub struct ShmAdaptor {
    shmem: UnsafeCell<SharedMem>,
    send_lock: Mutex<()>,
    connected: Cell<bool>,
    client: bool,
}

impl ShmAdaptor {
    const EVT_MASTER: usize = 0;
    const EVT_SLAVER: usize = 1;

    #[inline]
    fn shmem(&self) -> &mut SharedMem { 
        unsafe { &mut *self.shmem.get() }
    }

    fn send_channel(&self) -> &mut Channel {
        let this = self.as_comm();
        if self.client { &mut this.ch2 } else { &mut this.ch1 }
    }

    fn recv_channel(&self) -> &mut Channel {
        let this = self.as_comm();
        if self.client { &mut this.ch1 } else { &mut this.ch2 }
    }

    #[inline]
    fn send_eid(&self) -> usize {
        if self.client { Self::EVT_MASTER } else { Self::EVT_SLAVER }
    }

    #[inline]
    fn recv_eid(&self) -> usize {
        if self.client { Self::EVT_SLAVER } else { Self::EVT_MASTER }
    }

    #[inline]
    fn as_comm(&self) -> &'static mut Communicator {
        unsafe { transmute(self.shmem().get_ptr()) }
    }

    fn send_frame(&self, frame: Frame) -> bool {
        let _guard = self.send_lock.lock().unwrap();
        let ch = self.send_channel();
        if !ch.send_type(&frame) { return false; }

        let sid = self.send_eid();
        self.shmem().set(sid, EventState::Signaled);
        if let Frame::Data(data) = frame {
            return ch.send_data(&data);
        }
        true
    }

    fn new(shmem: SharedMem, client: bool) -> Self {
        ShmAdaptor {
            shmem: UnsafeCell::new(shmem),
            send_lock: Mutex::new(()),
            connected: Cell::new(true),
            client,
        }
    }

    pub fn create(path: &str) -> Result<Self, SharedMemError> {
        let size = size_of::<Communicator>();
        let shmem = SharedMemConf::default()
                .set_os_path(path).set_size(size)
                .add_event(EventType::Auto)?
                .add_event(EventType::Auto)?
                .create()?;
        let this = Self::new(shmem, false);
        this.as_comm().ch1.init();
        this.as_comm().ch2.init();
        Ok(this)
    }

    pub fn wait(&self, timeout: Option<Timeout>) -> bool {
        self.shmem().wait(Self::EVT_MASTER, timeout.unwrap_or(Timeout::Infinite)).is_ok()
    }

    pub fn open(path: &str) -> Result<Self, SharedMemError> {
        let mut shmem = SharedMem::open(path)?;
        shmem.set(Self::EVT_MASTER, EventState::Signaled);
        Ok(Self::new(shmem, true))
    }
}

unsafe impl Send for ShmAdaptor {}
unsafe impl Sync for ShmAdaptor {}

impl Adaptor for ShmAdaptor {
    fn send(&self, data: Vec<u8>) -> bool {
        return self.send_frame(Frame::Data(data));
    }

    fn recv(&self) -> Result<Vec<u8>, RecvError> {
        const CELL_TIMEOUT: usize = 100;
        let mut ping_time = 0usize;
        loop {
            let rid = self.recv_eid();
            if let Ok(_) = self.shmem().wait(rid, Timeout::Milli(CELL_TIMEOUT)) {
                match self.recv_channel().recv() {
                    Frame::Data(data) => return Ok(data),
                    Frame::Ping => { self.send_frame(Frame::Pong); }
                    Frame::Pong => { if ping_time > 0 { ping_time -= CELL_TIMEOUT; } }
                    Frame::None | Frame::Timeout => { panic!(""); }
                }
            } else if ping_time > 200 {
                self.connected.set(false);
                return Err(RecvError::Disconnected);
            } else {
                ping_time += CELL_TIMEOUT;
                self.send_frame(Frame::Ping);
            }
        }
    }

    fn connected(&self) -> bool { self.connected.get() }

    fn close(&self) { /* TODO: */ }
}

pub fn create(path: &str) -> Result<Arc<ShmAdaptor>, SharedMemError> {
    Ok(Arc::new(ShmAdaptor::create(path)?))
}

pub fn connect(path: &str) -> Result<Arc<ShmAdaptor>, SharedMemError> {
    Ok(Arc::new(ShmAdaptor::open(path)?))
}