#pragma once

#include <atomic>
#include <cstddef>
#include <vector>

namespace common {

/// Lock-Free Single-Producer Single-Consumer (SPSC) Ring Buffer.
/// Modeled after `Common::LFQueue` from "Building Low Latency Applications with C++".
template <typename T>
class LFQueue final {
public:
    explicit LFQueue(std::size_t capacity)
        : store_(capacity), capacity_(capacity) {}

    LFQueue(const LFQueue&) = delete;
    LFQueue& operator=(const LFQueue&) = delete;

    /// Returns pointer to the next writable slot, or nullptr if full.
    T* getNextToWriteTo() noexcept {
        const auto next_write = next_write_index_.load(std::memory_order_relaxed);
        const auto next_read = next_read_index_.load(std::memory_order_acquire);
        const auto after_write = (next_write + 1 == capacity_) ? 0 : next_write + 1;
        if (after_write == next_read) {
            return nullptr; // Queue full
        }
        return &store_[next_write];
    }

    /// Publishes the element written to `getNextToWriteTo()`.
    void updateWriteIndex() noexcept {
        const auto next_write = next_write_index_.load(std::memory_order_relaxed);
        const auto after_write = (next_write + 1 == capacity_) ? 0 : next_write + 1;
        next_write_index_.store(after_write, std::memory_order_release);
        num_elements_.fetch_add(1, std::memory_order_relaxed);
    }

    /// Convenience push returning false if queue is full.
    bool push(const T& item) noexcept {
        if (T* slot = getNextToWriteTo()) {
            *slot = item;
            updateWriteIndex();
            return true;
        }
        return false;
    }

    /// Returns pointer to the next readable element, or nullptr if empty.
    const T* getNextToRead() const noexcept {
        const auto next_read = next_read_index_.load(std::memory_order_relaxed);
        const auto next_write = next_write_index_.load(std::memory_order_acquire);
        if (next_read == next_write) {
            return nullptr;
        }
        return &store_[next_read];
    }

    /// Advances the read cursor after consuming the element from `getNextToRead()`.
    void updateReadIndex() noexcept {
        const auto next_read = next_read_index_.load(std::memory_order_relaxed);
        const auto after_read = (next_read + 1 == capacity_) ? 0 : next_read + 1;
        next_read_index_.store(after_read, std::memory_order_release);
        num_elements_.fetch_sub(1, std::memory_order_relaxed);
    }

    std::size_t size() const noexcept {
        return num_elements_.load(std::memory_order_relaxed);
    }

private:
    std::vector<T> store_;
    std::size_t capacity_;
    alignas(64) std::atomic<std::size_t> next_write_index_{0};
    alignas(64) std::atomic<std::size_t> next_read_index_{0};
    alignas(64) std::atomic<std::size_t> num_elements_{0};
};

} // namespace common
