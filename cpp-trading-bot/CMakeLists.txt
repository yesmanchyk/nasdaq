cmake_minimum_required(VERSION 3.16)
project(cpp_trading_bot LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

find_package(Threads REQUIRED)

add_compile_options(-Wall -Wextra -Wpedantic)
if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

add_executable(cpp_trading_bot
    src/main.cpp
)
target_include_directories(cpp_trading_bot PRIVATE src)
target_link_libraries(cpp_trading_bot PRIVATE Threads::Threads)

enable_testing()
add_executable(test_cpp_bot
    tests/test_cpp_bot.cpp
)
target_include_directories(test_cpp_bot PRIVATE src)
target_link_libraries(test_cpp_bot PRIVATE Threads::Threads)
add_test(NAME test_cpp_bot COMMAND test_cpp_bot)
