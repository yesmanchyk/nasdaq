#pragma once

#include <array>
#include <cstdint>
#include <cstring>
#include <optional>

namespace wire {

inline constexpr uint64_t PRICE_SCALE = 10'000;
inline constexpr uint64_t INITIAL_CAPITAL_DOLLARS = 100'000;
inline constexpr int64_t INITIAL_CAPITAL_FP =
    static_cast<int64_t>(INITIAL_CAPITAL_DOLLARS * PRICE_SCALE);

inline uint32_t read_be32(const uint8_t* p) noexcept {
    return (static_cast<uint32_t>(p[0]) << 24) |
           (static_cast<uint32_t>(p[1]) << 16) |
           (static_cast<uint32_t>(p[2]) << 8) |
           static_cast<uint32_t>(p[3]);
}

inline uint64_t read_be64(const uint8_t* p) noexcept {
    return (static_cast<uint64_t>(p[0]) << 56) |
           (static_cast<uint64_t>(p[1]) << 48) |
           (static_cast<uint64_t>(p[2]) << 40) |
           (static_cast<uint64_t>(p[3]) << 32) |
           (static_cast<uint64_t>(p[4]) << 24) |
           (static_cast<uint64_t>(p[5]) << 16) |
           (static_cast<uint64_t>(p[6]) << 8) |
           static_cast<uint64_t>(p[7]);
}

inline void write_be32(uint8_t* p, uint32_t v) noexcept {
    p[0] = static_cast<uint8_t>((v >> 24) & 0xFF);
    p[1] = static_cast<uint8_t>((v >> 16) & 0xFF);
    p[2] = static_cast<uint8_t>((v >> 8) & 0xFF);
    p[3] = static_cast<uint8_t>(v & 0xFF);
}

inline void write_be64(uint8_t* p, uint64_t v) noexcept {
    p[0] = static_cast<uint8_t>((v >> 56) & 0xFF);
    p[1] = static_cast<uint8_t>((v >> 48) & 0xFF);
    p[2] = static_cast<uint8_t>((v >> 40) & 0xFF);
    p[3] = static_cast<uint8_t>((v >> 32) & 0xFF);
    p[4] = static_cast<uint8_t>((v >> 24) & 0xFF);
    p[5] = static_cast<uint8_t>((v >> 16) & 0xFF);
    p[6] = static_cast<uint8_t>((v >> 8) & 0xFF);
    p[7] = static_cast<uint8_t>(v & 0xFF);
}

struct ItchMsg {
    static constexpr std::size_t WIRE_SIZE = 30;
    uint8_t type{0}; // 'A', 'E', 'D'
    uint64_t seq_num{0};
    uint64_t order_ref{0};
    uint8_t side{0}; // 'B', 'S'
    uint32_t shares{0};
    uint64_t price{0};

    static std::optional<ItchMsg> from_bytes(const uint8_t* buf) noexcept {
        ItchMsg m{};
        m.type = buf[0];
        m.seq_num = read_be64(&buf[1]);
        m.order_ref = read_be64(&buf[9]);
        m.side = buf[17];
        m.shares = read_be32(&buf[18]);
        m.price = read_be64(&buf[22]);
        if (m.type == 'A' || m.type == 'E') {
            if (m.side != 'B' && m.side != 'S') return std::nullopt;
            return m;
        }
        if (m.type == 'D') return m;
        return std::nullopt;
    }
};

struct PgmFrame {
    static constexpr std::size_t WIRE_SIZE = 52;
    static constexpr uint8_t VERSION = 0x81;

    uint8_t type{0}; // 0x00 SPM, 0x04 ODATA, 0x05 RDATA, 0x08 NAK, 0x09 NCF
    uint32_t tsi{0};
    uint64_t field_a{0};
    uint64_t field_b{0};
    ItchMsg itch{};

    static std::array<uint8_t, WIRE_SIZE> make_nak(uint32_t tsi, uint64_t start, uint64_t end) noexcept {
        std::array<uint8_t, WIRE_SIZE> buf{};
        buf[0] = VERSION;
        buf[1] = 0x08;
        write_be32(&buf[2], tsi);
        write_be64(&buf[6], start);
        write_be64(&buf[14], end);
        return buf;
    }

    static std::optional<PgmFrame> from_bytes(const uint8_t* buf) noexcept {
        if (buf[0] != VERSION) return std::nullopt;
        PgmFrame f{};
        f.type = buf[1];
        f.tsi = read_be32(&buf[2]);
        f.field_a = read_be64(&buf[6]);
        f.field_b = read_be64(&buf[14]);
        if (f.type == 0x04 || f.type == 0x05) {
            auto it = ItchMsg::from_bytes(&buf[22]);
            if (!it) return std::nullopt;
            f.itch = *it;
        }
        return f;
    }
};

struct OuchOutboundMsg {
    static constexpr std::size_t WIRE_SIZE = 30;
    uint8_t type{0}; // 'A', 'E', 'C'
    uint64_t cl_ord_id{0};
    uint64_t order_ref{0};
    uint8_t side{0};
    uint32_t shares{0};
    uint64_t price{0};

    static std::optional<OuchOutboundMsg> from_bytes(const uint8_t* buf) noexcept {
        OuchOutboundMsg m{};
        m.type = buf[0];
        m.cl_ord_id = read_be64(&buf[1]);
        m.order_ref = read_be64(&buf[9]);
        m.side = buf[17];
        m.shares = read_be32(&buf[18]);
        m.price = read_be64(&buf[22]);
        if (m.type == 'A' || m.type == 'E') {
            if (m.side != 'B' && m.side != 'S') return std::nullopt;
            return m;
        }
        if (m.type == 'C') return m;
        return std::nullopt;
    }
};

inline std::array<uint8_t, 22> encode_ouch_enter(
    uint64_t cl_ord_id, uint8_t side, uint32_t shares, uint64_t price) noexcept {
    std::array<uint8_t, 22> buf{};
    buf[0] = 'O';
    write_be64(&buf[1], cl_ord_id);
    buf[9] = side;
    write_be32(&buf[10], shares);
    write_be64(&buf[14], price);
    return buf;
}

inline std::array<uint8_t, 22> encode_ouch_cancel(uint64_t cl_ord_id) noexcept {
    std::array<uint8_t, 22> buf{};
    buf[0] = 'X';
    write_be64(&buf[1], cl_ord_id);
    return buf;
}

} // namespace wire
