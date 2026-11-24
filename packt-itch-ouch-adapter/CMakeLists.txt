cmake_minimum_required(VERSION 3.16)
project(packt_itch_ouch_adapter LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Threads REQUIRED)

if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

set(PACKT_CH12_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../external/Building-Low-Latency-Applications-with-CPP/Chapter12")

if(NOT EXISTS "${PACKT_CH12_DIR}/trading/strategy/trade_engine.cpp")
    message(FATAL_ERROR "Packt submodule not found at ${PACKT_CH12_DIR}. Run: git submodule update --init --recursive")
endif()

# Compile the real upstream Packt Chapter 12 trading strategy & engine sources directly
set(PACKT_STRATEGY_SOURCES
    "${PACKT_CH12_DIR}/trading/strategy/liquidity_taker.cpp"
    "${PACKT_CH12_DIR}/trading/strategy/market_maker.cpp"
    "${PACKT_CH12_DIR}/trading/strategy/market_order.cpp"
    "${PACKT_CH12_DIR}/trading/strategy/market_order_book.cpp"
    "${PACKT_CH12_DIR}/trading/strategy/order_manager.cpp"
    "${PACKT_CH12_DIR}/trading/strategy/risk_manager.cpp"
    "${PACKT_CH12_DIR}/trading/strategy/trade_engine.cpp"
)

add_executable(packt_itch_ouch_bot
    src/main.cpp
    ${PACKT_STRATEGY_SOURCES}
)

target_include_directories(packt_itch_ouch_bot PRIVATE
    src
    "${PACKT_CH12_DIR}"
    "${PACKT_CH12_DIR}/trading"
)

target_compile_options(packt_itch_ouch_bot PRIVATE -O3 -Wall -Wextra -Wno-unused-parameter)
target_link_libraries(packt_itch_ouch_bot PRIVATE Threads::Threads)
