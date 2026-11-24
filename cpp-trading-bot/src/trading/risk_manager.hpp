#pragma once

#include "common/protocol.hpp"

#include <algorithm>
#include <cstdint>

namespace trading {

struct PositionSnapshot {
    int64_t initial_capital_fp{common::INITIAL_BOT_CAPITAL_FP};
    int64_t position{0};
    int64_t cash_fp{common::INITIAL_BOT_CAPITAL_FP};
    uint64_t mark_fp{100 * common::PRICE_SCALE};
    int64_t equity_fp{common::INITIAL_BOT_CAPITAL_FP};
    int64_t mtm_pnl_fp{0};
    uint64_t fills{0};
    uint64_t acks{0};
    uint64_t cancels{0};
};

struct RiskConfig {
    uint32_t max_order_size{200};
    int64_t max_position{600};
    int64_t max_loss_fp{5'000 * static_cast<int64_t>(common::PRICE_SCALE)}; // -$5,000 kill switch
};

/// Tracks cash, inventory, fills, and continuous mark-to-market PnL
/// starting from an equal fixed `$100,000.00` capital budget.
/// Modeled after `Trading::PositionKeeper` + `Trading::RiskManager`.
class RiskManager final {
public:
    explicit RiskManager(
        int64_t initial_capital_fp = common::INITIAL_BOT_CAPITAL_FP,
        RiskConfig cfg = RiskConfig{}) noexcept
        : initial_capital_fp_(initial_capital_fp),
          cash_fp_(initial_capital_fp),
          cfg_(cfg) {}

    void onOrderAccepted() noexcept { ++acks_; }
    void onOrderCanceled() noexcept { ++cancels_; }

    void onFill(common::Side side, uint32_t executed_shares, uint64_t fill_price) noexcept {
        ++fills_;
        const int64_t qty = static_cast<int64_t>(executed_shares);
        const int64_t notional = qty * static_cast<int64_t>(fill_price);
        if (side == common::Side::Buy) {
            position_ += qty;
            cash_fp_ -= notional;
        } else {
            position_ -= qty;
            cash_fp_ += notional;
        }
    }

    /// Pre-trade risk check + buying-power clamp (matches Rust `BotState::affordable_shares`).
    uint32_t checkPreTradeRisk(
        common::Side side,
        uint32_t requested_shares,
        uint64_t price,
        uint64_t mark_price) const noexcept {
        if (price == 0 || requested_shares == 0) return 0;

        // Kill-switch check on max drawdown
        const auto snap = snapshot(mark_price);
        if (snap.mtm_pnl_fp < -cfg_.max_loss_fp) {
            // Only allow risk-reducing trades if kill switch tripped
            if (side == common::Side::Buy && position_ >= 0) return 0;
            if (side == common::Side::Sell && position_ <= 0) return 0;
        }

        // Max single-order size & max inventory cap
        uint32_t allowed = std::min(requested_shares, cfg_.max_order_size);
        if (side == common::Side::Buy) {
            const int64_t room = std::max<int64_t>(0, cfg_.max_position - position_);
            allowed = std::min<uint32_t>(allowed, static_cast<uint32_t>(room));
        } else {
            const int64_t room = std::max<int64_t>(0, cfg_.max_position + position_);
            allowed = std::min<uint32_t>(allowed, static_cast<uint32_t>(room));
        }
        if (allowed == 0) return 0;

        // Symmetric 1x equity buying-power enforcement
        const int64_t px = static_cast<int64_t>(price);
        const int64_t equity_fp = std::max<int64_t>(0, cash_fp_ + position_ * px);
        const int64_t max_exposure_shares = equity_fp / px;

        if (side == common::Side::Buy) {
            const int64_t max_by_cash = std::max<int64_t>(0, cash_fp_) / px;
            const int64_t max_by_equity = std::max<int64_t>(0, max_exposure_shares - position_);
            const uint32_t max_sh = static_cast<uint32_t>(std::min(max_by_cash, max_by_equity));
            return std::min(allowed, max_sh);
        } else {
            const int64_t max_by_equity = std::max<int64_t>(0, max_exposure_shares + position_);
            return std::min(allowed, static_cast<uint32_t>(max_by_equity));
        }
    }

    PositionSnapshot snapshot(uint64_t mark_fp) const noexcept {
        PositionSnapshot s{};
        s.initial_capital_fp = initial_capital_fp_;
        s.position = position_;
        s.cash_fp = cash_fp_;
        s.mark_fp = mark_fp;
        s.equity_fp = cash_fp_ + position_ * static_cast<int64_t>(mark_fp);
        s.mtm_pnl_fp = s.equity_fp - initial_capital_fp_;
        s.fills = fills_;
        s.acks = acks_;
        s.cancels = cancels_;
        return s;
    }

    int64_t position() const noexcept { return position_; }
    int64_t cashFp() const noexcept { return cash_fp_; }

private:
    int64_t initial_capital_fp_;
    int64_t position_{0};
    int64_t cash_fp_;
    uint64_t fills_{0};
    uint64_t acks_{0};
    uint64_t cancels_{0};
    RiskConfig cfg_;
};

} // namespace trading
