#include "common/logging.h"
#include "itch_ouch_md_consumer.hpp"
#include "itch_ouch_order_gw.hpp"
#include "strategy/trade_engine.h"

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <string>
#include <thread>

namespace {

std::atomic<bool> g_running{true};

void handleSignal(int) {
    g_running.store(false, std::memory_order_release);
}

std::pair<std::string, uint16_t> parseHostPort(
    const std::string& addr, const std::string& def_host, uint16_t def_port) {
    auto pos = addr.rfind(':');
    if (pos == std::string::npos) return {def_host, def_port};
    return {addr.substr(0, pos), static_cast<uint16_t>(std::stoi(addr.substr(pos + 1)))};
}

void printSummary(
    const std::string& prefix,
    const std::string& name,
    uint64_t itch_pkts,
    uint64_t rdata,
    uint64_t naks,
    uint64_t mark_fp,
    const adapter::GatewayTelemetry& t) {
    const int64_t equity_fp = t.cash_fp + t.position * static_cast<int64_t>(mark_fp);
    const int64_t mtm_pnl_fp = equity_fp - wire::INITIAL_CAPITAL_FP;
    const double equity = static_cast<double>(equity_fp) / wire::PRICE_SCALE;
    const double pnl = static_cast<double>(mtm_pnl_fp) / wire::PRICE_SCALE;

    std::cout << prefix << "[" << name << "] PGM_ITCH: " << std::setw(4) << itch_pkts
              << " (RDATA:" << rdata << "/NAK:" << naks << ")"
              << " | Acks: " << std::setw(4) << t.acks
              << " | Cancels: " << std::setw(4) << t.cancels
              << " | Fills: " << std::setw(4) << t.fills
              << " | Pos: " << std::showpos << std::setw(5) << t.position << std::noshowpos << " sh"
              << " | Equity: $" << std::fixed << std::setprecision(2) << std::setw(10) << equity
              << " | MTM PnL: $" << std::showpos << std::setw(9) << pnl << std::noshowpos
              << std::endl;
}

void appendHistoryCsv(
    const std::string& history_csv,
    uint64_t duration_ms,
    uint64_t bot_id,
    const std::string& bot_name,
    uint64_t mark_fp,
    const adapter::GatewayTelemetry& t) {
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
    const int64_t equity_fp = t.cash_fp + t.position * static_cast<int64_t>(mark_fp);
    const int64_t mtm_pnl_fp = equity_fp - wire::INITIAL_CAPITAL_FP;
    out << ts << ",packt_ch12," << duration_ms << "," << bot_id << "," << bot_name << ","
        << std::fixed << std::setprecision(2)
        << static_cast<double>(wire::INITIAL_CAPITAL_DOLLARS) << ","
        << static_cast<double>(equity_fp) / wire::PRICE_SCALE << ","
        << static_cast<double>(mtm_pnl_fp) / wire::PRICE_SCALE << ","
        << static_cast<double>(t.cash_fp) / wire::PRICE_SCALE << ","
        << t.position << "," << t.fills << "," << t.acks << "," << t.cancels << ",0,0\n";
}

} // namespace

int main(int argc, char* argv[]) {
    std::signal(SIGINT, handleSignal);
    std::signal(SIGTERM, handleSignal);

    Common::ClientId client_id = 6;
    std::string algo_str = "MAKER";
    std::string ouch_addr = "127.0.0.1:40000";
    std::string mcast_group = "239.255.0.1";
    uint16_t mcast_port = 30001;
    std::string nak_addr = "127.0.0.1:30002";
    uint64_t duration_ms = 2'000;
    Common::Qty clip = 75;
    double threshold = 0.5;
    std::string history_csv;

    for (int i = 1; i < argc; ++i) {
        std::string arg = argv[i];
        if (arg == "--id" && i + 1 < argc) {
            client_id = static_cast<Common::ClientId>(std::stoul(argv[++i]));
        } else if (arg == "--algo" && i + 1 < argc) {
            algo_str = argv[++i]; // MAKER or TAKER
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
        } else if (arg == "--clip" && i + 1 < argc) {
            clip = static_cast<Common::Qty>(std::stoul(argv[++i]));
        } else if (arg == "--threshold" && i + 1 < argc) {
            threshold = std::stod(argv[++i]);
        } else if (arg == "--history-csv" && i + 1 < argc) {
            history_csv = argv[++i];
        } else if (arg == "--help" || arg == "-h") {
            std::cout
                << "Upstream Packt Chapter 12 TradeEngine -> PGM ITCH / TCP OUCH Adapter\n"
                << "Usage: packt_itch_ouch_bot [options]\n"
                << "  --id <N>              Client ID (default: 6)\n"
                << "  --algo <MAKER|TAKER>  Upstream Packt AlgoType (default: MAKER)\n"
                << "  --ouch <host:port>    Exchange TCP OUCH address (default: 127.0.0.1:40000)\n"
                << "  --mcast-group <ip>    PGM ITCH multicast group (default: 239.255.0.1)\n"
                << "  --mcast-port <port>   PGM ITCH multicast port (default: 30001)\n"
                << "  --nak <host:port>     PGM NAK address (default: 127.0.0.1:30002)\n"
                << "  --clip <shares>       Order clip size (default: 75)\n"
                << "  --threshold <val>     Feature threshold (default: 0.5)\n"
                << "  --duration-ms <ms>    Duration in ms, 0 = until Ctrl+C (default: 2000)\n"
                << "  --history-csv <path>  Optional CSV to append final run performance\n";
            return 0;
        }
    }

    const auto algo_type = Common::stringToAlgoType(algo_str);
    auto [ouch_host, ouch_port] = parseHostPort(ouch_addr, "127.0.0.1", 40000);
    auto [nak_host, nak_port] = parseHostPort(nak_addr, "127.0.0.1", 30002);

    // Configure upstream Packt TradeEngine for TickerId 0
    Common::TradeEngineCfgHashMap ticker_cfg;
    ticker_cfg.at(0) = {
        clip,
        threshold,
        {
            200,         // max_order_size_
            600,         // max_position_
            -50'000'000.0 // max_loss_ (in fixed-point units: -$5,000.00)
        }};

    // Real upstream Packt lock-free queues
    Exchange::ClientRequestLFQueue client_requests(Common::ME_MAX_CLIENT_UPDATES);
    Exchange::ClientResponseLFQueue client_responses(Common::ME_MAX_CLIENT_UPDATES);
    Exchange::MEMarketUpdateLFQueue market_updates(Common::ME_MAX_MARKET_UPDATES);

    const std::string bot_name =
        "Bot-" + std::to_string(client_id) + " (Packt " + algo_str + ")";

    try {
        std::cout << "-> Starting Upstream Packt Chapter 12 TradeEngine [" << bot_name
                  << "] | TCP OUCH: " << ouch_host << ":" << ouch_port
                  << " | PGM ITCH: " << mcast_group << ":" << mcast_port
                  << " | Capital: $" << wire::INITIAL_CAPITAL_DOLLARS << ".00" << std::endl;

        Trading::TradeEngine trade_engine(
            client_id, algo_type, ticker_cfg, &client_requests, &client_responses, &market_updates);
        adapter::ItchOuchOrderGateway order_gw(
            client_id, ouch_host, ouch_port, &client_requests, &client_responses);
        adapter::ItchOuchMarketDataConsumer md_consumer(
            mcast_group, mcast_port, nak_host, nak_port, &market_updates);

        trade_engine.start();
        order_gw.start();
        md_consumer.start();

        const auto start = std::chrono::steady_clock::now();
        while (g_running.load(std::memory_order_acquire)) {
            std::this_thread::sleep_for(std::chrono::milliseconds(400));
            const auto elapsed_ms = static_cast<uint64_t>(
                std::chrono::duration_cast<std::chrono::milliseconds>(
                    std::chrono::steady_clock::now() - start)
                    .count());
            printSummary(
                "t=" + std::to_string(elapsed_ms) + "ms | ",
                bot_name,
                md_consumer.itchPackets(),
                md_consumer.rdataRecv(),
                md_consumer.naksSent(),
                md_consumer.lastMidPrice(),
                order_gw.telemetry());
            if (duration_ms > 0 && elapsed_ms >= duration_ms) break;
        }

        // Stop producers first so upstream Packt TradeEngine::stop() drains queues cleanly
        md_consumer.stop();
        order_gw.stop();
        trade_engine.stop();

        const auto actual_ms = static_cast<uint64_t>(
            std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - start)
                .count());
        const auto final_t = order_gw.telemetry();
        printSummary(
            "[PACKT BOT FINAL] ",
            bot_name,
            md_consumer.itchPackets(),
            md_consumer.rdataRecv(),
            md_consumer.naksSent(),
            md_consumer.lastMidPrice(),
            final_t);

        if (!history_csv.empty()) {
            appendHistoryCsv(
                history_csv, actual_ms, client_id, bot_name, md_consumer.lastMidPrice(), final_t);
            std::cout << "-> Appended Packt bot performance to " << history_csv << std::endl;
        }
    } catch (const std::exception& ex) {
        std::cerr << "Error: " << ex.what() << std::endl;
        return 1;
    }

    return 0;
}
