#pragma once

#include "exchange/order_server/client_request.h"
#include "exchange/order_server/client_response.h"
#include "itch_ouch_wire.hpp"

#include <algorithm>
#include <arpa/inet.h>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <stdexcept>
#include <string>
#include <sys/socket.h>
#include <thread>
#include <unistd.h>
#include <unordered_map>

namespace adapter {

struct GatewayTelemetry {
    int64_t position{0};
    int64_t cash_fp{wire::INITIAL_CAPITAL_FP};
    uint64_t fills{0};
    uint64_t acks{0};
    uint64_t cancels{0};
};

/// Bridges upstream Packt Chapter 12 `Exchange::ClientRequestLFQueue` and
/// `Exchange::ClientResponseLFQueue` directly to our Rust Exchange's TCP OUCH 5.0
/// binary protocol (`127.0.0.1:40000`), enforcing the equal `$100,000.00` capital limit.
class ItchOuchOrderGateway final {
public:
    ItchOuchOrderGateway(
        Common::ClientId client_id,
        const std::string& ouch_host,
        uint16_t ouch_port,
        Exchange::ClientRequestLFQueue* outgoing_requests,
        Exchange::ClientResponseLFQueue* incoming_responses)
        : client_id_(client_id),
          ouch_host_(ouch_host),
          ouch_port_(ouch_port),
          outgoing_requests_(outgoing_requests),
          incoming_responses_(incoming_responses),
          cl_ord_prefix_(static_cast<uint64_t>(client_id) * 1'000'000ULL) {}

    ~ItchOuchOrderGateway() { stop(); }

    void start() {
        connectSocket();
        run_.store(true, std::memory_order_release);
        thread_ = std::thread(&ItchOuchOrderGateway::runLoop, this);
    }

    void stop() {
        run_.store(false, std::memory_order_release);
        if (thread_.joinable()) thread_.join();
        if (sock_fd_ >= 0) {
            ::close(sock_fd_);
            sock_fd_ = -1;
        }
    }

    GatewayTelemetry telemetry() const noexcept {
        return GatewayTelemetry{
            position_.load(std::memory_order_relaxed),
            cash_fp_.load(std::memory_order_relaxed),
            fills_.load(std::memory_order_relaxed),
            acks_.load(std::memory_order_relaxed),
            cancels_.load(std::memory_order_relaxed),
        };
    }

private:
    struct PendingOrder {
        Common::OrderId packt_oid;
        Common::Side side;
        Common::Qty leaves_qty;
    };

    void connectSocket() {
        sock_fd_ = ::socket(AF_INET, SOCK_STREAM, 0);
        if (sock_fd_ < 0) throw std::runtime_error("Failed to create OUCH TCP socket");

        int nodelay = 1;
        ::setsockopt(sock_fd_, IPPROTO_TCP, TCP_NODELAY, &nodelay, sizeof(nodelay));

        sockaddr_in addr{};
        addr.sin_family = AF_INET;
        addr.sin_port = htons(ouch_port_);
        if (::inet_pton(AF_INET, ouch_host_.c_str(), &addr.sin_addr) != 1) {
            ::close(sock_fd_);
            sock_fd_ = -1;
            throw std::runtime_error("Invalid OUCH host IP");
        }

        if (::connect(sock_fd_, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
            ::close(sock_fd_);
            sock_fd_ = -1;
            throw std::runtime_error("TCP connect to Exchange OUCH failed");
        }

        int flags = ::fcntl(sock_fd_, F_GETFL, 0);
        ::fcntl(sock_fd_, F_SETFL, flags | O_NONBLOCK);
    }

    uint32_t affordableShares(Common::Side side, uint32_t requested, uint64_t price) const noexcept {
        if (price == 0 || requested == 0) return 0;
        const int64_t px = static_cast<int64_t>(price);
        const int64_t cash = cash_fp_.load(std::memory_order_relaxed);
        const int64_t pos = position_.load(std::memory_order_relaxed);
        const int64_t equity_fp = std::max<int64_t>(0, cash + pos * px);
        const int64_t max_exposure = equity_fp / px;
        if (side == Common::Side::BUY) {
            const int64_t max_cash = std::max<int64_t>(0, cash) / px;
            const int64_t max_eq = std::max<int64_t>(0, max_exposure - pos);
            return std::min(requested, static_cast<uint32_t>(std::min(max_cash, max_eq)));
        } else {
            const int64_t max_eq = std::max<int64_t>(0, max_exposure + pos);
            return std::min(requested, static_cast<uint32_t>(max_eq));
        }
    }

    bool writeAll(const uint8_t* data, std::size_t len) noexcept {
        std::size_t sent = 0;
        while (sent < len && run_.load(std::memory_order_relaxed)) {
            const ssize_t n = ::send(sock_fd_, data + sent, len - sent, MSG_NOSIGNAL);
            if (n > 0) {
                sent += static_cast<std::size_t>(n);
            } else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) {
                pollfd pfd{sock_fd_, POLLOUT, 0};
                ::poll(&pfd, 1, 1);
            } else {
                return false;
            }
        }
        return sent == len;
    }

    void pushResponse(const Exchange::MEClientResponse& resp) noexcept {
        auto* slot = incoming_responses_->getNextToWriteTo();
        *slot = resp;
        incoming_responses_->updateWriteIndex();
    }

    void handleOuchOutbound(const wire::OuchOutboundMsg& m) noexcept {
        auto it = orders_.find(m.cl_ord_id);
        Exchange::MEClientResponse resp{};
        resp.client_id_ = client_id_;
        resp.ticker_id_ = 0;
        resp.client_order_id_ = (it != orders_.end()) ? it->second.packt_oid : m.cl_ord_id;
        resp.market_order_id_ = m.order_ref;

        if (m.type == 'A') {
            acks_.fetch_add(1, std::memory_order_relaxed);
            resp.type_ = Exchange::ClientResponseType::ACCEPTED;
            resp.side_ = (m.side == 'B') ? Common::Side::BUY : Common::Side::SELL;
            resp.price_ = static_cast<Common::Price>(m.price);
            resp.exec_qty_ = 0;
            resp.leaves_qty_ = m.shares;
            if (it != orders_.end()) it->second.leaves_qty = m.shares;
            pushResponse(resp);
        } else if (m.type == 'E') {
            fills_.fetch_add(1, std::memory_order_relaxed);
            const Common::Side side = (m.side == 'B') ? Common::Side::BUY : Common::Side::SELL;
            const int64_t qty = static_cast<int64_t>(m.shares);
            const int64_t notional = qty * static_cast<int64_t>(m.price);
            if (side == Common::Side::BUY) {
                position_.fetch_add(qty, std::memory_order_relaxed);
                cash_fp_.fetch_sub(notional, std::memory_order_relaxed);
            } else {
                position_.fetch_sub(qty, std::memory_order_relaxed);
                cash_fp_.fetch_add(notional, std::memory_order_relaxed);
            }

            Common::Qty leaves = 0;
            if (it != orders_.end()) {
                it->second.leaves_qty =
                    (it->second.leaves_qty > m.shares) ? (it->second.leaves_qty - m.shares) : 0;
                leaves = it->second.leaves_qty;
                if (leaves == 0) orders_.erase(it);
            }

            resp.type_ = Exchange::ClientResponseType::FILLED;
            resp.side_ = side;
            resp.price_ = static_cast<Common::Price>(m.price);
            resp.exec_qty_ = m.shares;
            resp.leaves_qty_ = leaves;
            pushResponse(resp);
        } else if (m.type == 'C') {
            cancels_.fetch_add(1, std::memory_order_relaxed);
            if (it == orders_.end()) return; // Unknown / already completed
            resp.type_ = Exchange::ClientResponseType::CANCELED;
            resp.side_ = it->second.side;
            resp.exec_qty_ = 0;
            resp.leaves_qty_ = 0;
            orders_.erase(it);
            pushResponse(resp);
        }
    }

    void runLoop() {
        uint8_t rx_buf[wire::OuchOutboundMsg::WIRE_SIZE];
        std::size_t rx_offset = 0;

        while (run_.load(std::memory_order_acquire)) {
            bool did_work = false;

            // 1. Drain Packt `MEClientRequest`s and send 22-byte OUCH frames
            while (const auto* req = outgoing_requests_->getNextToRead()) {
                const auto r = *req;
                outgoing_requests_->updateReadIndex();
                did_work = true;

                const uint64_t wire_cl_ord_id = cl_ord_prefix_ + r.order_id_;
                if (r.type_ == Exchange::ClientRequestType::NEW) {
                    const uint64_t px = static_cast<uint64_t>(std::max<Common::Price>(10'000, r.price_));
                    const uint32_t allowed = affordableShares(r.side_, r.qty_, px);
                    if (allowed == 0) {
                        // Immediately return CANCELED so Packt's OrderManager exits PENDING_NEW cleanly
                        Exchange::MEClientResponse rej{};
                        rej.type_ = Exchange::ClientResponseType::CANCELED;
                        rej.client_id_ = client_id_;
                        rej.ticker_id_ = r.ticker_id_;
                        rej.client_order_id_ = r.order_id_;
                        rej.side_ = r.side_;
                        pushResponse(rej);
                        continue;
                    }
                    orders_[wire_cl_ord_id] = PendingOrder{r.order_id_, r.side_, allowed};
                    const uint8_t side_byte = (r.side_ == Common::Side::BUY) ? 'B' : 'S';
                    auto frame = wire::encode_ouch_enter(wire_cl_ord_id, side_byte, allowed, px);
                    if (!writeAll(frame.data(), frame.size())) {
                        run_.store(false, std::memory_order_release);
                        return;
                    }
                } else if (r.type_ == Exchange::ClientRequestType::CANCEL) {
                    if (orders_.find(wire_cl_ord_id) == orders_.end()) {
                        // Order already fully filled or canceled on exchange; acknowledge so
                        // upstream Packt OrderManager transitions out of PENDING_CANCEL.
                        Exchange::MEClientResponse cxl{};
                        cxl.type_ = Exchange::ClientResponseType::CANCELED;
                        cxl.client_id_ = client_id_;
                        cxl.ticker_id_ = r.ticker_id_;
                        cxl.client_order_id_ = r.order_id_;
                        cxl.side_ = r.side_;
                        pushResponse(cxl);
                        continue;
                    }
                    auto frame = wire::encode_ouch_cancel(wire_cl_ord_id);
                    if (!writeAll(frame.data(), frame.size())) {
                        run_.store(false, std::memory_order_release);
                        return;
                    }
                }
            }

            // 2. Read 30-byte OUCH Outbound frames and push Packt `MEClientResponse`s
            for (int iter = 0; iter < 32; ++iter) {
                const ssize_t n = ::recv(
                    sock_fd_, rx_buf + rx_offset, wire::OuchOutboundMsg::WIRE_SIZE - rx_offset, 0);
                if (n > 0) {
                    rx_offset += static_cast<std::size_t>(n);
                    did_work = true;
                    if (rx_offset == wire::OuchOutboundMsg::WIRE_SIZE) {
                        if (auto msg = wire::OuchOutboundMsg::from_bytes(rx_buf)) {
                            handleOuchOutbound(*msg);
                        }
                        rx_offset = 0;
                    }
                } else if (n == 0) {
                    run_.store(false, std::memory_order_release);
                    return;
                } else if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) {
                    break;
                } else {
                    run_.store(false, std::memory_order_release);
                    return;
                }
            }

            if (!did_work) std::this_thread::yield();
        }
    }

    Common::ClientId client_id_;
    std::string ouch_host_;
    uint16_t ouch_port_;
    Exchange::ClientRequestLFQueue* outgoing_requests_;
    Exchange::ClientResponseLFQueue* incoming_responses_;
    uint64_t cl_ord_prefix_;

    int sock_fd_{-1};
    std::unordered_map<uint64_t, PendingOrder> orders_;

    std::atomic<bool> run_{false};
    std::thread thread_;
    std::atomic<int64_t> position_{0};
    std::atomic<int64_t> cash_fp_{wire::INITIAL_CAPITAL_FP};
    std::atomic<uint64_t> fills_{0};
    std::atomic<uint64_t> acks_{0};
    std::atomic<uint64_t> cancels_{0};
};

} // namespace adapter
