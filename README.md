`easy-rpc` 是跨通信方式的Rust RPC框架，也有其他语言实现。

||Rust|JavaScript|
|--|--|--|
|WebSocket|✓|✓|
|SharedMem|✓||

WebSocket/JavaScript用于Rust和网页交互数据，共享内存(SharedMem)用于进程间通信。

## 优点

* 基于MsgPack，不需要协议文件，动态解析类型
* 通信双方可以递归地Request，类似本地的函数递归调用

## 缺点

`easy-rpc`(目前)不是异步实现，每一个会话都会占据一个线程。如果你用在有大量IO的服务端上，可能不太合适。

## 文档

有待完善。可以参考test里的例子，如果你用过serde系列库，应该会很容易上手。

## 注意事项

`easy-rpc`目前依赖一个git库，所以没有发布到 crates.io，使用时要通过git引用。

Cargo.toml

```toml
[package]
# ...

[dependencies]
# ...
easy-rpc = {git = 'https://github.com/metaworm/easy-rpc'}
```

## 示例

```rust
use std::sync::Arc;
use easy_rpc::*;

struct ServerService;

const MUL: u32 = 1;
easy_service! {
    ServerService(self, _ss, arg, response)

    StringMethod {
        "add" => (a: u32, b: u32) {
            a + b
        }
        "print" => (s: String) {
            println!("{}", s);
        }
    }
    IntegerMethod {
        MUL => (a: u32, b: u32) {
            a * b
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::thread::spawn(|| {
        let mut ser = ws::bind("127.0.0.1:3333").unwrap();
        let (adaptor, _uri) = ws::accept(&mut ser).unwrap();
        Session::new(adaptor, Arc::new(ServerService)).loop_handle();
    });

    std::thread::sleep_ms(100);
    let session = Session::new(ws::connect("ws://127.0.0.1:3333")?, Arc::new(EmptyService));
    let val: u32 = session.request("add", (1, 2)).into()?;
    session.notify("print", format!("the result is {}", val));
    let val: u32 = session.request(MUL, (2, 3)).into()?;
    assert_eq!(val, 6);

    Ok(())
}
```