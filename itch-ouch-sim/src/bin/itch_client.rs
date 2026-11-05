//! Standalone Real-Time ITCH 5.0 Market Data Client (`itch_client`)
//!
//! Connects to the Exchange's PGM Reliable Multicast group (RFC 3208),
//! reconstructs the Level-2 Order Book in real-time from ITCH 5.0 feeds,
//! streams live trade executions, and displays the top-of-book / market depth.
//!
//! Usage:
//!   cargo run --release --bin itch_client
//!   cargo run --release --bin itch_client -- --depth 10 --refresh-ms 200
//!   cargo run --release --bin itch_client -- --duration-ms 5000

use itch_ouch_sim::bot::ShadowBook;
use itch_ouch_sim::pgm::{
    DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR, PgmSubscriber,
};
use itch_ouch_sim::protocol::{ItchMessage, Side, fmt_price};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn print_book_snapshot(book: &ShadowBook, depth: usize, itch_msgs: u64, naks: u64, rdata: u64) {
    let best_bid = book.best_bid();
    let best_ask = book.best_ask();
    let mid = book.mid_price();
    let last = book.last_trade_price;
    let spread = book.spread();

    println!("\n========================================================================================");
    println!("  NASDAQ ITCH 5.0 REAL-TIME ORDER BOOK & MARKET DEPTH");
    println!("========================================================================================");

    let last_str = if last > 0 {
        format!("${}", fmt_price(last))
    } else {
        "N/A".to_string()
    };
    let mid_str = format!("${}", fmt_price(mid));
    let spread_str = if let Some(s) = spread {
        format!("${}", fmt_price(s))
    } else {
        "CROSS/NONE".to_string()
    };

    let bbo_str = match (best_bid, best_ask) {
        (Some((bp, bq)), Some((ap, aq))) => {
            format!("${} ({} sh)  x  ${} ({} sh)", fmt_price(bp), bq, fmt_price(ap), aq)
        }
        (Some((bp, bq)), None) => format!("${} ({} sh)  x  [No Asks]", fmt_price(bp), bq),
        (None, Some((ap, aq))) => format!("[No Bids]  x  ${} ({} sh)", fmt_price(ap), aq),
        (None, None) => "[Book Empty]".to_string(),
    };

    println!(
        "  CURRENT PRICE: {:<10} | MID: {:<10} | SPREAD: {:<10}",
        last_str, mid_str, spread_str
    );
    println!("  BEST BBO     : {}", bbo_str);
    println!(
        "  FEED STATS   : ITCH Packets: {} | Recovered RDATA: {} | NAK Requests: {}",
        itch_msgs, rdata, naks
    );
    println!("----------------------------------------------------------------------------------------");
    println!("   SIDE    |       PRICE |    SHARES | DEPTH CHART");
    println!("----------------------------------------------------------------------------------------");

    let asks = book.top_asks(depth);
    if asks.is_empty() {
        println!("   (no active sell orders)");
    } else {
        // Display asks reversed so higher asks are at top and Best Ask is right above the spread
        for (i, &(price, shares)) in asks.iter().enumerate().rev() {
            let bar_len = ((shares as usize) / 20).clamp(1, 35);
            let bar: String = "#".repeat(bar_len);
            println!(
                "   ASK {:<2} |   ${:>8} | {:>7} sh | {}",
                i + 1,
                fmt_price(price),
                shares,
                bar
            );
        }
    }

    println!("----------- SPREAD: {:^12} ------------------------------------------------", spread_str);

    let bids = book.top_bids(depth);
    if bids.is_empty() {
        println!("   (no active buy orders)");
    } else {
        for (i, &(price, shares)) in bids.iter().enumerate() {
            let bar_len = ((shares as usize) / 20).clamp(1, 35);
            let bar: String = "#".repeat(bar_len);
            println!(
                "   BID {:<2} |   ${:>8} | {:>7} sh | {}",
                i + 1,
                fmt_price(price),
                shares,
                bar
            );
        }
    }

    println!("----------------------------------------------------------------------------------------");
    println!(
        "  TOTAL DEPTH  : Bid: {} sh | Ask: {} sh",
        book.total_bid_shares(),
        book.total_ask_shares()
    );
    println!("========================================================================================");
}

fn main() -> std::io::Result<()> {
    let mut mcast_group = DEFAULT_MCAST_GROUP;
    let mut mcast_port = DEFAULT_MCAST_PORT;
    let mut nak_addr = DEFAULT_NAK_ADDR.to_string();
    let mut depth: usize = 5;
    let mut refresh_ms: u64 = 250;
    let mut duration_ms: Option<u64> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
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
            "--depth" => {
                if let Some(v) = args.next()
                    && let Ok(d) = v.parse::<usize>()
                {
                    depth = d.max(1);
                }
            }
            "--refresh-ms" => {
                if let Some(v) = args.next()
                    && let Ok(ms) = v.parse::<u64>()
                {
                    refresh_ms = ms.max(10);
                }
            }
            "--duration-ms" => {
                if let Some(v) = args.next() {
                    duration_ms = v.parse().ok();
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: itch_client [OPTIONS]\n\n\
                     Options:\n  \
                       --mcast-group <IP>    Multicast IP (default: 239.255.0.1)\n  \
                       --mcast-port <PORT>   Multicast port (default: 30001)\n  \
                       --nak <ADDR>          PGM NAK repair unicast address (default: 127.0.0.1:30002)\n  \
                       --depth <N>           Book depth levels to display (default: 5)\n  \
                       --refresh-ms <MS>     Throttle interval for book snapshots (default: 250)\n  \
                       --duration-ms <MS>    Optional execution limit in ms (default: run until stopped)"
                );
                return Ok(());
            }
            _ => {}
        }
    }

    println!(
        "[ITCH CLIENT] Subscribing to PGM ITCH Multicast {}:{} (NAK backchannel: {})...",
        mcast_group, mcast_port, nak_addr
    );
    let mut subscriber = PgmSubscriber::join(mcast_group, mcast_port, &nak_addr)?;
    println!("[ITCH CLIENT] Connected! Listening for real-time market data...\n");

    let running = Arc::new(AtomicBool::new(true));
    if let Some(ms) = duration_ms {
        let r = Arc::clone(&running);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(ms));
            r.store(false, Ordering::Relaxed);
        });
    }

    let mut book = ShadowBook::default();
    let mut batch = Vec::with_capacity(64);
    let mut total_itch_msgs: u64 = 0;
    let mut last_display = Instant::now();
    let refresh_interval = Duration::from_millis(refresh_ms);
    let mut book_dirty = false;

    while running.load(Ordering::Relaxed) {
        batch.clear();
        match subscriber.poll_itch(&mut batch) {
            Ok(true) => {
                for msg in batch.drain(..) {
                    total_itch_msgs += 1;
                    book.apply(msg);
                    book_dirty = true;

                    // Immediately stream trade executions in real-time
                    if let ItchMessage::OrderExecuted {
                        order_ref,
                        side,
                        executed_shares,
                        match_price,
                        ..
                    } = msg
                    {
                        let side_str = match side {
                            Side::Buy => "BUY ",
                            Side::Sell => "SELL",
                        };
                        println!(
                            "  >>> [REAL-TIME TRADE] {:>4} sh @ ${} | Side: {} | OrderRef: #{} | Current Price: ${}",
                            executed_shares,
                            fmt_price(match_price),
                            side_str,
                            order_ref,
                            fmt_price(match_price)
                        );
                    }
                }
            }
            Ok(false) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => {
                eprintln!("[ITCH CLIENT ERROR] {}", e);
                break;
            }
        }

        if book_dirty && last_display.elapsed() >= refresh_interval {
            print_book_snapshot(
                &book,
                depth,
                total_itch_msgs,
                subscriber.naks_sent,
                subscriber.rdata_recv,
            );
            last_display = Instant::now();
            book_dirty = false;
        }
    }

    println!("\n[ITCH CLIENT] Shutdown. Final Order Book state:");
    print_book_snapshot(
        &book,
        depth,
        total_itch_msgs,
        subscriber.naks_sent,
        subscriber.rdata_recv,
    );
    Ok(())
}
