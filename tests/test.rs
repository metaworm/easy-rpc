
use std::sync::Arc;
use easy_rpc::*;

struct ClientService;
struct ServerService;

const ECHO: Method = Method::Int(1);
const BIGDATA: Method = Method::Int(2);

impl Service for ClientService {
    fn handle(&self, ss: &Session, arg: Arg, ret: Ret) -> Result<(), HandleError> {
        match arg.method {
            ECHO => {
                let val = arg.into::<u32>()?;
                println!("Client Received {:?}", val);
                ret(val + 1);
            }
            _ => { /* Err("MethodNotFound".to_string()) */ }
        }
        Ok(())
    }
}

impl Service for ServerService {
    fn handle(&self, ss: &Session, arg: Arg, ret: Ret) -> Result<(), HandleError> {
        match arg.method {
            ECHO => {
                let val: u32 = arg.into()?;
                println!("Server Received {:?}", val);
                let val = ss.request(ECHO, &val).into::<u32>()?;
                println!("Server Requested {:?}", val);
                ret(val + 1);
            }
            BIGDATA => {
                let data: Vec<u8> = arg.into()?;
                println!("BigData Received {}", data.len());
                ret(data);
            }
            _ => { /* Err("MethodNotFound".to_string()) */ }
        }
        Ok(())
    }
}

fn session_test(session: &Session) {
    let val: u32 = session.request(ECHO, 0).into().unwrap();
    assert_eq!(val, 2);

    const LEN: usize = 0x10000;
    let tail = &[1, 2, 3];
    let mut data: Vec<u8> = Vec::with_capacity(LEN); data.resize(LEN, 0);
    data.extend_from_slice(tail);
    let data: Vec<u8> = session.request(BIGDATA, &data).into().unwrap();
    assert_eq!(data.len(), LEN + tail.len());
    assert_eq!(&data[data.len() - tail.len()..], tail);
}

#[test]
fn test_ws() {
    use easy_rpc::ws::*;

    std::thread::spawn(|| {
        let mut ser = bind("127.0.0.1:3333").unwrap();
        let session = WsSession::new(accept(&mut ser), Arc::new(ServerService));
        session.loop_handle();
    });

    std::thread::sleep_ms(100);
    let session = WsSession::new(connect("ws://127.0.0.1:3333").unwrap(), Arc::new(ClientService));
    session_test(&session);
}

#[test]
#[ignore]
fn test_js() {
    use easy_rpc::ws::*;

    let mut ser = bind("127.0.0.1:3333").unwrap();
    let session = WsSession::new(accept(&mut ser), Arc::new(ServerService));
    session.loop_handle();
}

#[test]
fn test_shm() {
    std::thread::spawn(move || {
        let s = shm::create_and_wait("sharememory_test", Arc::new(ServerService)).unwrap();
        s.loop_handle();
    });

    std::thread::sleep_ms(100);
    let s = shm::connect("sharememory_test", Arc::new(ClientService)).unwrap();
    println!("connect success");
    session_test(&s);
}