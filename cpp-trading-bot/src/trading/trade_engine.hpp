#pragma once

#include "common/lf_queue.hpp"
#include "common/protocol.hpp"
#include "trading/feature_engine.hpp"
#include "trading/market_order_book.hpp"
#include "trading/risk_manager.hpp"

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <mutex>
#include <optional>
#include <string>
#include <thread>

namespace trading {

enum class AlgoMode {
    Both,           // MarketMaker + LiquidityTaker (full book dual strategy)
    MarketMaker,    // Microprice passive quoting only
    LiquidityTaker, // Aggressive trade-ratio sniper only
};

struct TradeEngineConfig {
    uint64_t bot_id{5};
    std::string name{"Bot-5 (C++ LowLat)"};
    AlgoMode mode{AlgoMode::Both};
    uint32_t mm_clip{80};
    uint32_t taker_clip{45};
    double mm_threshold_fp{4.0};     // Fair-value edge threshold in fixed-point ticks
    double taker_ratio_thresh{0.35}; // Aggressive trade ratio threshold
    int64_t max_position{600};
};

/// Single-threaded Zero-Allocation Hot-Path TradeEngine.
/// Polls `LFQueue<ItchMessage>` and `LFQueue<OuchOutbound>`, updates `MarketOrderBook`,
/// `FeatureEngine`, and `RiskManager`, and dispatches orders from `MarketMaker` and
/// `LiquidityTaker` to `LFQueue<OuchInbound>`.
///
/// Modeled directly after `Trading::TradeEngine` from
/// "Building Low Latency Applications with C++".
class TradeEngine final {
public:
    TradeEngine(
        TradeEngineConfig cfg,
        common::LFQueue<common::ItchMessage>* md_queue,
        common::LFQueue<common::OuchInbound>* outgoing_requests,
        common::LFQueue<common::OuchOutbound>* incoming_responses)
        : cfg_(std::move(cfg)),
          md_queue_(md_queue),
          outgoing_requests_(outgoing_requests),
          incoming_responses_(incoming_responses),
          risk_manager_(
              common::INITIAL_BOT_CAPITAL_FP,
              RiskConfig{200, cfg_.max_position, 5'000 * static_cast<int64_t>(common::PRICE_SCALE)}),
          next_cl_ord_id_(cfg_.bot_id * 1'000'000 + 1) {}

    ~TradeEngine() { stop(); }

    void start() {
        run_.store(true, std::memory_order_release);
        thread_ = std::thread(&TradeEngine::runLoop, this);
    }

    void stop() {
        run_.store(false, std::memory_order_release);
        if (thread_.joinable()) {
            thread_.join();
        }
    }

    PositionSnapshot getSnapshot() const {
        std::lock_guard<std::mutex> lock(telemetry_mtx_);
        return last_snapshot_;
    }

    uint64_t itchPackets() const noexcept { return itch_packets_.load(std::memory_order_relaxed); }

private:
    struct RestingQuote {
        uint64_t cl_ord_id{0};
        uint64_t price{0};
        bool active{false};
    };

    void sendCancel(uint64_t cl_ord_id) noexcept {
        common::OuchInbound cancel{};
        cancel.type = common::OuchInboundType::CancelOrder;
        cancel.cl_ord_id = cl_ord_id;
        outgoing_requests_->push(cancel);
    }

    uint64_t sendNewOrder(common::Side side, uint32_t requested_shares, uint64_t price) noexcept {
        const uint64_t mark = book_.midPrice();
        const uint32_t allowed =
            risk_manager_.checkPreTradeRisk(side, requested_shares, price, mark);
        if (allowed == 0) return 0;

        common::OuchInbound order{};
        order.type = common::OuchInboundType::EnterOrder;
        order.cl_ord_id = next_cl_ord_id_++;
        order.side = side;
        order.shares = allowed;
        order.price = price;
        if (outgoing_requests_->push(order)) {
            return order.cl_ord_id;
        }
        return 0;
    }

    /// Book's `MarketMaker::onOrderBookUpdate`:
    /// Quotes passively around volume-weighted microprice (`FeatureEngine::getMktPrice`)
    /// with inventory skew and queue-priority preservation.
    void runMarketMaker(uint64_t tick_count) noexcept {
        const auto& bbo = book_.getBBO();
        const double fair_px = feature_engine_.getMktPrice();
        if (!bbo.isValid() || fair_px <= 0.0) return;

        const int64_t pos = risk_manager_.position();
        const double obi = feature_engine_.getObi();

        // Inventory skew: lean quotes to mean-revert position toward 0
        const double inv_skew = static_cast<double>(pos) * 0.04;
        const double reservation = fair_px + 4.0 * obi - inv_skew;

        uint64_t target_bid =
            (reservation - static_cast<double>(bbo.bid_price) >= cfg_.mm_threshold_fp)
                ? bbo.bid_price
                : (bbo.bid_price > 2 ? bbo.bid_price - 2 : bbo.bid_price);
        uint64_t target_ask =
            (static_cast<double>(bbo.ask_price) - reservation >= cfg_.mm_threshold_fp)
                ? bbo.ask_price
                : bbo.ask_price + 2;

        // Keep passive quotes strictly non-crossing
        if (target_bid >= bbo.ask_price) target_bid = bbo.ask_price - 1;
        if (target_ask <= bbo.bid_price) target_ask = bbo.bid_price + 1;
        if (target_ask <= target_bid) target_ask = target_bid + 2;

        const bool force_refresh = (tick_count % 8 == 0);

        // Manage resting Bid
        if (pos < cfg_.max_position) {
            const bool need_new_bid =
                !resting_bid_.active || force_refresh ||
                (resting_bid_.price > target_bid ? resting_bid_.price - target_bid
                                                 : target_bid - resting_bid_.price) > 2;
            if (need_new_bid) {
                if (resting_bid_.active) {
                    sendCancel(resting_bid_.cl_ord_id);
                    resting_bid_.active = false;
                }
                if (uint64_t id = sendNewOrder(common::Side::Buy, cfg_.mm_clip, target_bid)) {
                    resting_bid_ = RestingQuote{id, target_bid, true};
                }
            }
        } else if (resting_bid_.active) {
            sendCancel(resting_bid_.cl_ord_id);
            resting_bid_.active = false;
        }

        // Manage resting Ask
        if (pos > -cfg_.max_position) {
            const bool need_new_ask =
                !resting_ask_.active || force_refresh ||
                (resting_ask_.price > target_ask ? resting_ask_.price - target_ask
                                                 : target_ask - resting_ask_.price) > 2;
            if (need_new_ask) {
                if (resting_ask_.active) {
                    sendCancel(resting_ask_.cl_ord_id);
                    resting_ask_.active = false;
                }
                if (uint64_t id = sendNewOrder(common::Side::Sell, cfg_.mm_clip, target_ask)) {
                    resting_ask_ = RestingQuote{id, target_ask, true};
                }
            }
        } else if (resting_ask_.active) {
            sendCancel(resting_ask_.cl_ord_id);
            resting_ask_.active = false;
        }
    }

    /// Book's `LiquidityTaker::onTradeUpdate`:
    /// Reacts to large aggressive executions (`agg_trade_qty_ratio >= threshold`)
    /// aligned with Order-Book Imbalance by crossing the BBO.
    void runLiquidityTaker() noexcept {
        const auto& bbo = book_.getBBO();
        const double ratio = feature_engine_.getAggTradeQtyRatio();
        if (!bbo.isValid() || ratio < cfg_.taker_ratio_thresh) return;

        const double obi = feature_engine_.getObi();
        const auto agg_side = feature_engine_.getLastAggressorSide();

        // Cancel previous short-lived taker order if still resting
        if (active_taker_.active) {
            sendCancel(active_taker_.cl_ord_id);
            active_taker_.active = false;
        }

        if (agg_side == common::Side::Buy && obi > 0.05) {
            if (uint64_t id = sendNewOrder(common::Side::Buy, cfg_.taker_clip, bbo.ask_price + 2)) {
                active_taker_ = RestingQuote{id, bbo.ask_price + 2, true};
            }
        } else if (agg_side == common::Side::Sell && obi < -0.05) {
            const uint64_t px = (bbo.bid_price > 2) ? (bbo.bid_price - 2) : bbo.bid_price;
            if (uint64_t id = sendNewOrder(common::Side::Sell, cfg_.taker_clip, px)) {
                active_taker_ = RestingQuote{id, px, true};
            }
        }
    }

    void handleOrderResponse(const common::OuchOutbound& rep) noexcept {
        switch (rep.type) {
            case common::OuchOutboundType::OrderAccepted:
                risk_manager_.onOrderAccepted();
                break;
            case common::OuchOutboundType::OrderCanceled:
                risk_manager_.onOrderCanceled();
                if (resting_bid_.active && resting_bid_.cl_ord_id == rep.cl_ord_id) {
                    resting_bid_.active = false;
                }
                if (resting_ask_.active && resting_ask_.cl_ord_id == rep.cl_ord_id) {
                    resting_ask_.active = false;
                }
                if (active_taker_.active && active_taker_.cl_ord_id == rep.cl_ord_id) {
                    active_taker_.active = false;
                }
                break;
            case common::OuchOutboundType::OrderExecuted:
                risk_manager_.onFill(rep.side, rep.shares, rep.price);
                break;
        }
    }

    void publishTelemetry() {
        const auto snap = risk_manager_.snapshot(book_.midPrice());
        std::lock_guard<std::mutex> lock(telemetry_mtx_);
        last_snapshot_ = snap;
    }

    void runLoop() {
        using Clock = std::chrono::steady_clock;
        auto last_mm_time = Clock::now();
        auto last_taker_time = Clock::now();
        auto last_telemetry = Clock::now();
        uint64_t mm_ticks = 0;

        while (run_.load(std::memory_order_acquire)) {
            bool did_work = false;

            // 1. Process all incoming OUCH execution reports first (highest priority)
            while (const auto* rep = incoming_responses_->getNextToRead()) {
                handleOrderResponse(*rep);
                incoming_responses_->updateReadIndex();
                did_work = true;
            }

            // 2. Process incoming ITCH 5.0 market data updates from lock-free queue
            while (const auto* itch = md_queue_->getNextToRead()) {
                const auto msg = *itch;
                md_queue_->updateReadIndex();
                did_work = true;

                book_.onMarketUpdate(msg);
                itch_packets_.store(book_.itchPackets(), std::memory_order_relaxed);
                feature_engine_.onOrderBookUpdate(book_);

                if (msg.type == common::ItchType::OrderExecuted) {
                    feature_engine_.onTradeUpdate(msg, book_);
                    const auto now = Clock::now();
                    if ((cfg_.mode == AlgoMode::Both || cfg_.mode == AlgoMode::LiquidityTaker) &&
                        (now - last_taker_time >= std::chrono::milliseconds(5))) {
                        runLiquidityTaker();
                        last_taker_time = now;
                    }
                }
            }

            // 3. Periodic MarketMaker quote refresh (~4ms cadence)
            const auto now = Clock::now();
            if ((cfg_.mode == AlgoMode::Both || cfg_.mode == AlgoMode::MarketMaker) &&
                (now - last_mm_time >= std::chrono::milliseconds(4))) {
                ++mm_ticks;
                runMarketMaker(mm_ticks);
                last_mm_time = now;
            }

            // 4. Snapshot telemetry for main thread (~50ms cadence)
            if (now - last_telemetry >= std::chrono::milliseconds(50)) {
                publishTelemetry();
                last_telemetry = now;
            }

            if (!did_work) {
                std::this_thread::yield();
            }
        }

        // Cancel resting quotes on clean exit
        if (resting_bid_.active) sendCancel(resting_bid_.cl_ord_id);
        if (resting_ask_.active) sendCancel(resting_ask_.cl_ord_id);
        if (active_taker_.active) sendCancel(active_taker_.cl_ord_id);
        publishTelemetry();
    }

    TradeEngineConfig cfg_;
    common::LFQueue<common::ItchMessage>* md_queue_;
    common::LFQueue<common::OuchInbound>* outgoing_requests_;
    common::LFQueue<common::OuchOutbound>* incoming_responses_;

    MarketOrderBook book_;
    FeatureEngine feature_engine_;
    RiskManager risk_manager_;

    uint64_t next_cl_ord_id_{1};
    RestingQuote resting_bid_{};
    RestingQuote resting_ask_{};
    RestingQuote active_taker_{};

    std::atomic<bool> run_{false};
    std::thread thread_;
    std::atomic<uint64_t> itch_packets_{0};

    mutable std::mutex telemetry_mtx_;
    PositionSnapshot last_snapshot_{};
};

} // namespace trading
