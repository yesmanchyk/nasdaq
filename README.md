# NASDAQ ITCH 5.0 / OUCH 5.0 Trading Ecosystem

A multi-language low-latency trading simulation and market-data ecosystem implementing **NASDAQ ITCH 5.0** over **RFC 3208 PGM Reliable Multicast (IPv4 UDP)** and **NASDAQ OUCH 5.0** over **TCP**, with competing Rust and C++20 trading bots and a real-time desktop GUI terminal.

## Repository Structure

| Directory | Description |
| :--- | :--- |
| [`itch-ouch-sim/`](itch-ouch-sim/README.md) | Single-threaded Rust price-time priority matching engine (`exchange`), 4 multi-strategy Rust bots (`bot`, including Avellaneda-Stoikov MM, Microprice/OBI Alpha, and Online RL Q-Learning + UCB1 Bandit), all-in-one simulator (`sim`), real-time `eframe`/`egui` desktop trading terminal (`itch_client`), and profit-per-ms plotting scripts. |
| [`cpp-trading-bot/`](cpp-trading-bot/README.md) | Zero-dependency C++20 low-latency trading client modeled after *Building Low Latency Applications with C++* (`LFQueue`, `MemPool`, `MarketOrderBook`, `FeatureEngine`, `RiskManager`, `TradeEngine`), wired natively to `itch-ouch-sim` over PGM ITCH 5.0 and TCP OUCH 5.0. Supports both default header builds and opt-in C++20 Modules (`-DUSE_CXX_MODULES=ON`). |
| [`packt-itch-ouch-adapter/`](packt-itch-ouch-adapter/README.md) | Adapter that compiles and runs the **unmodified upstream Packt Chapter 12 `TradeEngine`** from `external/Building-Low-Latency-Applications-with-CPP` directly against the Rust `itch-ouch-sim` exchange. |
| `external/Building-Low-Latency-Applications-with-CPP` | Git submodule pointing to [`PacktPublishing/Building-Low-Latency-Applications-with-CPP`](https://github.com/PacktPublishing/Building-Low-Latency-Applications-with-CPP). |
| `ItchReader/`, `ItchSender/`, `ItchReceiver/` | Standalone C++ ITCH 5.0 parser and Asio-based TCP feed sender/receiver prototypes (uses `ItchSender/asio` submodule). |

---

## Cloning & Submodules

This repository uses Git submodules for the upstream Packt C++ low-latency trading source (`external/Building-Low-Latency-Applications-with-CPP`) and standalone Asio (`ItchSender/asio`).

Clone with submodules in one step:

```bash
git clone --recurse-submodules https://github.com/yesmanchyk/nasdaq.git
cd nasdaq
```

If you already cloned the repository without `--recurse-submodules`, initialize and fetch all submodules with:

```bash
git submodule update --init --recursive
```

---

## Quick Start

### 1. Run All Five Bots Together (`sim`)

The all-in-one `sim` launcher starts the single-threaded Rust Exchange along with **all 5 competing bots** (each with an equal **$100,000.00** starting capital):
- **Bot 1** (Rust): Market Maker + Mean Reversion
- **Bot 2** (Rust): Momentum Sniper + Liquidity Taker
- **Bot 3** (Rust): Smart Avellaneda-Stoikov MM + Microprice/OBI Alpha
- **Bot 4** (Rust): Online Reinforcement Learning (Tabular Q-Learning MM + Contextual UCB1 Bandit)
- **Bot 5** (C++20): Low-Latency `TradeEngine` (`cpp-trading-bot`, automatically discovered or built via CMake before trading starts)

```bash
# Run the Exchange + all 5 bots (4 Rust + 1 C++20) for 10 seconds
cargo run --manifest-path itch-ouch-sim/Cargo.toml --release --bin sim -- 10000

# Optional: point Bot 5 to a specific binary (e.g., the C++20 Modules build)
CPP_TRADING_BOT_BIN=cpp-trading-bot/build-modules/cpp_trading_bot \
  cargo run --manifest-path itch-ouch-sim/Cargo.toml --release --bin sim -- 10000

# Optional: run only the 4 Rust bots (disable C++20 Bot 5)
cargo run --manifest-path itch-ouch-sim/Cargo.toml --release --bin sim -- 10000 --no-cpp
```

To launch the real-time desktop GUI terminal instead (includes a 1-click Built-in Sim):

```bash
cargo run --manifest-path itch-ouch-sim/Cargo.toml --release --bin itch_client
```

To plot historical profit-per-ms across all 5 bots from `itch-ouch-sim/sim_history/runs.csv`:

```bash
python3 itch-ouch-sim/scripts/plot_profit_per_ms.py --html
```

### 2. C++20 Low-Latency Trading Bot (`cpp-trading-bot`)

**Default header build (CMake 3.16+):**

```bash
cmake -S cpp-trading-bot -B cpp-trading-bot/build -DCMAKE_BUILD_TYPE=Release
cmake --build cpp-trading-bot/build -j
ctest --test-dir cpp-trading-bot/build --output-on-failure
```

**Opt-in C++20 Modules build (CMake 3.28+, Ninja, Clang 16+ / GCC 14+ / MSVC):**

```bash
cmake -S cpp-trading-bot -B cpp-trading-bot/build-modules -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DUSE_CXX_MODULES=ON
cmake --build cpp-trading-bot/build-modules -j
ctest --test-dir cpp-trading-bot/build-modules --output-on-failure
```

Run against the live Rust exchange:

```bash
./cpp-trading-bot/build/cpp_trading_bot --id 5 --duration-ms 8000 --history-csv itch-ouch-sim/sim_history/runs.csv
```

### 3. Upstream Packt Chapter 12 Adapter (`packt-itch-ouch-adapter`)

Requires `git submodule update --init --recursive` first so `external/Building-Low-Latency-Applications-with-CPP` is populated:

```bash
cmake -S packt-itch-ouch-adapter -B packt-itch-ouch-adapter/build -DCMAKE_BUILD_TYPE=Release
cmake --build packt-itch-ouch-adapter/build -j

# Run against the live Rust exchange
./packt-itch-ouch-adapter/build/packt_itch_ouch_bot --id 6 --algo MAKER --duration-ms 8000
```
