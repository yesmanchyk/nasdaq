//! Standalone Single-Threaded Exchange Server Binary (`exchange`)
//!
//! - TCP OUCH 5.0 Order Entry & Execution Reports (`127.0.0.1:40000`)
//! - PGM (RFC 3208) Reliable Multicast ITCH 5.0 Publisher (`239.255.0.1:30001`)
//! - PGM NAK Repair Listener (`127.0.0.1:30002`)
//!
//! Usage:
//!   cargo run --release --bin exchange
//!   cargo run --release --bin exchange -- --duration-ms 5000

use itch_ouch_sim::exchange::ExchangeServer;
use itch_ouch_sim::pgm::{
    DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR, DEFAULT_OUCH_ADDR,
};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let mut ouch_addr = DEFAULT_OUCH_ADDR.to_string();
    let mut mcast_group = DEFAULT_MCAST_GROUP;
    let mut mcast_port = DEFAULT_MCAST_PORT;
    let mut nak_addr = DEFAULT_NAK_ADDR.to_string();
    let mut duration_ms: Option<u64> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ouch" => {
                if let Some(v) = args.next() {
                    ouch_addr = v;
                }
            }
            "--mcast-group" => {
                if let Some(v) = args.next()
                    && let Ok(ip) = v.parse::<Ipv4Addr>()
                {
                    mcast_group = ip;
                }
            }
            "--mcast-port" => {
                if let Some(v) = args.next()
                    && let Ok(p) = v.parse::<u16>()
                {
                    mcast_port = p;
                }
            }
            "--nak" => {
                if let Some(v) = args.next() {
                    nak_addr = v;
                }
            }
            "--duration-ms" => {
                if let Some(v) = args.next() {
                    duration_ms = v.parse().ok();
                }
            }
            _ => {}
        }
    }

    let mut server = ExchangeServer::bind(&ouch_addr, mcast_group, mcast_port, &nak_addr)?;
    println!(
        "[EXCHANGE] Standalone Single-Threaded Server Online\n  \
         -> TCP OUCH Listener : {}\n  \
         -> PGM ITCH Multicast: {}:{}\n  \
         -> PGM NAK Repair    : {}",
        server.local_ouch_addr()?,
        mcast_group,
        mcast_port,
        nak_addr
    );

    let running = Arc::new(AtomicBool::new(true));
    if let Some(ms) = duration_ms {
        let r = Arc::clone(&running);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(ms));
            r.store(false, Ordering::Relaxed);
        });
    }

    // Run the matching engine directly on the main thread (single-threaded matching core)
    server.run(running);

    println!("[EXCHANGE] Shutdown -> {}", server.summary());
    Ok(())
}
