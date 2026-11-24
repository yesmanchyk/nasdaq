#pragma once

#include "common/mem_pool.hpp"
#include "common/protocol.hpp"

#include <algorithm>
#include <cstdint>
#include <functional>
#include <map>
#include <optional>
#include <unordered_map>
#include <utility>

namespace trading {

struct MarketOrder {
    uint64_t order_ref{0};
    common::Side side{common::Side::Buy};
    uint64_t price{0};
    uint32_t qty{0};

    MarketOrder() = default;
    MarketOrder(uint64_t ref, common::Side s, uint64_t px, uint32_t q) noexcept
        : order_ref(ref), side(s), price(px), qty(q) {}
};

struct BBO {
    uint64_t bid_price{0};
    uint32_t bid_qty{0};
    uint64_t ask_price{0};
    uint32_t ask_qty{0};

    bool hasBid() const noexcept { return bid_price > 0 && bid_qty > 0; }
    bool hasAsk() const noexcept { return ask_price > 0 && ask_qty > 0; }
    bool isValid() const noexcept { return hasBid() && hasAsk() && ask_price >= bid_price; }
};

/// Level-3 Order Book reconstructed from ITCH 5.0 multicast updates using a
/// pre-allocated `MemPool<MarketOrder>` for zero-allocation order tracking.
/// Modeled after `Trading::MarketOrderBook` from "Building Low Latency Applications with C++".
class MarketOrderBook final {
public:
    explicit MarketOrderBook(std::size_t max_orders = 131'072)
        : order_pool_(max_orders) {
        order_map_.reserve(max_orders);
    }

    void onMarketUpdate(const common::ItchMessage& msg) noexcept {
        ++itch_packets_;
        switch (msg.type) {
            case common::ItchType::OrderAdded:
                addOrder(msg.order_ref, msg.side, msg.price, msg.shares);
                break;
            case common::ItchType::OrderExecuted:
                last_trade_price_ = msg.price;
                reduceOrder(msg.order_ref, msg.shares);
                break;
            case common::ItchType::OrderDeleted:
                removeOrder(msg.order_ref);
                break;
        }
        updateBBO();
    }

    const BBO& getBBO() const noexcept { return bbo_; }

    uint64_t midPrice() const noexcept {
        if (bbo_.hasBid() && bbo_.hasAsk()) {
            return (bbo_.bid_price + bbo_.ask_price) / 2;
        }
        if (bbo_.hasBid()) return bbo_.bid_price + 50;
        if (bbo_.hasAsk()) return (bbo_.ask_price > 50) ? (bbo_.ask_price - 50) : bbo_.ask_price;
        return (last_trade_price_ > 0) ? last_trade_price_ : (100 * common::PRICE_SCALE);
    }

    /// Multi-level exponentially weighted Order Book Imbalance (OBI) in `[-1.0, +1.0]`.
    double orderBookImbalance(std::size_t levels = 5) const noexcept {
        static constexpr double kWeights[5] = {1.0, 0.6065, 0.3679, 0.2231, 0.1353};
        double weighted_bid = 0.0;
        double weighted_ask = 0.0;

        std::size_t i = 0;
        for (const auto& [px, qty] : bids_) {
            if (i >= levels) break;
            const double w = (i < 5) ? kWeights[i] : 0.1;
            weighted_bid += static_cast<double>(qty) * w;
            ++i;
        }
        i = 0;
        for (const auto& [px, qty] : asks_) {
            if (i >= levels) break;
            const double w = (i < 5) ? kWeights[i] : 0.1;
            weighted_ask += static_cast<double>(qty) * w;
            ++i;
        }
        const double denom = weighted_bid + weighted_ask;
        if (denom <= 1e-9) return 0.0;
        return std::clamp((weighted_bid - weighted_ask) / denom, -1.0, 1.0);
    }

    uint64_t itchPackets() const noexcept { return itch_packets_; }
    uint64_t lastTradePrice() const noexcept { return last_trade_price_; }
    std::size_t activeOrders() const noexcept { return order_map_.size(); }

private:
    void addOrder(uint64_t order_ref, common::Side side, uint64_t price, uint32_t qty) noexcept {
        if (qty == 0) return;
        // If duplicate ref arrives, remove previous entry first
        if (order_map_.find(order_ref) != order_map_.end()) {
            removeOrder(order_ref);
        }
        if (MarketOrder* node = order_pool_.allocate(order_ref, side, price, qty)) {
            order_map_.emplace(order_ref, node);
            if (side == common::Side::Buy) {
                bids_[price] += qty;
            } else {
                asks_[price] += qty;
            }
        }
    }

    void reduceOrder(uint64_t order_ref, uint32_t delta) noexcept {
        auto it = order_map_.find(order_ref);
        if (it == order_map_.end()) return;
        MarketOrder* node = it->second;
        const uint32_t sub = std::min(node->qty, delta);
        node->qty -= sub;
        decrementLevel(node->side, node->price, sub);
        if (node->qty == 0) {
            order_map_.erase(it);
            order_pool_.deallocate(node);
        }
    }

    void removeOrder(uint64_t order_ref) noexcept {
        auto it = order_map_.find(order_ref);
        if (it == order_map_.end()) return;
        MarketOrder* node = it->second;
        decrementLevel(node->side, node->price, node->qty);
        order_map_.erase(it);
        order_pool_.deallocate(node);
    }

    void decrementLevel(common::Side side, uint64_t price, uint32_t qty) noexcept {
        if (side == common::Side::Buy) {
            auto lvl = bids_.find(price);
            if (lvl != bids_.end()) {
                if (lvl->second <= qty) {
                    bids_.erase(lvl);
                } else {
                    lvl->second -= qty;
                }
            }
        } else {
            auto lvl = asks_.find(price);
            if (lvl != asks_.end()) {
                if (lvl->second <= qty) {
                    asks_.erase(lvl);
                } else {
                    lvl->second -= qty;
                }
            }
        }
    }

    void updateBBO() noexcept {
        if (!bids_.empty()) {
            bbo_.bid_price = bids_.begin()->first;
            bbo_.bid_qty = bids_.begin()->second;
        } else {
            bbo_.bid_price = 0;
            bbo_.bid_qty = 0;
        }
        if (!asks_.empty()) {
            bbo_.ask_price = asks_.begin()->first;
            bbo_.ask_qty = asks_.begin()->second;
        } else {
            bbo_.ask_price = 0;
            bbo_.ask_qty = 0;
        }
    }

    common::MemPool<MarketOrder> order_pool_;
    std::unordered_map<uint64_t, MarketOrder*> order_map_;
    std::map<uint64_t, uint32_t, std::greater<uint64_t>> bids_; // Descending
    std::map<uint64_t, uint32_t, std::less<uint64_t>> asks_;    // Ascending
    BBO bbo_{};
    uint64_t last_trade_price_{0};
    uint64_t itch_packets_{0};
};

} // namespace trading
