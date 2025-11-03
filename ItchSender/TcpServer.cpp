
#include <iostream>
#include <fstream>
#include <ranges>

#include <asio/co_spawn.hpp>
#include <asio/detached.hpp>
#include <asio/io_context.hpp>
#include <asio/ip/tcp.hpp>
#include <asio/signal_set.hpp>
#include <asio/write.hpp>

using asio::ip::tcp;
using asio::awaitable;
using asio::co_spawn;
using asio::detached;
using asio::use_awaitable;
namespace this_coro = asio::this_coro;

awaitable<void> itch(const std::string& path, tcp::socket socket)
{
    using std::ranges::views::iota;
    using std::cout;
    using std::endl;
    cout << "Accepted " << socket.remote_endpoint().address().to_string() << endl;
    std::ifstream f{ path, std::ios::binary };
    if (f.good())
    {
        union
        {
            uint16_t n;
            char b[2];
        } u16{ 0 };
        try
        {
            char data[1024];
            for (int i : iota(0, 1e9)) {
                u16.n = 0;
                if (!f.good()) break;
                f.read(u16.b + 1, 1);
                f.read(u16.b, 1);
                auto n = u16.n;
                //std::cout << n << "=" << std::hex << n << std::endl;
                if (sizeof(data) < n) break;
                if (!f.good()) break;
                f.read(data, n);
                co_await async_write(socket, asio::buffer(u16.b + 1, 1), use_awaitable);
                co_await async_write(socket, asio::buffer(u16.b, 1), use_awaitable);
                co_await async_write(socket, asio::buffer(data, n), use_awaitable);
            }
        }
        catch (std::exception& e)
        {
            std::cout << "Exception: " << e.what() << std::endl;
        }
    }
    else
    {
        cout << "Can not open " << path << endl;
    }
    socket.close();
}

awaitable<void> listener(const std::string& path)
{
    auto executor = co_await this_coro::executor;
    tcp::acceptor acceptor(executor, { tcp::v4(), 55555});
    for (;;)
    {
        tcp::socket socket = co_await acceptor.async_accept(use_awaitable);
        co_spawn(executor, itch(path, std::move(socket)), detached);
    }
}

int main(int argc, char* argv[])
{
    using std::cout;
    using std::endl;
    std::string path = "12302019.NASDAQ_ITCH50";
    if (argc > 1) path = argv[1];
    {
        // test path
        std::ifstream f{ path, std::ios::binary };
        if (!f.good()) {
            cout << "usage: ItchSender <ITCH50-file-from-https://emi.nasdaq.com/ITCH/Nasdaq%20ITCH/>" << endl;
            return 1;
        }
    }
    try
    {
        asio::io_context io_context(1);

        asio::signal_set signals(io_context, SIGINT, SIGTERM);
        signals.async_wait([&](auto, auto) { io_context.stop(); });

        co_spawn(io_context, listener(path), detached);

        io_context.run();
    }
    catch (std::exception& e)
    {
        std::cout << "Exception: " << e.what() << std::endl;
    }
}