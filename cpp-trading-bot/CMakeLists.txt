cmake_minimum_required(VERSION 3.16...3.28)
project(cpp_trading_bot LANGUAGES CXX)

option(USE_CXX_MODULES "Build with C++20 Modules (requires CMake 3.28+, Ninja/VS, and Clang 16+/GCC 14+/MSVC)" OFF)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Threads REQUIRED)

add_compile_options(-Wall -Wextra -Wpedantic)
if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

if(USE_CXX_MODULES)
    if(CMAKE_VERSION VERSION_LESS 3.28)
        message(FATAL_ERROR "USE_CXX_MODULES=ON requires CMake 3.28 or newer (found ${CMAKE_VERSION}).")
    endif()

    add_library(cpp_trading_bot_modules STATIC)
    target_sources(cpp_trading_bot_modules
        PUBLIC
            FILE_SET CXX_MODULES FILES
                modules/common.cppm
                modules/trading.cppm
    )
    target_include_directories(cpp_trading_bot_modules PRIVATE src)
    target_link_libraries(cpp_trading_bot_modules PUBLIC Threads::Threads)
endif()

add_executable(cpp_trading_bot
    src/main.cpp
)
if(USE_CXX_MODULES)
    target_link_libraries(cpp_trading_bot PRIVATE cpp_trading_bot_modules)
    target_compile_definitions(cpp_trading_bot PRIVATE USE_CXX_MODULES)
else()
    target_include_directories(cpp_trading_bot PRIVATE src)
    target_link_libraries(cpp_trading_bot PRIVATE Threads::Threads)
endif()

enable_testing()
add_executable(test_cpp_bot
    tests/test_cpp_bot.cpp
)
if(USE_CXX_MODULES)
    target_link_libraries(test_cpp_bot PRIVATE cpp_trading_bot_modules)
    target_compile_definitions(test_cpp_bot PRIVATE USE_CXX_MODULES)
else()
    target_include_directories(test_cpp_bot PRIVATE src)
    target_link_libraries(test_cpp_bot PRIVATE Threads::Threads)
endif()
add_test(NAME test_cpp_bot COMMAND test_cpp_bot)
