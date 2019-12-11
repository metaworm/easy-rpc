
use std::sync::Mutex;
use std::sync::{Arc, RwLock};
use std::net::{TcpStream, TcpListener, ToSocketAddrs};
pub use websocket::WebSocketError;

use websocket::sync::{Server, Client};
use websocket::server::NoTlsAcceptor;
use websocket::server::WsServer;
use websocket::{
    OwnedMessage,
    client::{ClientBuilder, sync::{Reader, Writer}},
};

use crate::{Session, Adaptor, RecvError, ServiceT};

pub struct WsSession;

struct WsSendRecver {
    sender: Mutex<Writer<TcpStream>>,
    receiver: Mutex<Reader<TcpStream>>,
    disconnected: RwLock<bool>,
}

impl Adaptor for WsSendRecver {
    fn send(&self, data: Vec<u8>) -> bool {
        self.sender.lock().unwrap()
                   .send_message(&OwnedMessage::Binary(data)).is_ok()
    }

    fn recv(&self) -> Result<Vec<u8>, RecvError> {
        match recv_message(&mut self.receiver.lock().unwrap(), &self.sender) {
            Err(e) => {
                if is_disconnected(&e) {
                    *self.disconnected.write().unwrap() = true;
                    Err(RecvError::Disconnected)
                } else { panic!("Invalid Error: {:?}", e); }
            }
            Ok(data) => { Ok(data) }
        }
    }

    fn connected(&self) -> bool {
        !*self.disconnected.read().unwrap()
    }

    fn close(&self) {
        self.sender.lock().unwrap().shutdown_all();
    }
}

fn recv_message(r: &mut Reader<TcpStream>, s: &Mutex<Writer<TcpStream>>) -> Result<Vec<u8>, WebSocketError> {
    loop {
        match r.recv_message()? {
            OwnedMessage::Close(_) => {
                s.lock().unwrap()
                 .send_message(&OwnedMessage::Close(None))?;
            }
            OwnedMessage::Ping(ping) => {
                s.lock().unwrap()
                 .send_message(&OwnedMessage::Pong(ping))?;
            }
            OwnedMessage::Binary(msg) => { return Ok(msg); }
            _ => {}
        }
    }
}

impl WsSession {
    pub fn new(client: Client<TcpStream>, service: ServiceT) -> Session {
        let (receiver, sender) = client.split().unwrap();
        // let (data_send, data_recv) = channel::<Vec<u8>>();
        let sr = Arc::new(WsSendRecver {
            sender: Mutex::new(sender),
            receiver: Mutex::new(receiver),
            disconnected: RwLock::new(false),
        });
        Session::new(sr.clone(), service)
    }
}

fn is_disconnected(err: &WebSocketError) -> bool {
    match *err {
        WebSocketError::NoDataAvailable => true,
        WebSocketError::IoError(_) => true,
        _ => false,
    }
}

use std::io;

pub type ServerT = WsServer<NoTlsAcceptor, TcpListener>;

pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<ServerT> { Server::bind(addr) }

pub fn accept(server: &mut ServerT) -> Client<TcpStream> {
    loop {
        if let Ok(s) = server.accept() {
            if let Ok(s) = s.accept() { return s; }
        }
    }
}

pub fn accept_uri(server: &mut ServerT) -> (Client<TcpStream>, String) {
    loop {
        if let Ok(s) = server.accept() {
            let uri = s.uri();
            if let Ok(s) = s.accept() { return (s, uri); }
        }
    }
}

pub fn connect(url: &str) -> Result<Client<TcpStream>, WebSocketError> {
    Ok(ClientBuilder::new(url).unwrap().connect_insecure()?)
}