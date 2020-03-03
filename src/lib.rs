#![feature(unboxed_closures, fn_traits)]
#![allow(unused_must_use)]

//! `easy-rpc` is a cross-language RPC framework.
//! # Example
//! ```
//! use std::sync::Arc;
//! use easy_rpc::*;
//! 
//! struct ServerService;
//! 
//! const MUL: u32 = 1;
//! easy_service! {
//!     ServerService(self, _ss, arg, response)
//! 
//!     StringMethod {
//!         "add" => (a: u32, b: u32) {
//!             a + b
//!         }
//!         "print" => (s: String) {
//!             println!("{}", s);
//!         }
//!     }
//!     IntegerMethod {
//!         MUL => (a: u32, b: u32) {
//!             a * b
//!         }
//!     }
//! }
//! 
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     std::thread::spawn(|| {
//!         let mut ser = ws::bind("127.0.0.1:3333").unwrap();
//!         let (adaptor, _uri) = ws::accept(&mut ser).unwrap();
//!         Session::new(adaptor, Arc::new(ServerService)).loop_handle();
//!     });
//! 
//!     std::thread::sleep_ms(100);
//!     let session = Session::new(ws::connect("ws://127.0.0.1:3333")?, Arc::new(EmptyService));
//!     let val: u32 = session.request("add", (1, 2)).into()?;
//!     session.notify("print", format!("the result is {}", val));
//!     let val: u32 = session.request(MUL, (2, 3)).into()?;
//!     assert_eq!(val, 6);
//! 
//!     Ok(())
//! }
//! ```

#[macro_use]
extern crate downcast_rs;
extern crate rmp_serde as rmps;

/// Adaptor of WebSocket
#[cfg(feature = "ws")]
pub mod ws;
/// Adaptor of SharedMemory
#[cfg(all(feature = "shm", not(target_os = "android")))]
pub mod shm;

#[doc(no_inline)]
pub use serde_bytes::{Bytes, ByteBuf};

use std::sync::{
    Arc, RwLock, Mutex,
    mpsc::{channel, Sender},
    atomic::{AtomicU32, Ordering},
};
use std::fmt::{
    Debug, Display, Formatter,
    Result as FmtResult
};
use std::collections::HashMap;

use serde::Serialize;
use serde::de::DeserializeOwned;
use rmps::Serializer;
use rmps::decode::Error as DecodeError;
use rmp::{encode, decode};
use rmpv::{Value, decode::read_value};
use downcast_rs::DowncastSync;

const REQUEST: u32 = 0;         // Caller->Callee [REQUEST, ID: u32, METHOD: u32, ARGS: Any]
const RESPONSE: u32 = 1;        // Callee->Caller [RESPONSE, ID: u32, ERROR: Option<String>, RESULT: Any]
const NOTIFY: u32 = 2;          // [NOTIFY, METHOD: u32, ARGS: Any]

#[derive(Debug)]
pub enum RecvError {
    Disconnect,
}

/// Adaptor of different communicated methods
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

#[doc(hidden)]
pub struct RespData(Vec<u8>, usize);

impl RespData {
    #[inline]
    pub fn into<T: DeserializeOwned>(&self) -> Result<T, DecodeError> {
        rmps::from_read_ref(self.as_slice())
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] { &self.0[self.1..] }
}

pub enum RequestResult {
    Data(RespData),
    Error(String),
    Disconnect,
    Decode(RespData),
}

impl std::error::Error for RequestResult {}

impl Debug for RequestResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        use RequestResult::*;

        match self {
            Data(_) => write!(f, "<Success>"),
            Error(ref s) => write!(f, "Error: {}", s),
            Decode(_) => write!(f, "DecodeError"),
            Disconnect => write!(f, "Disconnect"),
        }; Ok(())
    }
}

impl Display for RequestResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        Debug::fmt(self, f)
    }
}

impl RequestResult {
    pub fn into<T: DeserializeOwned>(self) -> Result<T, RequestResult> {
        match self {
            RequestResult::Data(d) => rmps::from_read_ref(d.as_slice()).map_err(|_| RequestResult::Decode(d)),
            else_error => Err(else_error),
        }
    }

    #[inline]
    pub fn intos<T: DeserializeOwned>(self) -> Result<T, String> { self.into().map_err(|e| format!("{}", e)) }
}

/// Represent the arguments of a request/notify
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

/// Returner for a request, which can response some data
pub struct Ret<'a, 'b> {
    ss: &'a Session,
    req_id: &'b mut Option<u32>,
}

impl<T> std::ops::FnOnce<(T, )> for Ret<'_, '_> where T: Serialize {
    type Output = ();

    extern "rust-call" fn call_once(self, arg: (T, )) -> Self::Output {
        if let Some(req_id) = self.req_id.take() {
            self.ss.response(req_id, arg.0);
        }
    }
}

impl std::ops::FnOnce<(RequestResult, )> for Ret<'_, '_> {
    type Output = ();

    extern "rust-call" fn call_once(self, arg: (RequestResult, )) -> Self::Output {
        match arg.0 {
            RequestResult::Data(data) => unsafe { self.ret_raw(data.as_slice()) }
            RequestResult::Error(err) => { self.error(&err) } _ => {}
        }
    }
}

impl<'a, 'b> Ret<'a, 'b> {
    pub fn error(self, s: &str) {
        if let Some(req_id) = self.req_id.take() {
            self.ss.response_error(req_id, s);
        }
    }

    pub unsafe fn ret_raw(self, msgpack: &[u8]) {
        if let Some(req_id) = self.req_id.take() {
            let mut resp = self.ss.prepare_response(req_id);
            encode::write_nil(&mut resp);
            resp.extend_from_slice(msgpack);
            self.ss.send_pack(resp);
        }
    }

    /// Convert to AsyncRet. Be careful the session must be allocated by `Arc`
    pub unsafe fn into_async(self) -> Option<AsyncRet> {
        self.req_id.map(|req_id| AsyncRet { ss: self.ss.arc_clone(), req_id })
    }

    /// Distinguish request/notify, return true if the packet is a request
    #[inline]
    pub fn is_valid(&self) -> bool { return self.req_id.is_some() }
}

/// Asynchronous returner
pub struct AsyncRet {
    ss: Arc<Session>,
    req_id: u32,
}

impl AsyncRet {
    pub fn error(self, s: &str) {
        self.ss.response_error(self.req_id, s);
    }

    pub unsafe fn ret_raw(self, msgpack: &[u8]) {
        let mut resp = self.ss.prepare_response(self.req_id);
        encode::write_nil(&mut resp);
        resp.extend_from_slice(msgpack);
        self.ss.send_pack(resp);
    }
}

impl<T> std::ops::FnOnce<(T, )> for AsyncRet where T: Serialize {
    type Output = ();

    extern "rust-call" fn call_once(self, arg: (T, )) -> Self::Output {
        self.ss.response(self.req_id, &arg.0)
    }
}

impl std::ops::FnOnce<(RequestResult, )> for AsyncRet {
    type Output = ();

    extern "rust-call" fn call_once(self, arg: (RequestResult, )) -> Self::Output {
        match arg.0 {
            RequestResult::Data(data) => unsafe { self.ret_raw(data.as_slice()) }
            RequestResult::Error(err) => { self.error(&err) } _ => {}
        }
    }
}

pub struct HandleError(String);

impl<T: std::fmt::Debug> From<T> for HandleError {
    fn from(e: T) -> Self { HandleError(format!("{:#?}", e)) }
}

/// The method of request/notify, can be an integer or a string
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

impl<'a> Method<'a> {
    #[inline(always)]
    pub fn serialize<W: std::io::Write>(&self, w: &mut W) {
        match *self {
            Method::Int(i) => encode::write_u32(w, i),
            Method::Str(s) => encode::write_str(w, s),
        };
    }

    #[inline]
    pub fn to_str(self) -> Result<&'a str, HandleError> {
        match self {
            Method::Int(_) => Err(HandleError("Method not match".into())),
            Method::Str(s) => Ok(s),
        }
    }

    #[inline]
    pub fn to_int(self) -> Result<u32, HandleError> {
        match self {
            Method::Int(i) => Ok(i),
            Method::Str(_) => Err(HandleError("Method not match".into())),
        }
    }
}

/// A sugar for converting integer/string to `Method`
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

/// User defined RPC service, handle the request/notify
pub trait Service: DowncastSync {
    fn handle(&self, _ss: &Session, _arg: Arg, _ret: Ret) -> Result<(), HandleError> {
        Err(HandleError("No this method".into()))
    }
}
impl_downcast!(sync Service);

/// A [`Service`] implementation for test
pub struct EmptyService;
impl Service for EmptyService {}

pub type ServiceType = Arc<dyn Service + Send + Sync>;

/// Highly abstract communication endpoint
pub struct Session {
    sender_table: RwLock<HashMap<u32, Sender<RequestResult>>>,
    recv_mutex: Mutex<()>,
    id_counter: AtomicU32,
    pub adaptor: Arc<dyn Adaptor>,
    pub service: ServiceType,
}

impl Session {
    pub fn new(adaptor: Arc<dyn Adaptor>, service: ServiceType) -> Session {
        Session {
            sender_table: RwLock::new(HashMap::new()),
            recv_mutex: Mutex::new(()),
            id_counter: AtomicU32::new(1),
            adaptor, service,
        }
    }

    /// Convert `&Session` to `Arc<Session>`. Be careful the session must be allocated by `Arc`
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

    /// Receive a packet.
    /// This function will always block the current thread if there is no packet available.
    pub fn recv_packet(&self) -> Option<Result<Vec<u8>, RecvError>> {
        if let Ok(_) = self.recv_mutex.try_lock() {
            Some(self.adaptor.recv())
        } else { None }
    }

    /// Handle a packet which received by [`Session::recv_packet`]
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
                        RequestResult::Data(RespData(pack, offset))
                    } else {
                        RequestResult::Error(error.as_str().unwrap().into())
                    });
                } else { panic!("sender not found"); }
            }
            _else => { panic!("Invalid PackType"); }
        }
    }

    /// [`Session::recv_packet`] and then [`Session::handle_packet`] looply util the adaptor disconnect.
    pub fn loop_handle(&self) {
        loop {
            match self.recv_packet() {
                Some(Ok(pack)) => self.handle_packet(pack),
                Some(Err(RecvError::Disconnect)) => {
                    self.sender_table.write().unwrap().clear();
                    break;
                },
                err => { panic!("unexpected error: {:?}", err); }
            }
        }
    }

    fn send_pack(&self, frame: Vec<u8>) -> bool { self.adaptor.send(frame) }

    fn next_id(&self) -> u32 { self.id_counter.fetch_add(1, Ordering::SeqCst) }

    fn send_and_wait_response(&self, req_id: u32, pack: Vec<u8>) -> RequestResult {
        use RecvError::*;
        let (sender, recver) = channel::<RequestResult>();
        self.sender_table.write().unwrap().insert(req_id, sender);
        self.send_pack(pack);
        loop {
            if let Ok(r) = recver.try_recv() { break r; }
            match self.recv_packet() {
                None => break recver.recv().unwrap_or(RequestResult::Disconnect),
                Some(Ok(pack)) => { self.handle_packet(pack); }
                Some(Err(Disconnect)) => break RequestResult::Disconnect,
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

    fn serialize<S: Serialize, W: std::io::Write>(arg: &S, w: W) {
        if cfg!(feature = "struct_map") {
            arg.serialize(&mut Serializer::new(w).with_struct_map());
        } else {
            arg.serialize(&mut Serializer::new(w));
        }
    }

    /// Do a request.
    /// This function will always block the current thread if the other side is not response.
    pub fn request<'a>(&self, method: impl ToMethod<'a>, arg: impl Serialize) -> RequestResult {
        let (mut pack, req_id) = self.prepare_request(method.to_method());
        Self::serialize(&arg, &mut pack);
        self.send_and_wait_response(req_id, pack)
    }

    /// Do a notify.
    pub fn notify<'a>(&self, method: impl ToMethod<'a>, arg: impl Serialize) -> bool {
        let mut pack = self.prepare_notify(method.to_method());
        Self::serialize(&arg, &mut pack);
        self.send_pack(pack)
    }

    fn response(&self, req_id: u32, arg: impl Serialize) {
        let mut pack = self.prepare_response(req_id);
        encode::write_nil(&mut pack);
        Self::serialize(&arg, &mut pack);
        self.send_pack(pack);
    }

    fn response_error(&self, req_id: u32, err: impl AsRef<str>) {
        let mut pack = self.prepare_response(req_id);
        encode::write_str(&mut pack, err.as_ref());
        encode::write_nil(&mut pack);
        self.send_pack(pack);
    }

    /// Do a request with msgpack bytes.
    pub unsafe fn request_transfer<'a>(&self, method: impl ToMethod<'a>, msgpack: &[u8]) -> RequestResult {
        let (mut pack, req_id) = self.prepare_request(method.to_method());
        pack.extend_from_slice(msgpack);
        self.send_and_wait_response(req_id, pack)
    }

    /// Do a notify with msgpack bytes.
    pub unsafe fn notify_transfer<'a>(&self, method: impl ToMethod<'a>, msgpack: &[u8]) -> bool {
        let mut pack = self.prepare_notify(method.to_method());
        pack.extend_from_slice(msgpack);
        self.send_pack(pack)
    }

    pub unsafe fn response_transfer<'a>(&self, req_id: u32, msgpack: &[u8]) -> bool {
        let mut pack = self.prepare_response(req_id);
        pack.extend_from_slice(msgpack);
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

#[macro_export]
macro_rules! easy_handle {
    (@expand_args $arg:ident, $($i:ident: $t:ty),+) => {
        let ($($i),+): ($($t),+) = $arg.into()?;
    };
    (@expand_args $arg:ident,) => {};

    (@body_option $ret:ident manual $body:block) => { $body };
    (@body_option $ret:ident $body:block) => { $ret($body) };

    (
        @switch $switch:expr, $arg:ident, $ret:ident,
        $($m:tt => ($($argdef:tt)*) $($body_option:ident)? $block:block) *
    ) => {
        match $switch {
            $($m => {
                easy_handle!(@expand_args $arg, $($argdef)*);
                easy_handle!(@body_option $ret $($body_option)? $block)
            })*
            _ => { return Err("Unhandled Method".into()); }
        }
    };

    (
        $arg:ident, $ret:ident,
        IntegerMethod { $($tts_int:tt)* }
        $(Str($str_var:ident) => $handle_str:block)?
    ) => {
        use $crate::Method::*;
        #[allow(unreachable_patterns)]
        match $arg.method {
            Int(i) => easy_handle!(@switch i, $arg, $ret, $($tts_int)*),
            $(Str($str_var) => $handle_str,)?
            _ => { return Err("Unhandled Method".into()); }
        }
    };

    (
        $arg:ident, $ret:ident,
        StringMethod { $($tts_str:tt)* }
        $(Int($int_var:ident) => $handle_int:block)?
    ) => {
        use $crate::Method::*;
        #[allow(unreachable_patterns)]
        match $arg.method {
            Str(s) => easy_handle!(@switch s, $arg, $ret, $($tts_str)*),
            $(Int($int_var) => $handle_int,)?
            _ => { return Err("Unhandled Method".into()); }
        }
    };

    (
        $arg:ident, $ret:ident,
        StringMethod { $($tts_str:tt)* }
        IntegerMethod { $($tts_int:tt)* }
    ) => {
        use $crate::Method::*;
        #[allow(unreachable_patterns)]
        match $arg.method {
            Str(s) => easy_handle!(@switch s, $arg, $ret, $($tts_str)*),
            Int(i) => easy_handle!(@switch i, $arg, $ret, $($tts_int)*),
        }
    };
}

#[macro_export]
macro_rules! easy_service {
    ($sv:tt($self_:tt, $ss:ident, $arg:ident, $ret:ident) $($tts:tt)*) => {
        impl $crate::Service for $sv {
            fn handle(&$self_, $ss: &Session, $arg: Arg, $ret: Ret) -> Result<(), HandleError> {
                easy_handle!($arg, $ret, $($tts)*);
                Ok(())
            }
        }
    };
}