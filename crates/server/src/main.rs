mod connection;
mod md5;
mod proto;
mod session;

use connection::Connection;
use session::DbHandle;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

static NEXT_PID: AtomicU32 = AtomicU32::new(1);

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:7432".to_string());

    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("failed to bind {}: {}", addr, e);
        std::process::exit(1);
    });

    eprintln!("oigrap listening on {}", addr);

    let db = Arc::new(Mutex::new(DbHandle::new().unwrap_or_else(|e| {
        eprintln!("failed to initialize database: {}", e);
        std::process::exit(1);
    })));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
                let db = db.clone();
                thread::spawn(move || {
                    let peer = stream.peer_addr().ok();
                    if let Err(e) = Connection::new(stream, db, pid).and_then(|mut c| c.run()) {
                        if e.kind() != std::io::ErrorKind::UnexpectedEof
                            && e.kind() != std::io::ErrorKind::BrokenPipe
                            && e.kind() != std::io::ErrorKind::ConnectionReset
                        {
                            eprintln!("connection {:?} error: {}", peer, e);
                        }
                    }
                });
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
}
