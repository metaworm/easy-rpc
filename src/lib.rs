#![feature(unboxed_closures, fn_traits)]
#![allow(unused_must_use)]

#[macro_use]
extern crate downcast_rs;
extern crate rmp_serde as rmps;

pub mod ws;
pub mod shm;

pub use serde_bytes::{Bytes, ByteBuf};
pub use rmps::decode::Error as DecodeError;

use std::sync::{
    Arc, RwLock, Mutex,
    mpsc::{channel, Sender},
    atomic::{AtomicU32, Ordering},
};
use std::cell::Cell;
use std::collections::HashMap;
use std::thread::{self, ThreadId};

use serde::Serialize;
use serde::de::DeserializeOwned;
use rmps::Serializer;

use rmp::{encode, decode};
use rmpv::{Value, decode::read_value};
use downcast_rs::DowncastSync;

const REQUEST: u32 = 0;         // Caller->Callee [REQUEST, ID: u32, METHOD: u32, ARGS: Any]
const RESPONSE: u32 = 1;        // Callee->Caller [RESPONSE, ID: u32, ERROR: Option<String>, RESULT: Any]
const NOTIFY: u32 = 2;          // [NOTIFY, METHOD: u32, ARGS: Any]

#[derive(Debug)]
pub enum RecvError {
    NoData,
    Disconnected,
    MutexOccupied,
}

pub trait Adaptor: DowncastSync {
    // Send data
    fn send(&self, data: Vec<u8>) -> bool;

    // Recv Data, this function maybe blocked
    fn recv(&self) -> Result<Vec<u8>, RecvError>;

    // If the connection still connected
    fn connected(&self) -> bool;

    // Close the connection
    fn close(&self);
}
impl_downcast!(sync Adaptor);

pub struct RespData(Vec<u8>, usize);

pub enum Response {
    Data(RespData),
    Error(String),
}

impl RespData {
    #[inline]
    pub fn into<T>(&self) -> Result<T, DecodeError> where T: DeserializeOwned {
        rmps::from_read_ref(self.as_slice())
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] { &self.0[self.1..] }
}

impl Response {
    pub fn into<T>(self) -> Result<T, String> where T: DeserializeOwned {
        match self {
            Response::Data(d) => {
                rmps::from_read_ref(d.as_slice()).map_err(|e| format!("{:?}", e))
            }
            Response::Error(s) => { Err(s) }
        }
    }
}

pub struct Arg<'a> {
    pub method: Method<'a>,
    pub bytes: &'a [u8],
    pub id: u32,
}

impl<'a> Arg<'a> {
    #[inline]
    pub fn into<T>(self) -> Result<T, DecodeError> where T: DeserializeOwned {
        rmps::from_read_ref(self.bytes)
    }
}

pub struct Ret<'a, 'b> {
    ss: &'a Session,
    req_id: &'b mut Option<u32>,
}

impl<T> std::ops::FnOnce<(T, )> for Ret<'_, '_> where T: Serialize {
    type Output = ();

    extern "rust-call" fn call_once(self, arg: (T, )) -> Self::Output {
        self.ret(arg.0)
    }
}

impl std::ops::FnOnce<(Response, )> for Ret<'_, '_> {
    type Output = ();

    extern "rust-call" fn call_once(self, arg: (Response, )) -> Self::Output {
        match arg.0 {
            Response::Data(data) => { self.ret_raw(data.as_slice()) }
            Response::Error(err) => { self.err(&err) }
        }
    }
}

impl<'a, 'b> Ret<'a, 'b> {
    pub fn err(self, s: &str) {
        if let Some(req_id) = self.req_id.take() {
            self.ss.response_error(req_id, s);
        }
    }

    pub fn ret<T: Serialize>(self, r: T) {
        if let Some(req_id) = self.req_id.take() {
            self.ss.response(req_id, r);
        }
    }

    pub fn ret_raw(self, data: &[u8]) {
        if let Some(req_id) = self.req_id.take() {
            let mut resp = self.ss.prepare_response(req_id);
            encode::write_nil(&mut resp);
            resp.extend_from_slice(data);
            self.ss.send_pack(resp);
        }
    }

    pub unsafe fn into_async(self) -> AsyncRet {
        AsyncRet { ss: self.ss.arc_clone(), req_id: self.req_id.take().unwrap(), returned: Cell::new(false) }
    }

    #[inline]
    pub fn is_valid(&self) -> bool { return self.req_id.is_some() }
}

pub struct AsyncRet {
    ss: Arc<Session>,
    req_id: u32,
    returned: Cell<bool>,
}

impl AsyncRet {
    pub fn err(self, s: &str) {
        self.ss.response_error(self.req_id, s);
    }

    pub fn ret_raw(self, data: &[u8]) {
        let mut resp = self.ss.prepare_response(self.req_id);
        encode::write_nil(&mut resp);
        resp.extend_from_slice(data);
        self.ss.send_pack(resp);
    }
}

impl<T> std::ops::FnOnce<(T, )> for AsyncRet where T: Serialize {
    type Output = ();

    extern "rust-call" fn call_once(self, arg: (T, )) -> Self::Output {
        if !self.returned.replace(true) { self.ss.response(self.req_id, &arg.0) }
    }
}

pub struct HandleError(String);

impl<T: std::fmt::Debug> From<T> for HandleError {
    fn from(e: T) -> Self { HandleError(format!("{:#?}", e)) }
}

// impl From<String> for HandleError {
//     fn from(e: String) -> Self { HandleError(e) }
// }

// impl From<&str> for HandleError {
//     fn from(e: &str) -> Self { HandleError(e.into()) }
// }

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Method<'a> {
    Int(u32),
    Str(&'a str),
}

impl PartialEq<u32> for Method<'_> {
    #[inline]
    fn eq(&self, other: &u32) -> bool {
        match *self { Method::Int(n) => n == *other, _ => false }
    }
}

impl PartialEq<str> for Method<'_> {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        match *self { Method::Str(s) => s == other, _ => false }
    }
}

impl Method<'_> {
    #[inline(always)]
    pub fn serialize<W: std::io::Write>(&self, w: &mut W) {
        match *self {
            Method::Int(i) => encode::write_u32(w, i),
            Method::Str(s) => encode::write_str(w, s),
        };
    }
}

pub trait ToMethod<'a> {
    fn to_method(self) -> Method<'a>;
}

impl ToMethod<'_> for u32 {
    #[inline(always)]
    fn to_method(self) -> Method<'static> { Method::Int(self) }
}

impl<'a> ToMethod<'a> for &'a str {
    #[inline(always)]
    fn to_method(self) -> Method<'a> { Method::Str(self) }
}

impl<'a> ToMethod<'a> for Method<'a> {
    #[inline(always)]
    fn to_method(self) -> Method<'a> { self }
}

pub trait Service: DowncastSync {
    fn handle(&self, _ss: &Session, _arg: Arg, _ret: Ret) -> Result<(), HandleError> { Ok(()) }
}
impl_downcast!(sync Service);

pub struct EmptyService;
impl Service for EmptyService {}

pub type ServiceT = Arc<dyn Service + Send + Sync>;

pub struct Session {
    sender_table: RwLock<HashMap<u32, Sender<Response>>>,
    recv_mutex: Mutex<()>,
    id_counter: AtomicU32,
    pub handle_tid: Cell<ThreadId>,
    pub adaptor: Arc<dyn Adaptor>,
    pub service: ServiceT,
}

impl Session {
    pub fn new(adaptor: Arc<dyn Adaptor>, service: ServiceT) -> Session {
        Session {
            sender_table: RwLock::new(HashMap::new()),
            recv_mutex: Mutex::new(()),
            id_counter: AtomicU32::new(1),
            handle_tid: Cell::new(thread::current().id()),
            adaptor, service,
        }
    }

    pub unsafe fn arc_clone(&self) -> Arc<Session> {
        let s0 = Arc::from_raw(self as *const Session);
        let s1 = s0.clone(); std::mem::forget(s0); s1
    }

    #[inline]
    fn parse_method<'a>(val: &'a Value) -> Option<Method<'a>> {
        match val {
            Value::Integer(i) => Some(Method::Int(i.as_u64().unwrap() as u32)),
            Value::String(s) => Some(Method::Str(s.as_str().unwrap())),
            _ => None,
        }
    }

    pub fn recv_packet(&self) -> Result<Vec<u8>, RecvError> {
        if let Ok(_) = self.recv_mutex.try_lock() {
            self.adaptor.recv()
        } else { Err(RecvError::MutexOccupied) }
    }

    pub fn handle_packet(&self, pack: Vec<u8>) {
        let mut reader = &pack[..];
        let start_ptr = reader.as_ptr() as usize;
        let len = decode::read_array_len(&mut reader).unwrap();
        let pack_type: u32 = decode::read_int(&mut reader).unwrap();

        match pack_type {
            REQUEST => {
                assert!(len == 4);
                let req_id: u32 = decode::read_int(&mut reader).unwrap();
                let method_value = read_value(&mut reader).unwrap();
                let method = Self::parse_method(&method_value).unwrap();

                let mut req_wrapper = Some(req_id);
                let ret = Ret { ss: self, req_id: &mut req_wrapper };
                let arg = Arg { method, id: req_id, bytes: &reader };
                if let Err(e) = self.service.handle(self, arg, ret) {
                    self.response_error(req_id, e.0);
                } else if req_wrapper.is_some() {
                    // TODO: warning: not response the request
                }
            }
            NOTIFY => {
                assert!(len == 3);
                let method_value = read_value(&mut reader).unwrap();
                let method = Self::parse_method(&method_value).unwrap();
                let mut req_wrapper = None;
                let ret = Ret { ss: self, req_id: &mut req_wrapper };
                let arg = Arg { method, id: 0, bytes: &reader };
                self.service.handle(self, arg, ret);
            }
            RESPONSE => {
                assert!(len == 4);
                let req_id: u32 = decode::read_int(&mut reader).unwrap();
                let error = read_value(&mut reader).unwrap();
                if let Some(sender) = self.sender_table.write().unwrap().remove(&req_id) {
                    sender.send(if error.is_nil() {
                        let offset = reader.as_ptr() as usize - start_ptr;
                        Response::Data(RespData(pack, offset))
                    } else {
                        Response::Error(error.as_str().unwrap().into())
                    });
                }
            }
            _else => { panic!("Invalid PackType"); }
        }
    }

    pub fn loop_handle(&self) {
        loop {
            match self.recv_packet() {
                Err(RecvError::Disconnected) => break,
                Ok(pack) => self.handle_packet(pack),
                _ => {}
            }
        }
    }

    #[inline]
    pub fn connected(&self) -> bool { self.adaptor.connected() }

    #[inline]
    pub fn close(&self) { self.adaptor.close() }

    fn send_pack(&self, frame: Vec<u8>) -> bool {
        self.adaptor.send(frame)
    }

    fn next_id(&self) -> u32 {
        self.id_counter.fetch_add(1, Ordering::SeqCst)
    }

    fn wait_response(&self, req_id: u32) -> Response {
        let (sender, recver) = channel::<Response>();
        self.sender_table.write().unwrap().insert(req_id, sender);
        loop {
            match self.recv_packet() {
                Ok(pack) => {
                    self.handle_packet(pack);
                    if let Ok(r) = recver.try_recv() {
                        break r;
                    }
                }
                Err(RecvError::MutexOccupied) => {
                    break recver.recv().unwrap();
                }
                _ => {}
            }
        }
    }

    fn prepare_request(&self, method: Method) -> (Vec<u8>, u32) {
        let mut pack: Vec<u8> = Vec::with_capacity(0x30);
        let req_id = self.next_id();
        encode::write_array_len(&mut pack, 4);
        encode::write_u32(&mut pack, REQUEST);
        encode::write_u32(&mut pack, req_id);
        method.serialize(&mut pack);
        (pack, req_id)
    }

    pub fn request<'a>(&self, method: impl ToMethod<'a>, arg: impl Serialize) -> Response {
        let (mut pack, req_id) = self.prepare_request(method.to_method());
        arg.serialize(&mut Serializer::new(&mut pack).with_struct_map());
        self.send_pack(pack); self.wait_response(req_id)
    }

    pub fn notify<'a>(&self, method: impl ToMethod<'a>, arg: impl Serialize) -> bool {
        let mut pack = self.prepare_notify(method.to_method());
        arg.serialize(&mut Serializer::new(&mut pack).with_struct_map());
        self.send_pack(pack)
    }

    fn response(&self, req_id: u32, arg: impl Serialize) {
        let mut pack = self.prepare_response(req_id);
        encode::write_nil(&mut pack);
        arg.serialize(&mut Serializer::new(&mut pack).with_struct_map());
        self.send_pack(pack);
    }

    fn response_error(&self, req_id: u32, err: impl AsRef<str>) {
        let mut pack = self.prepare_response(req_id);
        encode::write_str(&mut pack, err.as_ref());
        encode::write_nil(&mut pack);
        self.send_pack(pack);
    }

    pub unsafe fn request_transfer<'a>(&self, method: impl ToMethod<'a>, arg: &[u8]) -> Response {
        let (mut pack, req_id) = self.prepare_request(method.to_method());
        pack.extend_from_slice(arg);
        self.send_pack(pack); self.wait_response(req_id)
    }

    pub unsafe fn notify_transfer<'a>(&self, method: impl ToMethod<'a>, arg: &[u8]) -> bool {
        let mut pack = self.prepare_notify(method.to_method());
        pack.extend_from_slice(arg);
        self.send_pack(pack)
    }

    fn prepare_notify(&self, method: Method) -> Vec<u8> {
        let mut pack: Vec<u8> = Vec::new();
        encode::write_array_len(&mut pack, 3);
        encode::write_u32(&mut pack, NOTIFY);
        method.serialize(&mut pack);
        pack
    }

    fn prepare_response(&self, req_id: u32) -> Vec<u8> {
        let mut pack: Vec<u8> = Vec::new();
        encode::write_array_len(&mut pack, 4);
        encode::write_u32(&mut pack, RESPONSE);
        encode::write_u32(&mut pack, req_id);
        pack
    }
}

unsafe impl Send for Session {}
unsafe impl Sync for Session {}