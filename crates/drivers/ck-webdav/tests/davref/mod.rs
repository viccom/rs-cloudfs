//! dav-server 参照桩（Phase 7 / WD3 conformance）——**独立第二实现**。
//!
//! 「桩照协议真形建模，绝不照驱动实现抄」纪律（K74/K77.2）的另一条
//! 腿：conformance 断言由**真实现**（dav-server 0.11 + LocalFs 真文件
//! 系统）判定，而非本仓手搓桩——驱动的协议面必须对第三方实现同样成
//! 立。装配面照抄 `crates/cloudkit-webdav/src/server.rs`（hyper 1.x
//! accept loop + graceful shutdown + FakeLs + 显式 method 集），差异：
//!
//! - filesystem = `LocalFs` 放 tempdir（真文件系统语义——MKCOL/MOVE/
//!   DELETE 的状态码与目录语义由 dav-server 生产代码给出）；
//! - 无认证（conformance 驱动无凭据）；
//! - **断言⑤注入面**：`fail_next_propfind(status)` 在 DavHandler 之前
//!   拦截恰一个 PROPFIND 回绝（真实 handler 不被触碰，一次性——注入
//!   消费后即恢复）。
//!
//! 共享测试支撑模块：与手搓桩 `stub/mod.rs` 同款豁免 dead_code。

#![allow(dead_code)]

use std::convert::Infallible;
use std::pin::pin;
use std::sync::{Arc, Mutex};

use dav_server::body::Body as DavBody;
use dav_server::fakels::FakeLs;
use dav_server::localfs::LocalFs;
use dav_server::{DavHandler, DavMethod, DavMethodSet};
use http::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// 参照桩把手：基地址（尾斜杠形态——直接作 `webdav_url`）+ 注入面 +
/// 观测面 + 优雅停机。
pub struct DavRefHandle {
    /// 桩基地址。
    pub url: String,
    /// 断言⑤注入位（Some(status) = 下一个 PROPFIND 回绝该码）。
    fault: Arc<Mutex<Option<u16>>>,
    /// 请求记录（"METHOD uri -> status" 到达序——调试/断言观测面）。
    log: Arc<Mutex<Vec<String>>>,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// tempdir 活到 shutdown（LocalFs 的文件系统根）。
    _dir: tempfile::TempDir,
}

impl DavRefHandle {
    /// 断言⑤注入：下一个 PROPFIND 以该状态码回绝（一次性——消费即
    /// 复位；真实 DavHandler 不被触碰）。
    pub fn fail_next_propfind(&self, status: u16) {
        *lock_fault(&self.fault) = Some(status);
    }

    /// 请求记录快照（"METHOD uri -> status" 到达序）。
    pub fn requests(&self) -> Vec<String> {
        lock_log(&self.log).clone()
    }

    /// 优雅停机（server.rs 同款语义：accept loop 停收、在途连接排空）。
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// 故障位读取（毒锁恢复——server.rs 同款纪律）。
fn lock_fault(fault: &Mutex<Option<u16>>) -> std::sync::MutexGuard<'_, Option<u16>> {
    fault
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 日志读取（毒锁恢复同上）。
fn lock_log(log: &Mutex<Vec<String>>) -> std::sync::MutexGuard<'_, Vec<String>> {
    log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 起参照桩：tempdir + LocalFs + `:0` 端口 + 无认证。
pub async fn spawn_davref() -> DavRefHandle {
    let dir = tempfile::tempdir().expect("davref tempdir");
    let handler = DavHandler::builder()
        .filesystem(LocalFs::new(dir.path(), false, false, false))
        .locksystem(FakeLs::new())
        .methods(method_set())
        .build_handler();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("davref bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("davref local addr");

    let fault = Arc::new(Mutex::new(None));
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (shutdown, rx) = watch::channel(false);
    let task = tokio::spawn(accept_loop(
        listener,
        handler,
        fault.clone(),
        log.clone(),
        rx,
    ));

    DavRefHandle {
        url: format!("http://{addr}/"),
        fault,
        log,
        shutdown,
        task: Some(task),
        _dir: dir,
    }
}

/// accept loop（server.rs `accept_loop` 的注入面版：service_fn 闭包里
/// 先查故障位再交真实 handler；收尾记录 "METHOD uri -> status"）。
async fn accept_loop(
    listener: TcpListener,
    handler: DavHandler,
    fault: Arc<Mutex<Option<u16>>>,
    log: Arc<Mutex<Vec<String>>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let (stream, _peer) = tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(_) => break,
            },
        };
        let handler = handler.clone();
        let fault = fault.clone();
        let log = log.clone();
        let mut conn_shutdown = shutdown.clone();
        tokio::task::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let handler = handler.clone();
                let fault = fault.clone();
                let log = log.clone();
                async move {
                    let wire = format!("{} {}", req.method(), req.uri());
                    // 断言⑤注入面：armed 且是 PROPFIND → canned 回绝
                    //（一次性；真实 handler 不被触碰）。
                    if req.method().as_str() == "PROPFIND" {
                        let armed = lock_fault(&fault).take();
                        if let Some(status) = armed {
                            let code = http::StatusCode::from_u16(status)
                                .expect("injected status is a valid u16 code");
                            lock_log(&log).push(format!("{wire} -> {}", code.as_u16()));
                            return Ok::<_, Infallible>(
                                http::Response::builder()
                                    .status(code)
                                    .body(DavBody::empty())
                                    .expect("static canned fault response"),
                            );
                        }
                    }
                    let response = handler.handle(req).await;
                    lock_log(&log).push(format!("{wire} -> {}", response.status().as_u16()));
                    Ok(response)
                }
            });
            let mut conn =
                pin!(http1::Builder::new().serve_connection(TokioIo::new(stream), service));
            tokio::select! {
                _ = conn_shutdown.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = conn.as_mut().await;
                }
                _ = conn.as_mut() => {}
            }
        });
    }
}

/// method 集（server.rs `method_set` 同款——LOCK/UNLOCK 随 FakeLs 就位，
/// PROPPATCH 答多状态；conformance 驱动从不发 LOCK/PROPPATCH，全集
/// 只为贴近生产装配形态）。
fn method_set() -> DavMethodSet {
    let mut set = DavMethodSet::none();
    for method in [
        DavMethod::PropFind,
        DavMethod::PropPatch,
        DavMethod::Get,
        DavMethod::Head,
        DavMethod::Put,
        DavMethod::Delete,
        DavMethod::Options,
        DavMethod::MkCol,
        DavMethod::Move,
        DavMethod::Copy,
        DavMethod::Lock,
        DavMethod::Unlock,
    ] {
        set.add(method);
    }
    set
}
