#include <ranges>
#include <string>
#include <iostream>

#include <asio/io_context.hpp>
#include <asio/ip/tcp.hpp>
#include <asio/connect.hpp>

#include <asio/co_spawn.hpp>
#include <asio/detached.hpp>
#include <asio/signal_set.hpp>
#include <asio/write.hpp>

using asio::awaitable;
using asio::ip::tcp;

awaitable<void> async_client(tcp::socket socket)
{
	using std::ranges::views::iota;
	using std::cout;
	using std::endl;
	union
	{
		uint16_t n;
		char b[2];
	} u16{ 0 };
	char data[1024] = {};
	for (int i : iota(0, 1e9))
	{
		std::size_t n = co_await socket.async_read_some(asio::buffer(u16.b + 1, 1), asio::use_awaitable);
		if (n != 1) break;
		n = co_await socket.async_read_some(asio::buffer(u16.b, 1), asio::use_awaitable);
		if (n != 1) break;
		if (sizeof(data) < u16.n) break;
		n = co_await socket.async_read_some(asio::buffer(data, u16.n), asio::use_awaitable);
		if (n < 1) break;
		cout << i << ": " << n << "-byte message " << *data << endl;
	}
}

void client(tcp::socket socket)
{
	using std::ranges::views::iota;
	using std::cout;
	using std::endl;
	union
	{
		uint16_t n;
		char b[2];
	} u16{ 0 };
	std::array<char, 1024> data;
	for (int i : iota(0, 1e9))
	{
		std::error_code error;
		size_t n = socket.read_some(asio::buffer(u16.b + 1, 1), error);
		if (n != 1) break;
		n = socket.read_some(asio::buffer(u16.b, 1), error);
		if (n != 1) break;
		n = socket.read_some(asio::buffer(data, u16.n), error);
		if (error == asio::error::eof)
			break; // Connection closed cleanly by peer.
		else if (error)
			throw std::system_error(error); // Some other error.
		if (n < 1) break;
		cout << i << ": " << n << "-byte message " << *data.begin() << endl;
	}
}

int main(int argc, char* argv[])
{
	try
	{
		std::string host = "localhost";
		std::string service = "55555";
		if (argc > 1) host = argv[1];
		if (argc > 2) service = argv[2];
		asio::io_context io_context;
		tcp::resolver resolver(io_context);
		tcp::resolver::results_type endpoints =
			resolver.resolve(host, service);
		tcp::socket socket(io_context);
		asio::connect(socket, endpoints);
		client(std::move(socket));
		return 0;
	}
	catch (std::exception& e)
	{
		std::cerr << "exception: " << e.what() << std::endl;
		return 1;
	}
}