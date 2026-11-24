module;

#include "trading/feature_engine.hpp"
#include "trading/market_data_consumer.hpp"
#include "trading/market_order_book.hpp"
#include "trading/order_gateway.hpp"
#include "trading/risk_manager.hpp"
#include "trading/trade_engine.hpp"

export module trading;

export import common;

export namespace trading {

using ::trading::MarketOrder;
using ::trading::BBO;
using ::trading::MarketOrderBook;

using ::trading::FEATURE_INVALID;
using ::trading::FeatureEngine;

using ::trading::PositionSnapshot;
using ::trading::RiskConfig;
using ::trading::RiskManager;

using ::trading::MarketDataConsumer;
using ::trading::OrderGateway;

using ::trading::AlgoMode;
using ::trading::TradeEngineConfig;
using ::trading::TradeEngine;

} // namespace trading
