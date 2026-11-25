#pragma once

#include "common/lf_queue.hpp"
#include "common/protocol.hpp"

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

namespace trading {

/// Dedicated Market Data Consumer Thread implementing RFC 3208 PGM Reliable Multicast
/// over UDP (`239.255.0.1:30001`) + unicast NAK repair (`127.0.0.1:30002`),
/// pushing decoded 30-byte ITCH 5.0 frames into a lock-free SPSC queue.
/// Modeled after `Trading::MarketDataConsumer` from "Building Low Latency Applications with C++".
class MarketDataConsumer final {
public:
    MarketDataConsumer(
        const std::string& mcast_group,
        uint16_t mcast_port,
        const std::string& nak_host,
        uint16_t nak_port,
        common::LFQueue<common::ItchMessage>* md_queue)
        : mcast_group_(mcast_group),
          mcast_port_(mcast_port),
          nak_host_(nak_host),
          nak_port_(nak_port),
          md_queue_(md_queue) {}

    ~MarketDataConsumer() { stop(); }

    void start() {
        setupSockets();
        run_.store(true, std::memory_order_release);
        thread_ = std::thread(&MarketDataConsumer::runLoop, this);
    }

    void stop() {
        run_.store(false, std::memory_order_release);
        if (thread_.joinable()) {
            thread_.join();
        }
        if (mcast_fd_ >= 0) {
            ::close(mcast_fd_);
            mcast_fd_ = -1;
        }
        if (nak_fd_ >= 0) {
            ::close(nak_fd_);
            nak_fd_ = -1;
        }
    }

    uint64_t odataRecv() const noexcept { return odata_recv_.load(std::memory_order_relaxed); }
    uint64_t rdataRecv() const noexcept { return rdata_recv_.load(std::memory_order_relaxed); }
    uint64_t naksSent() const noexcept { return naks_sent_.load(std::memory_order_relaxed); }

private:
    void setupSockets() {
        mcast_fd_ = ::socket(AF_INET, SOCK_DGRAM, 0);
        if (mcast_fd_ < 0) {
            throw std::runtime_error("MarketDataConsumer: failed to create multicast UDP socket");
        }

        int reuse = 1;
        ::setsockopt(mcast_fd_, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse));
#ifdef SO_REUSEPORT
        ::setsockopt(mcast_fd_, SOL_SOCKET, SO_REUSEPORT, &reuse, sizeof(reuse));
#endif
        // Enlarge kernel receive buffer to minimize multicast drops
        int rcvbuf = 2 * 1024 * 1024;
        ::setsockopt(mcast_fd_, SOL_SOCKET, SO_RCVBUF, &rcvbuf, sizeof(rcvbuf));

        sockaddr_in bind_addr{};
        bind_addr.sin_family = AF_INET;
        bind_addr.sin_port = htons(mcast_port_);
        bind_addr.sin_addr.s_addr = htonl(INADDR_ANY);
        if (::bind(mcast_fd_, reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)) < 0) {
            ::close(mcast_fd_);
            mcast_fd_ = -1;
            throw std::runtime_error("MarketDataConsumer: failed to bind multicast port");
        }

        ip_mreq mreq{};
        if (::inet_pton(AF_INET, mcast_group_.c_str(), &mreq.imr_multiaddr) != 1) {
            ::close(mcast_fd_);
            mcast_fd_ = -1;
            throw std::runtime_error("MarketDataConsumer: invalid multicast group IP");
        }
        // Match Rust exchange's loopback multicast interface (127.0.0.1)
        mreq.imr_interface.s_addr = htonl(INADDR_LOOPBACK);
        if (::setsockopt(mcast_fd_, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mreq, sizeof(mreq)) < 0) {
            mreq.imr_interface.s_addr = htonl(INADDR_ANY);
            if (::setsockopt(mcast_fd_, IPPROTO_IP, IP_ADD_MEMBERSHIP, &mreq, sizeof(mreq)) < 0) {
                ::close(mcast_fd_);
                mcast_fd_ = -1;
                throw std::runtime_error("MarketDataConsumer: IP_ADD_MEMBERSHIP failed");
            }
        }

        int flags = ::fcntl(mcast_fd_, F_GETFL, 0);
        ::fcntl(mcast_fd_, F_SETFL, flags | O_NONBLOCK);

        // Unicast UDP socket for sending RFC 3208 PGM_NAK packets
        nak_fd_ = ::socket(AF_INET, SOCK_DGRAM, 0);
        if (nak_fd_ < 0) {
            ::close(mcast_fd_);
            mcast_fd_ = -1;
            throw std::runtime_error("MarketDataConsumer: failed to create NAK UDP socket");
        }
        std::memset(&nak_addr_, 0, sizeof(nak_addr_));
        nak_addr_.sin_family = AF_INET;
        nak_addr_.sin_port = htons(nak_port_);
        if (::inet_pton(AF_INET, nak_host_.c_str(), &nak_addr_.sin_addr) != 1) {
            ::close(mcast_fd_);
            ::close(nak_fd_);
            mcast_fd_ = -1;
            nak_fd_ = -1;
            throw std::runtime_error("MarketDataConsumer: invalid NAK host IP");
        }
    }

    void sendNak(uint32_t tsi, uint64_t start_sqn, uint64_t end_sqn) noexcept {
        if (nak_fd_ < 0 || start_sqn > end_sqn) return;
        common::PgmFrame nak{};
        nak.type = common::PgmPacketType::Nak;
        nak.tsi = tsi;
        nak.field_a = start_sqn;
        nak.field_b = end_sqn;
        auto bytes = nak.to_bytes();
        ::sendto(
            nak_fd_,
            bytes.data(),
            bytes.size(),
            0,
            reinterpret_cast<const sockaddr*>(&nak_addr_),
            sizeof(nak_addr_));
        naks_sent_.fetch_add(1, std::memory_order_relaxed);
    }

    void deliverInOrder(uint64_t sqn, const common::ItchMessage& itch) noexcept {
        if (next_expected_sqn_ == 0) {
            next_expected_sqn_ = sqn + 1;
            md_queue_->push(itch);
            return;
        }
        if (sqn < next_expected_sqn_) {
            return; // Duplicate
        }
        if (sqn == next_expected_sqn_) {
            md_queue_->push(itch);
            ++next_expected_sqn_;
            // Flush any buffered subsequent packets
            while (true) {
                auto it = reorder_buf_.find(next_expected_sqn_);
                if (it == reorder_buf_.end()) break;
                md_queue_->push(it->second);
                reorder_buf_.erase(it);
                ++next_expected_sqn_;
            }
        } else {
            reorder_buf_.emplace(sqn, itch);
        }
    }

    void runLoop() {
        uint8_t buf[common::PgmFrame::WIRE_SIZE];
        while (run_.load(std::memory_order_acquire)) {
            int drained = 0;
            while (drained < 64) {
                const ssize_t n = ::recv(mcast_fd_, buf, sizeof(buf), 0);
                if (n < static_cast<ssize_t>(common::PgmFrame::WIRE_SIZE)) {
                    break;
                }
                ++drained;
                auto frame_opt = common::PgmFrame::from_bytes(buf);
                if (!frame_opt) continue;
                const auto& f = *frame_opt;

                if (f.type == common::PgmPacketType::Odata) {
                    odata_recv_.fetch_add(1, std::memory_order_relaxed);
                    if (next_expected_sqn_ > 0 && f.field_a > next_expected_sqn_) {
                        const uint64_t gap_start = std::max(next_expected_sqn_, f.field_b);
                        if (gap_start < f.field_a) {
                            sendNak(f.tsi, gap_start, f.field_a - 1);
                        }
                        if (next_expected_sqn_ < f.field_b) {
                            next_expected_sqn_ = f.field_b;
                        }
                    }
                    deliverInOrder(f.field_a, f.itch);
                } else if (f.type == common::PgmPacketType::Rdata) {
                    rdata_recv_.fetch_add(1, std::memory_order_relaxed);
                    deliverInOrder(f.field_a, f.itch);
                } else if (f.type == common::PgmPacketType::Spm) {
                    if (next_expected_sqn_ > 0 && f.field_a >= next_expected_sqn_) {
                        const uint64_t gap_start = std::max(next_expected_sqn_, f.field_b);
                        if (gap_start <= f.field_a) {
                            sendNak(f.tsi, gap_start, f.field_a);
                        }
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
    common::LFQueue<common::ItchMessage>* md_queue_;

    int mcast_fd_{-1};
    int nak_fd_{-1};
    sockaddr_in nak_addr_{};

    uint64_t next_expected_sqn_{0};
    std::map<uint64_t, common::ItchMessage> reorder_buf_;

    std::atomic<bool> run_{false};
    std::thread thread_;

    std::atomic<uint64_t> odata_recv_{0};
    std::atomic<uint64_t> rdata_recv_{0};
    std::atomic<uint64_t> naks_sent_{0};
};

} // namespace trading
