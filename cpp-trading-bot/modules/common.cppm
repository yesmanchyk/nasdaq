module;

#include "common/lf_queue.hpp"
#include "common/mem_pool.hpp"
#include "common/protocol.hpp"

export module common;

export namespace common {

using ::common::LFQueue;
using ::common::MemPool;

using ::common::PRICE_SCALE;
using ::common::INITIAL_BOT_CAPITAL_DOLLARS;
using ::common::INITIAL_BOT_CAPITAL_FP;

using ::common::DEFAULT_MCAST_GROUP;
using ::common::DEFAULT_MCAST_PORT;
using ::common::DEFAULT_NAK_HOST;
using ::common::DEFAULT_NAK_PORT;
using ::common::DEFAULT_OUCH_HOST;
using ::common::DEFAULT_OUCH_PORT;

using ::common::read_be32;
using ::common::read_be64;
using ::common::write_be32;
using ::common::write_be64;

using ::common::Side;
using ::common::side_from_byte;

using ::common::OuchInboundType;
using ::common::OuchInbound;

using ::common::OuchOutboundType;
using ::common::OuchOutbound;

using ::common::ItchType;
using ::common::ItchMessage;

using ::common::PgmPacketType;
using ::common::PgmFrame;

} // namespace common
