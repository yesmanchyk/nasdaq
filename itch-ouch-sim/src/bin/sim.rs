//! Multi-Process / End-to-End Launcher (`sim`)
//!
//! Spawns the standalone `exchange` server and four standalone `bot` clients
//! connected via TCP OUCH (`127.0.0.1:40000`) and PGM Reliable Multicast ITCH
//! (`239.255.0.1:30001`, NAK repair on `127.0.0.1:30002`).

use itch_ouch_sim::bot::{BotRunPerformance, append_run_history, default_strategies_for_bot, spawn_bot};
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
    println!("  Standalone Single-Threaded Exchange (PGM ITCH + TCP OUCH) + 4 Multi-Strategy Bots (Rust)");
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

    // 3. Connect Bot 1, Bot 2, Bot 3 (Smart), and Bot 4 (RL) via PGM Multicast ITCH & TCP OUCH
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

    let (name3, strat3) = default_strategies_for_bot(3);
    let bot3 = spawn_bot(
        name3,
        3,
        &ouch_addr,
        DEFAULT_MCAST_GROUP,
        DEFAULT_MCAST_PORT,
        DEFAULT_NAK_ADDR,
        strat3,
        Arc::clone(&running),
    )?;

    let (name4, strat4) = default_strategies_for_bot(4);
    let bot4 = spawn_bot(
        name4,
        4,
        &ouch_addr,
        DEFAULT_MCAST_GROUP,
        DEFAULT_MCAST_PORT,
        DEFAULT_NAK_ADDR,
        strat4,
        Arc::clone(&running),
    )?;

    println!(
        "-> Connected Bot 1 [MM || MeanRev], Bot 2 [Mom || Taker], Bot 3 [Smart AS || OBI], Bot 4 [RL Q-Learn || Bandit]\n"
    );

    let start = Instant::now();
    let step = Duration::from_millis(400);
    while start.elapsed() < Duration::from_millis(duration_ms) {
        thread::sleep(step);
        println!(
            "t={:4}ms | {}\n         | {}\n         | {}\n         | {}",
            start.elapsed().as_millis(),
            bot1.state.summary(),
            bot2.state.summary(),
            bot3.state.summary(),
            bot4.state.summary()
        );
    }

    running.store(false, Ordering::Relaxed);
    let bot1_final = bot1.state.summary();
    let bot2_final = bot2.state.summary();
    let bot3_final = bot3.state.summary();
    let bot4_final = bot4.state.summary();

    let bot_perf_records = vec![
        BotRunPerformance {
            bot_id: 1,
            name: bot1.state.name.clone(),
            pnl: bot1.state.pnl_snapshot(),
        },
        BotRunPerformance {
            bot_id: 2,
            name: bot2.state.name.clone(),
            pnl: bot2.state.pnl_snapshot(),
        },
        BotRunPerformance {
            bot_id: 3,
            name: bot3.state.name.clone(),
            pnl: bot3.state.pnl_snapshot(),
        },
        BotRunPerformance {
            bot_id: 4,
            name: bot4.state.name.clone(),
            pnl: bot4.state.pnl_snapshot(),
        },
    ];

    bot1.join();
    bot2.join();
    bot3.join();
    bot4.join();
    let exchange = exchange_thread.join().unwrap();

    let actual_duration_ms = start.elapsed().as_millis() as u64;
    let history_note = match append_run_history("sim", actual_duration_ms, &bot_perf_records) {
        Ok(path) => format!("Saved run performance history to {}", path.display()),
        Err(e) => format!("Warning: failed to append run performance history: {e}"),
    };

    println!("\n====================================================================================================");
    println!("  FINAL SIMULATION SUMMARY ({} ms)", duration_ms);
    println!("====================================================================================================");
    println!("Exchange : {}", exchange.summary());
    println!("Bot 1    : {}", bot1_final);
    println!("Bot 2    : {}", bot2_final);
    println!("Bot 3    : {}", bot3_final);
    println!("Bot 4    : {}", bot4_final);
    println!("History  : {}", history_note);

    Ok(())
}
