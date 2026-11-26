//! Multi-Process / End-to-End Launcher (`sim`)
//!
//! Spawns the standalone `exchange` server, four Rust `bot` clients (Bots 1-4),
//! and the C++20 low-latency trading bot (`cpp_trading_bot`, Bot 5) connected via
//! TCP OUCH (`127.0.0.1:40000`) and PGM Reliable Multicast ITCH
//! (`239.255.0.1:30001`, NAK repair on `127.0.0.1:30002`).

use itch_ouch_sim::bot::{
    BotRunPerformance, RunOhlc, append_run_history_with_ohlc, default_strategies_for_bot,
    sim_history_dir, spawn_bot,
};
use itch_ouch_sim::exchange::ExchangeServer;
use itch_ouch_sim::pgm::{
    DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR, DEFAULT_OUCH_ADDR,
};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct CppBotHandle {
    child: Child,
    latest_summary: Arc<Mutex<String>>,
    final_summary: Arc<Mutex<Option<String>>>,
    reader_thread: Option<thread::JoinHandle<()>>,
}

impl CppBotHandle {
    fn summary(&self) -> String {
        self.latest_summary
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "[Bot-5 (C++ LowLat)] starting...".to_string())
    }

    fn wait_and_finish(mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                Err(_) => break,
            }
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        if let Ok(guard) = self.final_summary.lock()
            && let Some(ref line) = *guard
        {
            return line.clone();
        }
        self.summary()
    }
}

fn cpp_bot_binary_name() -> &'static str {
    if cfg!(windows) {
        "cpp_trading_bot.exe"
    } else {
        "cpp_trading_bot"
    }
}

/// Locates a compiled `cpp_trading_bot` binary or builds it on demand via CMake.
fn resolve_cpp_bot_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CPP_TRADING_BOT_BIN")
        && !explicit.trim().is_empty()
    {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }

    let bin = cpp_bot_binary_name();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        PathBuf::from("cpp-trading-bot/build").join(bin),
        PathBuf::from("cpp-trading-bot/build-modules").join(bin),
        PathBuf::from("../cpp-trading-bot/build").join(bin),
        PathBuf::from("../cpp-trading-bot/build-modules").join(bin),
        manifest_dir.join("../cpp-trading-bot/build").join(bin),
        manifest_dir
            .join("../cpp-trading-bot/build-modules")
            .join(bin),
    ];

    for path in &candidates {
        if path.is_file() {
            return Some(path.clone());
        }
    }

    // Attempt an automatic Release build if the C++ source tree is present.
    let cpp_src_dir = manifest_dir.join("../cpp-trading-bot");
    if !cpp_src_dir.join("CMakeLists.txt").is_file() {
        return None;
    }
    let cpp_build_dir = cpp_src_dir.join("build");
    println!(
        "-> C++ bot binary not found; building {} via CMake...",
        cpp_src_dir.display()
    );
    let configured = Command::new("cmake")
        .arg("-S")
        .arg(&cpp_src_dir)
        .arg("-B")
        .arg(&cpp_build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !configured {
        return None;
    }
    let built = Command::new("cmake")
        .arg("--build")
        .arg(&cpp_build_dir)
        .arg("--target")
        .arg("cpp_trading_bot")
        .arg("-j")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !built {
        return None;
    }

    let built_bin = cpp_build_dir.join(bin);
    if built_bin.is_file() {
        Some(built_bin)
    } else {
        None
    }
}

fn spawn_cpp_bot(
    bin_path: &Path,
    ouch_addr: &str,
    duration_ms: u64,
    history_csv: &Path,
) -> std::io::Result<CppBotHandle> {
    let mut child = Command::new(bin_path)
        .arg("--id")
        .arg("5")
        .arg("--ouch")
        .arg(ouch_addr)
        .arg("--mcast-group")
        .arg(DEFAULT_MCAST_GROUP.to_string())
        .arg("--mcast-port")
        .arg(DEFAULT_MCAST_PORT.to_string())
        .arg("--nak")
        .arg(DEFAULT_NAK_ADDR)
        .arg("--duration-ms")
        .arg(duration_ms.to_string())
        .arg("--history-csv")
        .arg(history_csv)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let latest_summary = Arc::new(Mutex::new("[Bot-5 (C++ LowLat)] connecting...".to_string()));
    let final_summary = Arc::new(Mutex::new(None));

    let stdout = child.stdout.take();
    let latest_clone = Arc::clone(&latest_summary);
    let final_clone = Arc::clone(&final_summary);
    let reader_thread = stdout.map(|out| {
        thread::Builder::new()
            .name("cpp-bot-stdout".into())
            .spawn(move || {
                let reader = BufReader::new(out);
                for line in reader.lines().map_while(Result::ok) {
                    if let Some(rest) = line.strip_prefix("[C++ BOT FINAL] ") {
                        if let Ok(mut f) = final_clone.lock() {
                            *f = Some(rest.to_string());
                        }
                        if let Ok(mut l) = latest_clone.lock() {
                            *l = rest.to_string();
                        }
                    } else if let Some((_, rest)) = line.split_once(" | [Bot-") {
                        if let Ok(mut l) = latest_clone.lock() {
                            *l = format!("[Bot-{rest}");
                        }
                    }
                }
            })
            .expect("spawn cpp-bot-stdout reader")
    });

    Ok(CppBotHandle {
        child,
        latest_summary,
        final_summary,
        reader_thread,
    })
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_cpp = args.iter().any(|a| a == "--no-cpp")
        || std::env::var("SIM_NO_CPP_BOT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let duration_ms: u64 = args
        .iter()
        .find_map(|s| s.parse::<u64>().ok())
        .unwrap_or(2_000);

    println!("====================================================================================================");
    println!("  Standalone Single-Threaded Exchange (PGM ITCH + TCP OUCH) + 4 Rust Bots + 1 C++20 Bot");
    println!("====================================================================================================");

    // Resolve (or build via CMake) the C++20 bot binary before starting the exchange
    // so Bots 1-4 do not trade ahead of Bot 5 during a cold build.
    let cpp_bin = if no_cpp {
        None
    } else {
        let resolved = resolve_cpp_bot_binary();
        if resolved.is_none() {
            eprintln!(
                "Warning: C++20 Bot 5 binary not found (build with `cmake -S cpp-trading-bot -B cpp-trading-bot/build && cmake --build cpp-trading-bot/build -j`). Running Rust bots only."
            );
        }
        resolved
    };

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

    // 4. Spawn Bot 5 (C++20 Low-Latency Trading Bot) unless disabled via --no-cpp
    let history_csv_path = sim_history_dir().join("runs.csv");
    let cpp_bot = if let Some(ref cpp_bin) = cpp_bin {
        match spawn_cpp_bot(cpp_bin, &ouch_addr, duration_ms, &history_csv_path) {
            Ok(handle) => {
                println!("-> Launched C++20 Bot 5 from {}", cpp_bin.display());
                Some(handle)
            }
            Err(e) => {
                eprintln!(
                    "Warning: failed to launch C++20 Bot 5 ({}): {e}",
                    cpp_bin.display()
                );
                None
            }
        }
    } else {
        None
    };

    if cpp_bot.is_some() {
        println!(
            "-> Connected Bot 1 [MM || MeanRev], Bot 2 [Mom || Taker], Bot 3 [Smart AS || OBI], Bot 4 [RL Q-Learn || Bandit], Bot 5 [C++20 LowLat]\n"
        );
    } else {
        println!(
            "-> Connected Bot 1 [MM || MeanRev], Bot 2 [Mom || Taker], Bot 3 [Smart AS || OBI], Bot 4 [RL Q-Learn || Bandit]\n"
        );
    }

    let start = Instant::now();
    let step = Duration::from_millis(400);
    while start.elapsed() < Duration::from_millis(duration_ms) {
        thread::sleep(step);
        if let Some(ref bot5) = cpp_bot {
            println!(
                "t={:4}ms | {}\n         | {}\n         | {}\n         | {}\n         | {}",
                start.elapsed().as_millis(),
                bot1.state.summary(),
                bot2.state.summary(),
                bot3.state.summary(),
                bot4.state.summary(),
                bot5.summary()
            );
        } else {
            println!(
                "t={:4}ms | {}\n         | {}\n         | {}\n         | {}",
                start.elapsed().as_millis(),
                bot1.state.summary(),
                bot2.state.summary(),
                bot3.state.summary(),
                bot4.state.summary()
            );
        }
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
    let bot5_final = cpp_bot.map(CppBotHandle::wait_and_finish);
    let exchange = exchange_thread.join().unwrap();

    let actual_duration_ms = start.elapsed().as_millis() as u64;
    let run_ohlc = exchange.stats.ohlc().map(RunOhlc::from);
    let history_note = match append_run_history_with_ohlc(
        "sim",
        actual_duration_ms,
        &bot_perf_records,
        run_ohlc,
    ) {
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
    if let Some(ref b5) = bot5_final {
        println!("Bot 5    : {}", b5);
    }
    println!("History  : {}", history_note);

    Ok(())
}
