#pragma once

#include "common/protocol.hpp"
#include "trading/market_order_book.hpp"

#include <cmath>
#include <cstdint>

namespace trading {

inline constexpr double FEATURE_INVALID = -1.0e18;

/// Computes real-time microstructure signals on every ITCH tick:
/// 1. `mkt_price_`: Volume-weighted BBO microprice (in fixed-point units)
/// 2. `agg_trade_qty_ratio_`: Ratio of aggressive trade shares to resting BBO shares
/// 3. `obi_`: 5-level exponentially weighted Order Book Imbalance
///
/// Modeled directly after `Trading::FeatureEngine` from
/// "Building Low Latency Applications with C++".
class FeatureEngine final {
public:
    void onOrderBookUpdate(const MarketOrderBook& book) noexcept {
        const auto& bbo = book.getBBO();
        if (bbo.isValid()) {
            const double bid_px = static_cast<double>(bbo.bid_price);
            const double ask_px = static_cast<double>(bbo.ask_price);
            const double bid_qty = static_cast<double>(bbo.bid_qty);
            const double ask_qty = static_cast<double>(bbo.ask_qty);
            const double total = bid_qty + ask_qty;
            if (total > 0.0) {
                mkt_price_ = (bid_px * ask_qty + ask_px * bid_qty) / total;
            }
        }
        obi_ = book.orderBookImbalance(5);
    }

    void onTradeUpdate(const common::ItchMessage& trade, const MarketOrderBook& book) noexcept {
        const auto& bbo = book.getBBO();
        if (bbo.isValid()) {
            // In ITCH OrderExecuted, `trade.side` is the resting order's side:
            // resting Sell executed => aggressive Buy lifted the ask
            // resting Buy executed  => aggressive Sell hit the bid
            const uint32_t ref_qty =
                (trade.side == common::Side::Sell) ? bbo.ask_qty : bbo.bid_qty;
            if (ref_qty > 0) {
                agg_trade_qty_ratio_ =
                    static_cast<double>(trade.shares) / static_cast<double>(ref_qty);
            } else {
                agg_trade_qty_ratio_ = 1.0;
            }
            last_aggressor_side_ =
                (trade.side == common::Side::Sell) ? common::Side::Buy : common::Side::Sell;
        }
    }

    double getMktPrice() const noexcept { return mkt_price_; }
    double getAggTradeQtyRatio() const noexcept { return agg_trade_qty_ratio_; }
    double getObi() const noexcept { return obi_; }
    common::Side getLastAggressorSide() const noexcept { return last_aggressor_side_; }

private:
    double mkt_price_{FEATURE_INVALID};
    double agg_trade_qty_ratio_{FEATURE_INVALID};
    double obi_{0.0};
    common::Side last_aggressor_side_{common::Side::Buy};
};

} // namespace trading
