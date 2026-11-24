#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <string>
#include <thread>

#ifdef USE_CXX_MODULES
import common;
import trading;
#else
#include "common/lf_queue.hpp"
#include "common/protocol.hpp"
#include "trading/market_data_consumer.hpp"
#include "trading/order_gateway.hpp"
#include "trading/trade_engine.hpp"
#endif

namespace {

std::atomic<bool> g_running{true};

void handleSignal(int) {
    g_running.store(false, std::memory_order_release);
}

std::pair<std::string, uint16_t> parseHostPort(
    const std::string& addr,
    const std::string& default_host,
    uint16_t default_port) {
    auto pos = addr.rfind(':');
    if (pos == std::string::npos) {
        return {default_host, default_port};
    }
    return {addr.substr(0, pos), static_cast<uint16_t>(std::stoi(addr.substr(pos + 1)))};
}

void appendHistoryCsv(
    const std::string& history_csv,
    uint64_t duration_ms,
    uint64_t bot_id,
    const std::string& bot_name,
    const trading::PositionSnapshot& p) {
    namespace fs = std::filesystem;
    fs::path path(history_csv);
    if (path.has_parent_path()) {
        std::error_code ec;
        fs::create_directories(path.parent_path(), ec);
    }
    const bool is_new = !fs::exists(path) || fs::file_size(path) == 0;
    std::ofstream out(path, std::ios::app);
    if (!out) return;

    if (is_new) {
        out << "timestamp_unix_s,source,duration_ms,bot_id,bot_name,start_cap_dollars,"
               "equity_dollars,mtm_pnl_dollars,cash_dollars,position_shares,"
               "fills,acks,cancels,rl_q_updates,rl_bandit_updates\n";
    }
    const auto ts = std::chrono::duration_cast<std::chrono::seconds>(
                        std::chrono::system_clock::now().time_since_epoch())
                        .count();
    const double start_cap = static_cast<double>(p.initial_capital_fp) / common::PRICE_SCALE;
    const double equity = static_cast<double>(p.equity_fp) / common::PRICE_SCALE;
    const double pnl = static_cast<double>(p.mtm_pnl_fp) / common::PRICE_SCALE;
    const double cash = static_cast<double>(p.cash_fp) / common::PRICE_SCALE;

    out << ts << ",cpp_bot," << duration_ms << "," << bot_id << "," << bot_name << ","
        << std::fixed << std::setprecision(2) << start_cap << "," << equity << "," << pnl << ","
        << cash << "," << p.position << "," << p.fills << "," << p.acks << "," << p.cancels
        << ",0,0\n";
}

void printSummaryLine(
    const std::string& prefix,
    const std::string& name,
    uint64_t itch_pkts,
    uint64_t rdata,
    uint64_t naks,
    const trading::PositionSnapshot& s) {
    const double equity = static_cast<double>(s.equity_fp) / common::PRICE_SCALE;
    const double pnl = static_cast<double>(s.mtm_pnl_fp) / common::PRICE_SCALE;
    std::cout << prefix << "[" << name << "] PGM_ITCH: " << std::setw(4) << itch_pkts
              << " (RDATA:" << rdata << "/NAK:" << naks << ")"
              << " | Acks: " << std::setw(4) << s.acks
              << " | Cancels: " << std::setw(4) << s.cancels
              << " | Fills: " << std::setw(4) << s.fills
              << " | Pos: " << std::showpos << std::setw(5) << s.position << std::noshowpos << " sh"
              << " | Equity: $" << std::fixed << std::setprecision(2) << std::setw(10) << equity
              << " | MTM PnL: $" << std::showpos << std::setw(9) << pnl << std::noshowpos
              << std::endl;
}

} // namespace

int main(int argc, char* argv[]) {
    std::signal(SIGINT, handleSignal);
    std::signal(SIGTERM, handleSignal);

    uint64_t bot_id = 5;
    std::string ouch_addr = "127.0.0.1:40000";
    std::string mcast_group = common::DEFAULT_MCAST_GROUP;
    uint16_t mcast_port = common::DEFAULT_MCAST_PORT;
    std::string nak_addr = "127.0.0.1:30002";
    uint64_t duration_ms = 2'000;
    trading::AlgoMode mode = trading::AlgoMode::Both;
    std::string history_csv;

    for (int i = 1; i < argc; ++i) {
        std::string arg = argv[i];
        if (arg == "--id" && i + 1 < argc) {
            bot_id = std::stoull(argv[++i]);
        } else if (arg == "--ouch" && i + 1 < argc) {
            ouch_addr = argv[++i];
        } else if (arg == "--mcast-group" && i + 1 < argc) {
            mcast_group = argv[++i];
        } else if (arg == "--mcast-port" && i + 1 < argc) {
            mcast_port = static_cast<uint16_t>(std::stoi(argv[++i]));
        } else if (arg == "--nak" && i + 1 < argc) {
            nak_addr = argv[++i];
        } else if (arg == "--duration-ms" && i + 1 < argc) {
            duration_ms = std::stoull(argv[++i]);
        } else if (arg == "--mode" && i + 1 < argc) {
            std::string m = argv[++i];
            if (m == "mm") mode = trading::AlgoMode::MarketMaker;
            else if (m == "taker") mode = trading::AlgoMode::LiquidityTaker;
            else mode = trading::AlgoMode::Both;
        } else if (arg == "--history-csv" && i + 1 < argc) {
            history_csv = argv[++i];
        } else if (arg == "--help" || arg == "-h") {
            std::cout
                << "Low-Latency C++20 Trading Bot (Building-Low-Latency-Applications-with-CPP Architecture)\n"
                << "Usage: cpp_trading_bot [options]\n"
                << "  --id <N>              Bot client ID prefix (default: 5)\n"
                << "  --ouch <host:port>    Exchange TCP OUCH address (default: 127.0.0.1:40000)\n"
                << "  --mcast-group <ip>    PGM ITCH multicast group (default: 239.255.0.1)\n"
                << "  --mcast-port <port>   PGM ITCH multicast port (default: 30001)\n"
                << "  --nak <host:port>     PGM NAK repair address (default: 127.0.0.1:30002)\n"
                << "  --duration-ms <ms>    Run duration in ms, 0 = run until Ctrl+C (default: 2000)\n"
                << "  --mode <both|mm|taker> Strategy mode (default: both)\n"
                << "  --history-csv <path>  Optional CSV file to append final run performance\n";
            return 0;
        }
    }

    auto [ouch_host, ouch_port] =
        parseHostPort(ouch_addr, common::DEFAULT_OUCH_HOST, common::DEFAULT_OUCH_PORT);
    auto [nak_host, nak_port] =
        parseHostPort(nak_addr, common::DEFAULT_NAK_HOST, common::DEFAULT_NAK_PORT);

    trading::TradeEngineConfig cfg{};
    cfg.bot_id = bot_id;
    cfg.name = "Bot-" + std::to_string(bot_id) + " (C++ LowLat)";
    cfg.mode = mode;

    // Lock-Free SPSC Queues connecting I/O threads to the Zero-Allocation TradeEngine
    common::LFQueue<common::ItchMessage> md_queue(65'536);
    common::LFQueue<common::OuchInbound> outgoing_requests(16'384);
    common::LFQueue<common::OuchOutbound> incoming_responses(16'384);

    try {
        trading::MarketDataConsumer md_consumer(
            mcast_group, mcast_port, nak_host, nak_port, &md_queue);
        trading::OrderGateway order_gateway(
            ouch_host, ouch_port, &outgoing_requests, &incoming_responses);
        trading::TradeEngine trade_engine(
            cfg, &md_queue, &outgoing_requests, &incoming_responses);

        std::cout << "-> Starting " << cfg.name << " | TCP OUCH: " << ouch_host << ":" << ouch_port
                  << " | PGM ITCH: " << mcast_group << ":" << mcast_port
                  << " | NAK: " << nak_host << ":" << nak_port
                  << " | Capital: $" << common::INITIAL_BOT_CAPITAL_DOLLARS << ".00" << std::endl;

        md_consumer.start();
        order_gateway.start();
        trade_engine.start();

        const auto start = std::chrono::steady_clock::now();
        while (g_running.load(std::memory_order_acquire)) {
            std::this_thread::sleep_for(std::chrono::milliseconds(400));
            const auto elapsed_ms = static_cast<uint64_t>(
                std::chrono::duration_cast<std::chrono::milliseconds>(
                    std::chrono::steady_clock::now() - start)
                    .count());
            std::string prefix = "t=" + std::to_string(elapsed_ms) + "ms | ";
            printSummaryLine(
                prefix,
                cfg.name,
                trade_engine.itchPackets(),
                md_consumer.rdataRecv(),
                md_consumer.naksSent(),
                trade_engine.getSnapshot());
            if (duration_ms > 0 && elapsed_ms >= duration_ms) {
                break;
            }
        }

        trade_engine.stop();
        order_gateway.stop();
        md_consumer.stop();

        const auto actual_ms = static_cast<uint64_t>(
            std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - start)
                .count());
        const auto final_snap = trade_engine.getSnapshot();
        printSummaryLine(
            "[C++ BOT FINAL] ",
            cfg.name,
            trade_engine.itchPackets(),
            md_consumer.rdataRecv(),
            md_consumer.naksSent(),
            final_snap);

        if (!history_csv.empty()) {
            appendHistoryCsv(history_csv, actual_ms, bot_id, cfg.name, final_snap);
            std::cout << "-> Appended C++ bot run performance to " << history_csv << std::endl;
        }
    } catch (const std::exception& ex) {
        std::cerr << "Error: " << ex.what() << std::endl;
        return 1;
    }

    return 0;
}
