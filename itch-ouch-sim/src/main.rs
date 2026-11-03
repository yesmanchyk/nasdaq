mod bot;
mod exchange;
mod protocol;

use bot::{StrategyKind, spawn_bot};
use exchange::ExchangeServer;
use protocol::PRICE_SCALE;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let duration_ms: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);

    println!("==========================================================================================");
    println!("  NASDAQ ITCH 5.0 / OUCH 5.0 Single-Threaded Exchange + 2 Multi-Strategy Bots (Rust)");
    println!("==========================================================================================");

    // 1. Create kernel stream sockets for OUCH (order entry) and datagram sockets for ITCH (market data)
    let (ex_ouch_1, bot1_ouch) = UnixStream::pair()?;
    let (ex_ouch_2, bot2_ouch) = UnixStream::pair()?;
    let (ex_itch_1, bot1_itch) = UnixDatagram::pair()?;
    let (ex_itch_2, bot2_itch) = UnixDatagram::pair()?;

    // 2. Initialize the Single-Threaded Exchange Server
    let mut exchange =
        ExchangeServer::new_connected(vec![ex_ouch_1, ex_ouch_2], vec![ex_itch_1, ex_itch_2])?;
    println!(
        "-> Single-Threaded Exchange initialized | 2 OUCH Stream channels | 2 ITCH Datagram feeds"
    );

    let running = Arc::new(AtomicBool::new(true));

    // 3. Run the Exchange on a single dedicated thread (zero internal locks)
    let ex_running = Arc::clone(&running);
    let exchange_thread = thread::Builder::new()
        .name("exchange-1t".into())
        .spawn(move || {
            exchange.run(ex_running);
            exchange
        })?;

    // 4. Launch Bot 1 with 2 concurrent strategies:
    //    - Strategy A: Passive Market Maker (quotes +/- $0.0025 around mid with inventory skew)
    //    - Strategy B: Mean Reversion (fades deviations > $0.0030 from $100.0000 fair value)
    let bot1 = spawn_bot(
        "Bot-1 (MM + MeanRev)",
        1,
        bot1_ouch,
        bot1_itch,
        [
            StrategyKind::MarketMaker {
                half_spread: 25,
                lot_size: 100,
            },
            StrategyKind::MeanReversion {
                fair_value: 100 * PRICE_SCALE,
                threshold: 30,
            },
        ],
        Arc::clone(&running),
    )?;

    // 5. Launch Bot 2 with 2 concurrent strategies:
    //    - Strategy A: Momentum Sniper (follows ITCH execution flow imbalance)
    //    - Strategy B: Liquidity Taker (aggressively probes top of book)
    let bot2 = spawn_bot(
        "Bot-2 (Mom + Taker) ",
        2,
        bot2_ouch,
        bot2_itch,
        [
            StrategyKind::MomentumSniper { lot_size: 40 },
            StrategyKind::LiquidityTaker { lot_size: 60 },
        ],
        Arc::clone(&running),
    )?;

    println!("-> Spawned Bot 1 [MarketMaker || MeanReversion] and Bot 2 [MomentumSniper || LiquidityTaker]\n");

    // 6. Print live telemetry snapshots every 400ms
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

    // 7. Clean shutdown
    running.store(false, Ordering::Relaxed);
    let bot1_final = bot1.state.summary();
    let bot2_final = bot2.state.summary();
    bot1.join();
    bot2.join();
    let exchange = exchange_thread.join().unwrap();

    println!("\n==========================================================================================");
    println!("  FINAL SIMULATION SUMMARY ({} ms)", duration_ms);
    println!("==========================================================================================");
    println!("Exchange : {}", exchange.summary());
    println!("Bot 1    : {}", bot1_final);
    println!("Bot 2    : {}", bot2_final);

    Ok(())
}
