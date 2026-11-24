#pragma once

#include <array>
#include <cstdint>
#include <cstring>
#include <optional>
#include <string>

namespace common {

/// Fixed-point price scale matching itch-ouch-sim (`$100.0000` == `1_000_000`)
inline constexpr uint64_t PRICE_SCALE = 10'000;
inline constexpr uint64_t INITIAL_BOT_CAPITAL_DOLLARS = 100'000;
inline constexpr int64_t INITIAL_BOT_CAPITAL_FP =
    static_cast<int64_t>(INITIAL_BOT_CAPITAL_DOLLARS * PRICE_SCALE);

inline constexpr const char* DEFAULT_MCAST_GROUP = "239.255.0.1";
inline constexpr uint16_t DEFAULT_MCAST_PORT = 30001;
inline constexpr const char* DEFAULT_NAK_HOST = "127.0.0.1";
inline constexpr uint16_t DEFAULT_NAK_PORT = 30002;
inline constexpr const char* DEFAULT_OUCH_HOST = "127.0.0.1";
inline constexpr uint16_t DEFAULT_OUCH_PORT = 40000;

// Big-Endian helpers (network byte order)
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

enum class Side : uint8_t {
    Buy = 'B',
    Sell = 'S',
};

inline std::optional<Side> side_from_byte(uint8_t b) noexcept {
    if (b == 'B') return Side::Buy;
    if (b == 'S') return Side::Sell;
    return std::nullopt;
}

// ============================================================================
// OUCH Inbound (Bot -> Exchange over TCP, 22 bytes fixed frame)
// ============================================================================
enum class OuchInboundType : uint8_t {
    EnterOrder = 'O',
    CancelOrder = 'X',
};

struct OuchInbound {
    static constexpr std::size_t WIRE_SIZE = 22;

    OuchInboundType type{OuchInboundType::EnterOrder};
    uint64_t cl_ord_id{0};
    Side side{Side::Buy};
    uint32_t shares{0};
    uint64_t price{0};

    std::array<uint8_t, WIRE_SIZE> to_bytes() const noexcept {
        std::array<uint8_t, WIRE_SIZE> buf{};
        buf[0] = static_cast<uint8_t>(type);
        write_be64(&buf[1], cl_ord_id);
        if (type == OuchInboundType::EnterOrder) {
            buf[9] = static_cast<uint8_t>(side);
            write_be32(&buf[10], shares);
            write_be64(&buf[14], price);
        }
        return buf;
    }

    static std::optional<OuchInbound> from_bytes(const uint8_t* buf) noexcept {
        OuchInbound msg{};
        msg.cl_ord_id = read_be64(&buf[1]);
        if (buf[0] == 'O') {
            auto s = side_from_byte(buf[9]);
            if (!s) return std::nullopt;
            msg.type = OuchInboundType::EnterOrder;
            msg.side = *s;
            msg.shares = read_be32(&buf[10]);
            msg.price = read_be64(&buf[14]);
            return msg;
        }
        if (buf[0] == 'X') {
            msg.type = OuchInboundType::CancelOrder;
            return msg;
        }
        return std::nullopt;
    }
};

// ============================================================================
// OUCH Outbound (Exchange -> Bot over TCP, 30 bytes fixed frame)
// ============================================================================
enum class OuchOutboundType : uint8_t {
    OrderAccepted = 'A',
    OrderExecuted = 'E',
    OrderCanceled = 'C',
};

struct OuchOutbound {
    static constexpr std::size_t WIRE_SIZE = 30;

    OuchOutboundType type{OuchOutboundType::OrderAccepted};
    uint64_t cl_ord_id{0};
    uint64_t order_ref{0};
    Side side{Side::Buy};
    uint32_t shares{0}; // accepted / executed / canceled shares
    uint64_t price{0};  // limit price or fill price

    static std::optional<OuchOutbound> from_bytes(const uint8_t* buf) noexcept {
        OuchOutbound msg{};
        msg.cl_ord_id = read_be64(&buf[1]);
        msg.order_ref = read_be64(&buf[9]);
        msg.shares = read_be32(&buf[18]);
        msg.price = read_be64(&buf[22]);
        if (buf[0] == 'A') {
            auto s = side_from_byte(buf[17]);
            if (!s) return std::nullopt;
            msg.type = OuchOutboundType::OrderAccepted;
            msg.side = *s;
            return msg;
        }
        if (buf[0] == 'E') {
            auto s = side_from_byte(buf[17]);
            if (!s) return std::nullopt;
            msg.type = OuchOutboundType::OrderExecuted;
            msg.side = *s;
            return msg;
        }
        if (buf[0] == 'C') {
            msg.type = OuchOutboundType::OrderCanceled;
            return msg;
        }
        return std::nullopt;
    }
};

// ============================================================================
// ITCH 5.0 Market Data (Exchange -> All Bots over PGM Multicast, 30 bytes)
// ============================================================================
enum class ItchType : uint8_t {
    OrderAdded = 'A',
    OrderExecuted = 'E',
    OrderDeleted = 'D',
};

struct ItchMessage {
    static constexpr std::size_t WIRE_SIZE = 30;

    ItchType type{ItchType::OrderAdded};
    uint64_t seq_num{0};
    uint64_t order_ref{0};
    Side side{Side::Buy};
    uint32_t shares{0};
    uint64_t price{0};

    std::array<uint8_t, WIRE_SIZE> to_bytes() const noexcept {
        std::array<uint8_t, WIRE_SIZE> buf{};
        buf[0] = static_cast<uint8_t>(type);
        write_be64(&buf[1], seq_num);
        write_be64(&buf[9], order_ref);
        if (type == ItchType::OrderAdded || type == ItchType::OrderExecuted) {
            buf[17] = static_cast<uint8_t>(side);
            write_be32(&buf[18], shares);
            write_be64(&buf[22], price);
        }
        return buf;
    }

    static std::optional<ItchMessage> from_bytes(const uint8_t* buf) noexcept {
        ItchMessage msg{};
        msg.seq_num = read_be64(&buf[1]);
        msg.order_ref = read_be64(&buf[9]);
        msg.shares = read_be32(&buf[18]);
        msg.price = read_be64(&buf[22]);
        if (buf[0] == 'A') {
            auto s = side_from_byte(buf[17]);
            if (!s) return std::nullopt;
            msg.type = ItchType::OrderAdded;
            msg.side = *s;
            return msg;
        }
        if (buf[0] == 'E') {
            auto s = side_from_byte(buf[17]);
            if (!s) return std::nullopt;
            msg.type = ItchType::OrderExecuted;
            msg.side = *s;
            return msg;
        }
        if (buf[0] == 'D') {
            msg.type = ItchType::OrderDeleted;
            return msg;
        }
        return std::nullopt;
    }
};

// ============================================================================
// RFC 3208 UDP-Encapsulated PGM Wire Frame (52 bytes = 22 B header + 30 B ITCH)
// ============================================================================
enum class PgmPacketType : uint8_t {
    Spm = 0x00,
    Odata = 0x04,
    Rdata = 0x05,
    Nak = 0x08,
    Ncf = 0x09,
};

struct PgmFrame {
    static constexpr std::size_t WIRE_SIZE = 22 + ItchMessage::WIRE_SIZE; // 52 bytes
    static constexpr uint8_t VERSION_FLAGS = 0x81;

    PgmPacketType type{PgmPacketType::Odata};
    uint32_t tsi{0};
    uint64_t field_a{0}; // sqn / sqn_lead / missing_start / repair_start
    uint64_t field_b{0}; // txw_trail / missing_end / repair_end
    ItchMessage itch{};

    std::array<uint8_t, WIRE_SIZE> to_bytes() const noexcept {
        std::array<uint8_t, WIRE_SIZE> buf{};
        buf[0] = VERSION_FLAGS;
        buf[1] = static_cast<uint8_t>(type);
        write_be32(&buf[2], tsi);
        write_be64(&buf[6], field_a);
        write_be64(&buf[14], field_b);
        if (type == PgmPacketType::Odata || type == PgmPacketType::Rdata) {
            auto itch_bytes = itch.to_bytes();
            std::memcpy(&buf[22], itch_bytes.data(), ItchMessage::WIRE_SIZE);
        }
        return buf;
    }

    static std::optional<PgmFrame> from_bytes(const uint8_t* buf) noexcept {
        if (buf[0] != VERSION_FLAGS) return std::nullopt;
        PgmFrame f{};
        f.tsi = read_be32(&buf[2]);
        f.field_a = read_be64(&buf[6]);
        f.field_b = read_be64(&buf[14]);
        switch (buf[1]) {
            case 0x00:
                f.type = PgmPacketType::Spm;
                return f;
            case 0x04: {
                auto it = ItchMessage::from_bytes(&buf[22]);
                if (!it) return std::nullopt;
                f.type = PgmPacketType::Odata;
                f.itch = *it;
                return f;
            }
            case 0x05: {
                auto it = ItchMessage::from_bytes(&buf[22]);
                if (!it) return std::nullopt;
                f.type = PgmPacketType::Rdata;
                f.itch = *it;
                return f;
            }
            case 0x08:
                f.type = PgmPacketType::Nak;
                return f;
            case 0x09:
                f.type = PgmPacketType::Ncf;
                return f;
            default:
                return std::nullopt;
        }
    }
};

} // namespace common
