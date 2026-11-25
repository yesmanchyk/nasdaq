#pragma once

#include "exchange/market_data/market_update.h"
#include "itch_ouch_wire.hpp"

#include <arpa/inet.h>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <map>
#include <netinet/in.h>
#include <poll.h>
#include <stdexcept>
#include <string>
#include <sys/socket.h>
#include <thread>
#include <unistd.h>
#include <unordered_map>

namespace adapter {

/// Bridges our Rust Exchange's PGM Reliable Multicast ITCH 5.0 stream (`239.255.0.1:30001`,
/// NAK `127.0.0.1:30002`) directly into the upstream Packt Chapter 12
/// `Exchange::MEMarketUpdateLFQueue` consumed by `Trading::TradeEngine`.
class ItchOuchMarketDataConsumer final {
public:
    ItchOuchMarketDataConsumer(
        const std::string& mcast_group,
        uint16_t mcast_port,
        const std::string& nak_host,
        uint16_t nak_port,
        Exchange::MEMarketUpdateLFQueue* market_updates)
        : mcast_group_(mcast_group),
          mcast_port_(mcast_port),
          nak_host_(nak_host),
          nak_port_(nak_port),
          market_updates_(market_updates) {}

    ~ItchOuchMarketDataConsumer() { stop(); }

    void start() {
        setupSockets();
        run_.store(true, std::memory_order_release);
        thread_ = std::thread(&ItchOuchMarketDataConsumer::runLoop, this);
    }

    void stop() {
        run_.store(false, std::memory_order_release);
        if (thread_.joinable()) thread_.join();
        if (mcast_fd_ >= 0) {
            ::close(mcast_fd_);
            mcast_fd_ = -1;
        }
        if (nak_fd_ >= 0) {
            ::close(nak_fd_);
            nak_fd_ = -1;
        }
    }

    uint64_t itchPackets() const noexcept { return itch_packets_.load(std::memory_order_relaxed); }
    uint64_t rdataRecv() const noexcept { return rdata_recv_.load(std::memory_order_relaxed); }
    uint64_t naksSent() const noexcept { return naks_sent_.load(std::memory_order_relaxed); }
    uint64_t lastMidPrice() const noexcept { return last_mid_fp_.load(std::memory_order_relaxed); }

private:
    struct TrackedOrder {
        Common::Side side;
        Common::Price price;
        Common::Qty qty;
    };

    void setupSockets() {
        mcast_fd_ = ::socket(AF_INET, SOCK_DGRAM, 0);
        if (mcast_fd_ < 0) throw std::runtime_error("Failed to create PGM multicast socket");

        int reuse = 1;
        ::setsockopt(mcast_fd_, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse));
#ifdef SO_REUSEPORT
        ::setsockopt(mcast_fd_, SOL_SOCKET, SO_REUSEPORT, &reuse, sizeof(reuse));
#endif
        int rcvbuf = 2 * 1024 * 1024;
        ::setsockopt(mcast_fd_, SOL_SOCKET, SO_RCVBUF, &rcvbuf, sizeof(rcvbuf));

        sockaddr_in bind_addr{};
        bind_addr.sin_family = AF_INET;
        bind_addr.sin_port = htons(mcast_port_);
        bind_addr.sin_addr.s_addr = htonl(INADDR_ANY);
        if (::bind(mcast_fd_, reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)) < 0) {
            ::close(mcast_fd_);
            mcast_fd_ = -1;
            throw std::runtime_error("Failed to bind PGM multicast port");
        }

        ip_mreq mreq{};
        if (::inet_pton(AF_INET, mcast_group_.c_str(), &mreq.imr_multiaddr) != 1) {
            ::close(mcast_fd_);
            mcast_fd_ = -1;
            throw std::runtime_error("Invalid multicast group IP");
        }
        mreq.imr_interface.s_addr = htonl(INADDR_LOOPBACK);
        if (::setsockopt(mcast_fd_, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mreq, sizeof(mreq)) < 0) {
            mreq.imr_interface.s_addr = htonl(INADDR_ANY);
            if (::setsockopt(mcast_fd_, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mreq, sizeof(mreq)) < 0) {
                ::close(mcast_fd_);
                mcast_fd_ = -1;
                throw std::runtime_error("IP_ADD_MEMBERSHIP failed");
            }
        }

        int flags = ::fcntl(mcast_fd_, F_GETFL, 0);
        ::fcntl(mcast_fd_, F_SETFL, flags | O_NONBLOCK);

        nak_fd_ = ::socket(AF_INET, SOCK_DGRAM, 0);
        std::memset(&nak_addr_, 0, sizeof(nak_addr_));
        nak_addr_.sin_family = AF_INET;
        nak_addr_.sin_port = htons(nak_port_);
        ::inet_pton(AF_INET, nak_host_.c_str(), &nak_addr_.sin_addr);
    }

    void pushUpdate(const Exchange::MEMarketUpdate& u) noexcept {
        auto* slot = market_updates_->getNextToWriteTo();
        *slot = u;
        market_updates_->updateWriteIndex();
    }

    void sendNak(uint32_t tsi, uint64_t start_sqn, uint64_t end_sqn) noexcept {
        if (nak_fd_ < 0 || start_sqn > end_sqn) return;
        auto bytes = wire::PgmFrame::make_nak(tsi, start_sqn, end_sqn);
        ::sendto(nak_fd_, bytes.data(), bytes.size(), 0,
                 reinterpret_cast<const sockaddr*>(&nak_addr_), sizeof(nak_addr_));
        naks_sent_.fetch_add(1, std::memory_order_relaxed);
    }

    /// Translates an ITCH 5.0 message into upstream Packt `Exchange::MEMarketUpdate` events.
    /// Ensures `order_id_ < Common::ME_MAX_ORDER_IDS` and synthesizes `MODIFY`/`CANCEL` after
    /// `TRADE` so Packt's `MarketOrderBook` stays 100% consistent with our Rust Exchange.
    void translateAndEmit(const wire::ItchMsg& m) noexcept {
        itch_packets_.fetch_add(1, std::memory_order_relaxed);
        const Common::OrderId oid = m.order_ref % Common::ME_MAX_ORDER_IDS;
        const Common::Price px = static_cast<Common::Price>(m.price);

        if (m.type == 'A') {
            if (m.shares == 0) return;
            // Remove stale entry if order_ref wrapped around
            if (auto old = active_orders_.find(oid); old != active_orders_.end()) {
                Exchange::MEMarketUpdate cxl{};
                cxl.type_ = Exchange::MarketUpdateType::CANCEL;
                cxl.ticker_id_ = 0;
                cxl.order_id_ = oid;
                cxl.side_ = old->second.side;
                cxl.price_ = old->second.price;
                cxl.qty_ = old->second.qty;
                pushUpdate(cxl);
                active_orders_.erase(old);
            }

            const Common::Side side = (m.side == 'B') ? Common::Side::BUY : Common::Side::SELL;
            active_orders_.emplace(oid, TrackedOrder{side, px, m.shares});
            if (px > 0) last_mid_fp_.store(static_cast<uint64_t>(px), std::memory_order_relaxed);

            Exchange::MEMarketUpdate add{};
            add.type_ = Exchange::MarketUpdateType::ADD;
            add.ticker_id_ = 0;
            add.order_id_ = oid;
            add.side_ = side;
            add.price_ = px;
            add.qty_ = m.shares;
            add.priority_ = ++priority_seq_;
            pushUpdate(add);
        } else if (m.type == 'E') {
            // In ITCH OrderExecuted, `m.side` is the resting order's side;
            // Packt's LiquidityTaker expects `TRADE.side_` to be the aggressive taker's side.
            const Common::Side aggressor_side =
                (m.side == 'S') ? Common::Side::BUY : Common::Side::SELL;
            if (px > 0) last_mid_fp_.store(static_cast<uint64_t>(px), std::memory_order_relaxed);

            Exchange::MEMarketUpdate trd{};
            trd.type_ = Exchange::MarketUpdateType::TRADE;
            trd.ticker_id_ = 0;
            trd.order_id_ = oid;
            trd.side_ = aggressor_side;
            trd.price_ = px;
            trd.qty_ = m.shares;
            pushUpdate(trd);

            // Reduce or remove resting order in Packt's MarketOrderBook
            if (auto it = active_orders_.find(oid); it != active_orders_.end()) {
                if (it->second.qty <= m.shares) {
                    Exchange::MEMarketUpdate cxl{};
                    cxl.type_ = Exchange::MarketUpdateType::CANCEL;
                    cxl.ticker_id_ = 0;
                    cxl.order_id_ = oid;
                    cxl.side_ = it->second.side;
                    cxl.price_ = it->second.price;
                    cxl.qty_ = it->second.qty;
                    pushUpdate(cxl);
                    active_orders_.erase(it);
                } else {
                    it->second.qty -= m.shares;
                    Exchange::MEMarketUpdate mod{};
                    mod.type_ = Exchange::MarketUpdateType::MODIFY;
                    mod.ticker_id_ = 0;
                    mod.order_id_ = oid;
                    mod.side_ = it->second.side;
                    mod.price_ = it->second.price;
                    mod.qty_ = it->second.qty;
                    pushUpdate(mod);
                }
            }
        } else if (m.type == 'D') {
            if (auto it = active_orders_.find(oid); it != active_orders_.end()) {
                Exchange::MEMarketUpdate cxl{};
                cxl.type_ = Exchange::MarketUpdateType::CANCEL;
                cxl.ticker_id_ = 0;
                cxl.order_id_ = oid;
                cxl.side_ = it->second.side;
                cxl.price_ = it->second.price;
                cxl.qty_ = it->second.qty;
                pushUpdate(cxl);
                active_orders_.erase(it);
            }
        }
    }

    void deliverInOrder(uint64_t sqn, const wire::ItchMsg& itch) noexcept {
        if (next_expected_sqn_ == 0) {
            next_expected_sqn_ = sqn + 1;
            translateAndEmit(itch);
            return;
        }
        if (sqn < next_expected_sqn_) return;
        if (sqn == next_expected_sqn_) {
            translateAndEmit(itch);
            ++next_expected_sqn_;
            while (true) {
                auto it = reorder_buf_.find(next_expected_sqn_);
                if (it == reorder_buf_.end()) break;
                translateAndEmit(it->second);
                reorder_buf_.erase(it);
                ++next_expected_sqn_;
            }
        } else {
            reorder_buf_.emplace(sqn, itch);
        }
    }

    void runLoop() {
        uint8_t buf[wire::PgmFrame::WIRE_SIZE];
        while (run_.load(std::memory_order_acquire)) {
            int drained = 0;
            while (drained < 64) {
                const ssize_t n = ::recv(mcast_fd_, buf, sizeof(buf), 0);
                if (n < static_cast<ssize_t>(wire::PgmFrame::WIRE_SIZE)) break;
                ++drained;
                auto fopt = wire::PgmFrame::from_bytes(buf);
                if (!fopt) continue;
                const auto& f = *fopt;
                if (f.type == 0x04) { // ODATA
                    if (next_expected_sqn_ > 0 && f.field_a > next_expected_sqn_) {
                        const uint64_t gap_start = std::max(next_expected_sqn_, f.field_b);
                        if (gap_start < f.field_a) sendNak(f.tsi, gap_start, f.field_a - 1);
                        if (next_expected_sqn_ < f.field_b) next_expected_sqn_ = f.field_b;
                    }
                    deliverInOrder(f.field_a, f.itch);
                } else if (f.type == 0x05) { // RDATA
                    rdata_recv_.fetch_add(1, std::memory_order_relaxed);
                    deliverInOrder(f.field_a, f.itch);
                } else if (f.type == 0x00) { // SPM
                    if (next_expected_sqn_ > 0 && f.field_a >= next_expected_sqn_) {
                        const uint64_t gap_start = std::max(next_expected_sqn_, f.field_b);
                        if (gap_start <= f.field_a) sendNak(f.tsi, gap_start, f.field_a);
                    }
                }
            }
            if (drained == 0) {
                pollfd pfd{mcast_fd_, POLLIN, 0};
                ::poll(&pfd, 1, 1);
            }
        }
    }

    std::string mcast_group_;
    uint16_t mcast_port_;
    std::string nak_host_;
    uint16_t nak_port_;
    Exchange::MEMarketUpdateLFQueue* market_updates_;

    int mcast_fd_{-1};
    int nak_fd_{-1};
    sockaddr_in nak_addr_{};
    uint64_t next_expected_sqn_{0};
    uint64_t priority_seq_{0};
    std::map<uint64_t, wire::ItchMsg> reorder_buf_;
    std::unordered_map<Common::OrderId, TrackedOrder> active_orders_;

    std::atomic<bool> run_{false};
    std::thread thread_;
    std::atomic<uint64_t> itch_packets_{0};
    std::atomic<uint64_t> rdata_recv_{0};
    std::atomic<uint64_t> naks_sent_{0};
    std::atomic<uint64_t> last_mid_fp_{100 * wire::PRICE_SCALE};
};

} // namespace adapter
