//! Multi-Process / End-to-End Launcher (`sim`)
//!
//! Spawns the standalone `exchange` server and two standalone `bot` clients
//! connected via TCP OUCH (`127.0.0.1:40000`) and PGM Reliable Multicast ITCH
//! (`239.255.0.1:30001`, NAK repair on `127.0.0.1:30002`).

use itch_ouch_sim::bot::{default_strategies_for_bot, spawn_bot};
use itch_ouch_sim::exchange::ExchangeServer;
use itch_ouch_sim::pgm::{
    DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR, DEFAULT_OUCH_ADDR,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let duration_ms: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);

    println!("====================================================================================================");
    println!("  Standalone Single-Threaded Exchange (PGM ITCH + TCP OUCH) + 2 Multi-Strategy Bots (Rust)");
    println!("====================================================================================================");

    // 1. Bind standalone Exchange Server (TCP OUCH + PGM Multicast ITCH + PGM NAK repair)
    let mut exchange = ExchangeServer::bind(
        DEFAULT_OUCH_ADDR,
        DEFAULT_MCAST_GROUP,
        DEFAULT_MCAST_PORT,
        DEFAULT_NAK_ADDR,
    )?;
    let ouch_addr = exchange.local_ouch_addr()?.to_string();
    println!(
        "-> Exchange Online | TCP OUCH: {} | PGM ITCH Multicast: {}:{} | PGM NAK: {}",
        ouch_addr, DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR
    );

    let running = Arc::new(AtomicBool::new(true));

    // 2. Run the Single-Threaded Exchange Server
    let ex_running = Arc::clone(&running);
    let exchange_thread = thread::Builder::new()
        .name("exchange-1t".into())
        .spawn(move || {
            exchange.run(ex_running);
            exchange
        })?;

    // 3. Connect Bot 1 and Bot 2 via PGM Multicast ITCH & TCP OUCH
    let (name1, strat1) = default_strategies_for_bot(1);
    let bot1 = spawn_bot(
        name1,
        1,
        &ouch_addr,
        DEFAULT_MCAST_GROUP,
        DEFAULT_MCAST_PORT,
        DEFAULT_NAK_ADDR,
        strat1,
        Arc::clone(&running),
    )?;

    let (name2, strat2) = default_strategies_for_bot(2);
    let bot2 = spawn_bot(
        name2,
        2,
        &ouch_addr,
        DEFAULT_MCAST_GROUP,
        DEFAULT_MCAST_PORT,
        DEFAULT_NAK_ADDR,
        strat2,
        Arc::clone(&running),
    )?;

    println!("-> Connected Bot 1 [MarketMaker || MeanReversion] and Bot 2 [MomentumSniper || LiquidityTaker]\n");

    let start = Instant::now();
    let step = Duration::from_millis(400);
    while start.elapsed() < Duration::from_millis(duration_ms) {
        thread::sleep(step);
        println!(
            "t={:4}ms | {}\n         | {}",
            start.elapsed().as_millis(),
            bot1.state.summary(),
            bot2.state.summary()
        );
    }

    running.store(false, Ordering::Relaxed);
    let bot1_final = bot1.state.summary();
    let bot2_final = bot2.state.summary();
    bot1.join();
    bot2.join();
    let exchange = exchange_thread.join().unwrap();

    println!("\n====================================================================================================");
    println!("  FINAL SIMULATION SUMMARY ({} ms)", duration_ms);
    println!("====================================================================================================");
    println!("Exchange : {}", exchange.summary());
    println!("Bot 1    : {}", bot1_final);
    println!("Bot 2    : {}", bot2_final);

    Ok(())
}
