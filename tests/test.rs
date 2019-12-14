
use std::sync::Arc;
use easy_rpc::*;

struct ClientService;
struct ServerService;

const RECURSIVE_ADD: u32 = 1;
const ECHO_BIGDATA: u32 = 2;

easy_service! {
    ClientService(self, _ss, arg, ret)

    IntegerMethod {
        RECURSIVE_ADD => (val: u32) {
            println!("Client Received {:?}", val);
            val + 1
        }
    }
}

easy_service! {
    ServerService(self, ss, arg, ret)

    IntegerMethod {
        RECURSIVE_ADD => (val: u32) {
            println!("Server Received {:?}", val);
            let val = ss.request(RECURSIVE_ADD, &val).into::<u32>()?;
            println!("Server Requested {:?}", val);
            val + 1
        }
        ECHO_BIGDATA => (data: Vec<u8>) {
            println!("BigData Received {}", data.len());
            data
        }
    }
}

fn session_test(session: &Session) {
    let val: u32 = session.request(RECURSIVE_ADD, 0).into().unwrap();
    assert_eq!(val, 2);

    const LEN: usize = 0x10000;
    let tail = &[1, 2, 3];
    let mut data: Vec<u8> = Vec::with_capacity(LEN);
    data.resize(LEN, 0); data.extend_from_slice(tail);

    let data: Vec<u8> = session.request(ECHO_BIGDATA, &data).into().unwrap();
    assert_eq!(data.len(), LEN + tail.len());
    assert_eq!(&data[data.len() - tail.len()..], tail);
}

#[test]
fn test_ws() {
    std::thread::spawn(|| {
        let mut ser = ws::bind("127.0.0.1:3333").unwrap();
        let (adaptor, _uri) = ws::accept(&mut ser).unwrap();
        let s = Session::new(adaptor, Arc::new(ServerService));
        s.loop_handle();
    });

    std::thread::sleep_ms(100);
    let session = Session::new(ws::connect("ws://127.0.0.1:3333").unwrap(), Arc::new(ClientService));
    session_test(&session);
}

#[test]
#[ignore]
fn test_js() {
    let mut ser = ws::bind("127.0.0.1:3333").unwrap();
    let (adaptor, _uri) = ws::accept(&mut ser).unwrap();
    let session = Session::new(adaptor, Arc::new(ServerService));
    session.loop_handle();
}

#[test]
fn test_shm() {
    std::thread::spawn(move || {
        let adaptor = shm::create("sharememory_test").unwrap();
        adaptor.wait(None);
        let s = Session::new(adaptor, Arc::new(ServerService));
        s.loop_handle();
    });

    std::thread::sleep_ms(100);
    let s = Session::new(shm::connect("sharememory_test").unwrap(), Arc::new(ClientService));
    session_test(&s);
}