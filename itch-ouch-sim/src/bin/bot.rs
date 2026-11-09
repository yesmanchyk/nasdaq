//! Standalone Multi-Strategy Trading Bot Binary (`bot`)
//!
//! - Joins PGM (RFC 3208) Multicast Group (`239.255.0.1:30001`) for ITCH 5.0 Market Data
//! - Sends PGM NAK repair requests to `127.0.0.1:30002` on sequence gaps
//! - Connects over TCP (`TCP_NODELAY`) to `127.0.0.1:40000` for OUCH 5.0 Order Entry
//! - Runs 2 concurrent strategy threads sharing the shadow book and OUCH MPSC gateway
//!
//! Usage:
//!   cargo run --release --bin bot -- --id 1
//!   cargo run --release --bin bot -- --id 2
//!   cargo run --release --bin bot -- --id 3

use itch_ouch_sim::bot::{default_strategies_for_bot, spawn_bot};
use itch_ouch_sim::pgm::{
    DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR, DEFAULT_OUCH_ADDR,
};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let mut bot_id: u64 = 1;
    let mut ouch_addr = DEFAULT_OUCH_ADDR.to_string();
    let mut mcast_group = DEFAULT_MCAST_GROUP;
    let mut mcast_port = DEFAULT_MCAST_PORT;
    let mut nak_addr = DEFAULT_NAK_ADDR.to_string();
    let mut duration_ms: u64 = 2_000;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--id" => {
                if let Some(v) = args.next()
                    && let Ok(id) = v.parse::<u64>()
                {
                    bot_id = id;
                }
            }
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
                if let Some(v) = args.next()
                    && let Ok(ms) = v.parse::<u64>()
                {
                    duration_ms = ms;
                }
            }
            _ => {}
        }
    }

    let (name, strategies) = default_strategies_for_bot(bot_id);
    let running = Arc::new(AtomicBool::new(true));

    let bot = spawn_bot(
        name,
        bot_id,
        &ouch_addr,
        mcast_group,
        mcast_port,
        &nak_addr,
        strategies,
        Arc::clone(&running),
    )?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(duration_ms) {
        thread::sleep(Duration::from_millis(400));
        println!("t={:4}ms | {}", start.elapsed().as_millis(), bot.state.summary());
    }

    running.store(false, Ordering::Relaxed);
    let final_summary = bot.state.summary();
    bot.join();
    println!("[BOT {} FINAL] {}", bot_id, final_summary);
    Ok(())
}
