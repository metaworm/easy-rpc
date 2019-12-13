
use std::io;
use std::sync::Mutex;
use std::sync::{Arc, RwLock};
use std::net::{TcpStream, TcpListener, ToSocketAddrs};

use websocket::sync::{Server, Client};
use websocket::server::NoTlsAcceptor;
use websocket::server::WsServer;
use websocket::{
    OwnedMessage,
    client::{ClientBuilder, sync::{Reader, Writer}},
};
pub use websocket::WebSocketError;

use crate::{Adaptor, RecvError};

pub struct WsAdaptor {
    sender: Mutex<Writer<TcpStream>>,
    receiver: Mutex<Reader<TcpStream>>,
    disconnected: RwLock<bool>,
}

impl WsAdaptor {
    pub fn new(client: Client<TcpStream>) -> io::Result<WsAdaptor> {
        let (receiver, sender) = client.split()?;
        Ok(WsAdaptor {
            sender: Mutex::new(sender),
            receiver: Mutex::new(receiver),
            disconnected: RwLock::new(false),
        })
    }
}

impl Adaptor for WsAdaptor {
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

fn is_disconnected(err: &WebSocketError) -> bool {
    match *err {
        WebSocketError::NoDataAvailable => true,
        WebSocketError::IoError(_) => true,
        _ => false,
    }
}

pub type ServerT = WsServer<NoTlsAcceptor, TcpListener>;

pub fn bind(addr: impl ToSocketAddrs) -> io::Result<ServerT> {
    Server::bind(addr)
}

pub fn accept(server: &mut ServerT) -> io::Result<(Arc<WsAdaptor>, String)> {
    loop {
        if let Ok(s) = server.accept() {
            let uri = s.uri();
            if let Ok(s) = s.accept() {
                return Ok((Arc::new(WsAdaptor::new(s)?), uri));
            }
        }
    }
}

pub fn connect(url: &str) -> Result<Arc<WsAdaptor>, WebSocketError> {
    Ok(Arc::new(WsAdaptor::new(
        ClientBuilder::new(url).unwrap().connect_insecure()?
    ).map_err(|e| WebSocketError::IoError(e))?))
}