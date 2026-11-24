# Upstream Packt Chapter 12 TradeEngine -> PGM ITCH / TCP OUCH Adapter

This adapter compiles and runs the **actual unmodified upstream C++ source code** from [`PacktPublishing/Building-Low-Latency-Applications-with-CPP`](https://github.com/PacktPublishing/Building-Low-Latency-Applications-with-CPP) (`external/Building-Low-Latency-Applications-with-CPP/Chapter12/trading/strategy/`) directly against your Rust Exchange (`itch-ouch-sim`).

## What Runs from Upstream Packt Chapter 12 (Unmodified)
- `Trading::TradeEngine` (`trade_engine.cpp` / `trade_engine.h`)
- `Trading::MarketOrderBook` & `MarketOrder` (`market_order_book.cpp`, `market_order.cpp`)
- `Trading::FeatureEngine` (`feature_engine.h`)
- `Trading::MarketMaker` (`market_maker.cpp` / `market_maker.h`)
- `Trading::LiquidityTaker` (`liquidity_taker.cpp` / `liquidity_taker.h`)
- `Trading::OrderManager` (`order_manager.cpp` / `order_manager.h`)
- `Trading::PositionKeeper` & `Trading::RiskManager` (`position_keeper.h`, `risk_manager.cpp`)
- `Common::LFQueue`, `Common::MemPool`, `Common::Logger`

## What This Adapter Provides
- **`ItchOuchMarketDataConsumer`** (`src/itch_ouch_md_consumer.hpp`): Joins your Rust Exchange's PGM Reliable Multicast ITCH 5.0 stream (`239.255.0.1:30001`, NAK `127.0.0.1:30002`), translates `'A'`/`'E'`/`'D'` ITCH frames into Packt's `Exchange::MEMarketUpdate` (`ADD`, `TRADE` + `MODIFY`/`CANCEL`), and pushes them into Packt's `Exchange::MEMarketUpdateLFQueue`.
- **`ItchOuchOrderGateway`** (`src/itch_ouch_order_gw.hpp`): Bridges Packt's `Exchange::ClientRequestLFQueue` / `Exchange::ClientResponseLFQueue` to your Rust Exchange's 22-byte / 30-byte TCP OUCH 5.0 frames (`127.0.0.1:40000`), enforcing the equal **\$100,000.00** starting capital limit.

## Build & Run

```bash
# Ensure submodule is initialized
git submodule update --init --recursive

# Build the upstream Packt TradeEngine + ITCH/OUCH adapter
cmake -S packt-itch-ouch-adapter -B packt-itch-ouch-adapter/build -DCMAKE_BUILD_TYPE=Release
cmake --build packt-itch-ouch-adapter/build -j

# Run live against the Rust Exchange + 4 Rust Bots:
# Terminal 1:
cargo run --manifest-path itch-ouch-sim/Cargo.toml --release -- 10000
# Terminal 2 (Upstream Packt MarketMaker):
./packt-itch-ouch-adapter/build/packt_itch_ouch_bot --id 6 --algo MAKER --duration-ms 8000
# Terminal 3 (Upstream Packt LiquidityTaker):
./packt-itch-ouch-adapter/build/packt_itch_ouch_bot --id 7 --algo TAKER --threshold 0.25 --duration-ms 8000
```
