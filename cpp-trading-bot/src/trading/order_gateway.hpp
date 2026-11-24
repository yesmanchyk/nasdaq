#pragma once

#include "common/lf_queue.hpp"
#include "common/protocol.hpp"

#include <arpa/inet.h>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdexcept>
#include <string>
#include <sys/socket.h>
#include <thread>
#include <unistd.h>

namespace trading {

/// Dedicated Order Gateway Thread over non-blocking TCP (`TCP_NODELAY`):
/// - Drains `LFQueue<OuchInbound>` and writes 22-byte OUCH frames to the Exchange.
/// - Reads 30-byte `OuchOutbound` frames from the Exchange and pushes them into
///   `LFQueue<OuchOutbound>` for the `TradeEngine`.
///
/// Modeled after `Trading::OrderGateway` from "Building Low Latency Applications with C++".
class OrderGateway final {
public:
    OrderGateway(
        const std::string& host,
        uint16_t port,
        common::LFQueue<common::OuchInbound>* outgoing_requests,
        common::LFQueue<common::OuchOutbound>* incoming_responses)
        : host_(host),
          port_(port),
          outgoing_requests_(outgoing_requests),
          incoming_responses_(incoming_responses) {}

    ~OrderGateway() { stop(); }

    void start() {
        connectSocket();
        run_.store(true, std::memory_order_release);
        thread_ = std::thread(&OrderGateway::runLoop, this);
    }

    void stop() {
        run_.store(false, std::memory_order_release);
        if (thread_.joinable()) {
            thread_.join();
        }
        if (sock_fd_ >= 0) {
            ::close(sock_fd_);
            sock_fd_ = -1;
        }
    }

private:
    void connectSocket() {
        sock_fd_ = ::socket(AF_INET, SOCK_STREAM, 0);
        if (sock_fd_ < 0) {
            throw std::runtime_error("OrderGateway: failed to create TCP socket");
        }

        int nodelay = 1;
        ::setsockopt(sock_fd_, IPPROTO_TCP, TCP_NODELAY, &nodelay, sizeof(nodelay));

        sockaddr_in addr{};
        addr.sin_family = AF_INET;
        addr.sin_port = htons(port_);
        if (::inet_pton(AF_INET, host_.c_str(), &addr.sin_addr) != 1) {
            ::close(sock_fd_);
            sock_fd_ = -1;
            throw std::runtime_error("OrderGateway: invalid OUCH host IP");
        }

        if (::connect(sock_fd_, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
            ::close(sock_fd_);
            sock_fd_ = -1;
            throw std::runtime_error("OrderGateway: TCP connect to Exchange OUCH failed");
        }

        int flags = ::fcntl(sock_fd_, F_GETFL, 0);
        ::fcntl(sock_fd_, F_SETFL, flags | O_NONBLOCK);
    }

    bool writeAll(const uint8_t* data, std::size_t len) noexcept {
        std::size_t sent = 0;
        while (sent < len && run_.load(std::memory_order_relaxed)) {
            const ssize_t n = ::send(sock_fd_, data + sent, len - sent, MSG_NOSIGNAL);
            if (n > 0) {
                sent += static_cast<std::size_t>(n);
            } else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) {
                std::this_thread::sleep_for(std::chrono::microseconds(10));
            } else {
                return false;
            }
        }
        return sent == len;
    }

    void runLoop() {
        uint8_t rx_buf[common::OuchOutbound::WIRE_SIZE];
        std::size_t rx_offset = 0;

        while (run_.load(std::memory_order_acquire)) {
            bool did_work = false;

            // 1. Drain outgoing OUCH requests from TradeEngine
            while (const auto* req = outgoing_requests_->getNextToRead()) {
                auto bytes = req->to_bytes();
                outgoing_requests_->updateReadIndex();
                if (!writeAll(bytes.data(), bytes.size())) {
                    run_.store(false, std::memory_order_release);
                    return;
                }
                did_work = true;
            }

            // 2. Non-blocking read of 30-byte OUCH outbound execution reports
            for (int iter = 0; iter < 32; ++iter) {
                const ssize_t n = ::recv(
                    sock_fd_,
                    rx_buf + rx_offset,
                    common::OuchOutbound::WIRE_SIZE - rx_offset,
                    0);
                if (n > 0) {
                    rx_offset += static_cast<std::size_t>(n);
                    did_work = true;
                    if (rx_offset == common::OuchOutbound::WIRE_SIZE) {
                        if (auto msg = common::OuchOutbound::from_bytes(rx_buf)) {
                            incoming_responses_->push(*msg);
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

            if (!did_work) {
                std::this_thread::sleep_for(std::chrono::microseconds(50));
            }
        }
    }

    std::string host_;
    uint16_t port_;
    common::LFQueue<common::OuchInbound>* outgoing_requests_;
    common::LFQueue<common::OuchOutbound>* incoming_responses_;

    int sock_fd_{-1};
    std::atomic<bool> run_{false};
    std::thread thread_;
};

} // namespace trading
