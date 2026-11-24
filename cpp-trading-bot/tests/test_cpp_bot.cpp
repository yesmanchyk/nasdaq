#include "common/lf_queue.hpp"
#include "common/mem_pool.hpp"
#include "common/protocol.hpp"
#include "trading/feature_engine.hpp"
#include "trading/market_order_book.hpp"
#include "trading/risk_manager.hpp"

#include <cmath>
#include <cstdlib>
#include <iostream>

#define REQUIRE(cond)                                                                      \
    do {                                                                                   \
        if (!(cond)) {                                                                     \
            std::cerr << "Test failed at line " << __LINE__ << ": " #cond << std::endl;    \
            std::exit(1);                                                                  \
        }                                                                                  \
    } while (0)

void test_lf_queue() {
    common::LFQueue<int> q(8);
    REQUIRE(q.getNextToRead() == nullptr);
    for (int i = 1; i <= 7; ++i) {
        REQUIRE(q.push(i * 10));
    }
    REQUIRE(!q.push(999)); // Full (capacity - 1 usable slots)
    for (int i = 1; i <= 7; ++i) {
        const int* val = q.getNextToRead();
        REQUIRE(val != nullptr && *val == i * 10);
        q.updateReadIndex();
    }
    REQUIRE(q.getNextToRead() == nullptr);
}

void test_mem_pool() {
    common::MemPool<trading::MarketOrder> pool(4);
    auto* a = pool.allocate(101, common::Side::Buy, 1'000'000, 100);
    auto* b = pool.allocate(102, common::Side::Sell, 1'000'100, 200);
    REQUIRE(a != nullptr && b != nullptr);
    REQUIRE(pool.allocatedCount() == 2);
    pool.deallocate(a);
    REQUIRE(pool.allocatedCount() == 1);
    auto* c = pool.allocate(103, common::Side::Buy, 999'900, 50);
    REQUIRE(c != nullptr && c->order_ref == 103);
    REQUIRE(pool.allocatedCount() == 2);
}

void test_wire_protocol_roundtrip() {
    common::OuchInbound enter{};
    enter.type = common::OuchInboundType::EnterOrder;
    enter.cl_ord_id = 5'000'042;
    enter.side = common::Side::Buy;
    enter.shares = 75;
    enter.price = 1'000'250; // $100.0250
    auto enter_bytes = enter.to_bytes();
    REQUIRE(enter_bytes[0] == 'O');
    auto enter_dec = common::OuchInbound::from_bytes(enter_bytes.data());
    REQUIRE(enter_dec.has_value());
    REQUIRE(enter_dec->cl_ord_id == 5'000'042);
    REQUIRE(enter_dec->side == common::Side::Buy);
    REQUIRE(enter_dec->shares == 75);
    REQUIRE(enter_dec->price == 1'000'250);

    common::PgmFrame frame{};
    frame.type = common::PgmPacketType::Odata;
    frame.tsi = 0x514D4431;
    frame.field_a = 1234;
    frame.field_b = 1;
    frame.itch.type = common::ItchType::OrderAdded;
    frame.itch.seq_num = 1234;
    frame.itch.order_ref = 999;
    frame.itch.side = common::Side::Sell;
    frame.itch.shares = 150;
    frame.itch.price = 1'000'500;

    auto pgm_bytes = frame.to_bytes();
    REQUIRE(pgm_bytes[0] == 0x81);
    REQUIRE(pgm_bytes[1] == 0x04);
    auto pgm_dec = common::PgmFrame::from_bytes(pgm_bytes.data());
    REQUIRE(pgm_dec.has_value());
    REQUIRE(pgm_dec->type == common::PgmPacketType::Odata);
    REQUIRE(pgm_dec->field_a == 1234);
    REQUIRE(pgm_dec->itch.order_ref == 999);
    REQUIRE(pgm_dec->itch.shares == 150);
    REQUIRE(pgm_dec->itch.price == 1'000'500);
}

void test_order_book_and_feature_engine() {
    trading::MarketOrderBook book(1024);
    trading::FeatureEngine fe;

    common::ItchMessage bid{common::ItchType::OrderAdded, 1, 10, common::Side::Buy, 300, 999'900};
    common::ItchMessage ask{common::ItchType::OrderAdded, 2, 11, common::Side::Sell, 100, 1'000'100};
    book.onMarketUpdate(bid);
    book.onMarketUpdate(ask);
    fe.onOrderBookUpdate(book);

    REQUIRE(book.midPrice() == 1'000'000);
    REQUIRE(std::abs(fe.getMktPrice() - 1'000'050.0) < 1e-6);
    REQUIRE(std::abs(fe.getObi() - 0.5) < 1e-6);

    common::ItchMessage exec{common::ItchType::OrderExecuted, 3, 11, common::Side::Sell, 50, 1'000'100};
    fe.onTradeUpdate(exec, book);
    REQUIRE(std::abs(fe.getAggTradeQtyRatio() - 0.5) < 1e-6);
    REQUIRE(fe.getLastAggressorSide() == common::Side::Buy);
}

void test_risk_manager_capital_limits() {
    trading::RiskManager rm;
    REQUIRE(rm.checkPreTradeRisk(common::Side::Buy, 250, 1'000'000, 1'000'000) == 200);
    rm.onFill(common::Side::Buy, 600, 1'000'000);
    REQUIRE(rm.checkPreTradeRisk(common::Side::Buy, 50, 1'000'000, 1'000'000) == 0);
    REQUIRE(rm.checkPreTradeRisk(common::Side::Sell, 100, 1'000'000, 1'000'000) == 100);
}

int main() {
    test_lf_queue();
    test_mem_pool();
    test_wire_protocol_roundtrip();
    test_order_book_and_feature_engine();
    test_risk_manager_capital_limits();
    std::cout << "All C++ Low-Latency Trading Bot unit tests passed!" << std::endl;
    return 0;
}
