#pragma once

#include <cassert>
#include <cstddef>
#include <utility>
#include <vector>

namespace common {

/// Pre-allocated contiguous Memory Pool for zero-allocation hot-path objects.
/// Modeled after `Common::MemPool` from "Building Low Latency Applications with C++".
template <typename T>
class MemPool final {
public:
    explicit MemPool(std::size_t num_elems) : store_(num_elems) {}

    MemPool(const MemPool&) = delete;
    MemPool& operator=(const MemPool&) = delete;

    template <typename... Args>
    T* allocate(Args&&... args) noexcept {
        auto* obj_block = &store_[next_free_index_];
        if (!obj_block->is_free) {
            updateNextFreeIndex();
            obj_block = &store_[next_free_index_];
            if (!obj_block->is_free) {
                return nullptr; // Pool exhausted
            }
        }
        T* ret = &(obj_block->object);
        *ret = T(std::forward<Args>(args)...);
        obj_block->is_free = false;
        ++allocated_count_;
        updateNextFreeIndex();
        return ret;
    }

    void deallocate(const T* elem) noexcept {
        if (!elem) return;
        const auto elem_index =
            static_cast<std::size_t>(reinterpret_cast<const ObjectBlock*>(elem) - &store_[0]);
        assert(elem_index < store_.size());
        if (!store_[elem_index].is_free) {
            store_[elem_index].is_free = true;
            --allocated_count_;
            next_free_index_ = elem_index;
        }
    }

    std::size_t allocatedCount() const noexcept { return allocated_count_; }
    std::size_t capacity() const noexcept { return store_.size(); }

private:
    struct ObjectBlock {
        T object{};
        bool is_free{true};
    };

    void updateNextFreeIndex() noexcept {
        const auto initial = next_free_index_;
        const auto n = store_.size();
        for (std::size_t i = 0; i < n; ++i) {
            const auto idx = (initial + i) % n;
            if (store_[idx].is_free) {
                next_free_index_ = idx;
                return;
            }
        }
    }

    std::vector<ObjectBlock> store_;
    std::size_t next_free_index_{0};
    std::size_t allocated_count_{0};
};

} // namespace common
