# C++20 Low-Latency Trading Bot (`cpp-trading-bot`)

A zero-dependency C++20 low-latency trading client modeled after [`PacktPublishing/Building-Low-Latency-Applications-with-CPP`](https://github.com/PacktPublishing/Building-Low-Latency-Applications-with-CPP), wired natively to the Rust Exchange Server (`itch-ouch-sim`) over **PGM Reliable Multicast ITCH 5.0** and **TCP OUCH 5.0**.

## Architecture

- **`common::LFQueue<T>`** (`src/common/lf_queue.hpp`): Cache-line-aligned lock-free Single-Producer Single-Consumer (SPSC) ring buffers decoupling network I/O threads from the trading hot path.
- **`common::MemPool<T>`** (`src/common/mem_pool.hpp`): Pre-allocated contiguous memory pool for `MarketOrder` nodes inside the L3 book.
- **`common::protocol`** (`src/common/protocol.hpp`): Exact Big-Endian binary wire codec matching `itch-ouch-sim` (52-byte RFC 3208 PGM frames, 30-byte ITCH 5.0, 22-byte OUCH Inbound, 30-byte OUCH Outbound).
- **`trading::MarketDataConsumer`** (`src/trading/market_data_consumer.hpp`): Joins IPv4 multicast `239.255.0.1:30001`, tracks PGM sequence continuity, sends 52-byte `PGM_NAK` (`0x08`) repair requests to `127.0.0.1:30002`, and pushes in-order `ItchMessage` frames to `LFQueue`.
- **`trading::OrderGateway`** (`src/trading/order_gateway.hpp`): Non-blocking TCP (`TCP_NODELAY`) gateway to `127.0.0.1:40000`.
- **`trading::TradeEngine`** (`src/trading/trade_engine.hpp`): Single-threaded zero-allocation hot path combining `MarketOrderBook`, `FeatureEngine` (volume-weighted microprice, 5-level OBI, aggressive trade ratio), `RiskManager` (enforcing the equal **\$100,000.00** starting capital and $1\times$ equity buying power), **`MarketMaker`**, and **`LiquidityTaker`**.

## Build & Test

### Default Header Build (CMake 3.16+)

```bash
cd cpp-trading-bot
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
ctest --test-dir build --output-on-failure
```

### Opt-In C++20 Modules Build (`-DUSE_CXX_MODULES=ON`)

Requires **CMake 3.28+**, **Ninja** (or Visual Studio), and a C++20 Modules-capable compiler (**Clang 16+**, **GCC 14+**, or **MSVC**). Builds `modules/common.cppm` and `modules/trading.cppm` via `FILE_SET CXX_MODULES`:

```bash
cd cpp-trading-bot
cmake -S . -B build-modules -G Ninja \
  -DCMAKE_CXX_COMPILER=clang++ \
  -DCMAKE_BUILD_TYPE=Release \
  -DUSE_CXX_MODULES=ON
cmake --build build-modules -j
ctest --test-dir build-modules --output-on-failure
```

## Run Live Against the Rust Exchange & Rust Bots

Start the Rust `sim` (or standalone `exchange` + `bot`s) in Terminal 1:
```bash
cargo run --manifest-path itch-ouch-sim/Cargo.toml --release -- 10000
```

Launch the C++20 bot in Terminal 2 to compete head-to-head on the same order book:
```bash
./cpp-trading-bot/build/cpp_trading_bot --id 5 --duration-ms 8000 --history-csv itch-ouch-sim/sim_history/runs.csv
```
