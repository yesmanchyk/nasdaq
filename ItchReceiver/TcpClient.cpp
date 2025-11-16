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

#include "Messages.cpp"
#include "Book.cpp"

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
	std::string_view ticker = "GOOG    ";
	Book book;
	auto printBook = [&book]() {
		std::cout << "bids:\n";
		for (auto& entry : book.state(true)) {
			std::cout << std::setw(8)
				<< std::get<0>(entry)
				<< " " << std::get<1>(entry)
				<< " x " << std::get<2>(entry)
				<< std::endl;
		}
		std::cout << "asks:\n";
		for (auto& entry : book.state(false)) {
			std::cout << std::setw(8)
				<< std::get<0>(entry) 
				<< " " << std::get<1>(entry)
				<< " x " << std::get<2>(entry)
				<< std::endl;
		}
	};

	std::unordered_map<Book::OrderIdType, std::string> orderSymbols;
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
		auto buf = &data[0];
		if (*buf == 'A' || *buf == 'F') {
			// hex dump
			for (auto j : std::ranges::views::iota(0u, n)) {
				int h = buf[j];
				h &= 255;
				cout << std::hex << std::setfill('0') << std::setw(2) << h << " ";
			}
			cout << std::dec << endl;
			//
			auto* message = reinterpret_cast<AddOrder*>(buf);
			auto orderId = message->orderId.get();
			double todayPrice = message->price.get();
			std::string_view symbol(message->symbol, 8);
			orderSymbols[orderId] = symbol;
			if (ticker == symbol) {
				std::cout << "adding order " << orderId 
					<< " of " << symbol << std::endl;

				todayPrice = message->price.get() / 1e4;
				todayPrice /= 20.0; // split in 2022

				book.add(
					message->side == 'B',
					message->orderId.get(),
					message->price.get(),
					message->quantity.get()
				);
				printBook();
			}

			std::cout << std::string_view(message->symbol, 8)
				<< " " << message->orderId.get()
				<< " " << message->side
				<< " " << message->quantity.get()
				<< " x " << message->price.get()
				<< " aka " << todayPrice
				<< std::endl;

		}
		else if (*buf == 'C' || *buf == 'E') {
			auto* message = reinterpret_cast<OrderExec*>(buf);
			auto orderId = message->orderId.get();
			std::cout << "orderId = " << orderId
				<< ", volume = " << message->quantity.get()
				<< std::endl;
			if (orderSymbols[orderId] == ticker) {
				std::cout << "executing order of " << ticker << std::endl;
				book.exec(orderId, message->quantity.get());
				printBook();
			}
		}
		else if (*buf == 'X') {
			auto* message = reinterpret_cast<CancelOrder*>(buf);
			auto orderId = message->orderId.get();
			std::cout << "orderId = " << orderId << std::endl;
			if (orderSymbols[orderId] == ticker) {
				std::cout << "canceling order of " << ticker << std::endl;
				book.exec(orderId, message->quantity.get());
				printBook();
			}
		}
		else if (*buf == 'D') {
			auto* message = reinterpret_cast<DelOrder*>(buf);
			auto orderId = message->orderId.get();
			std::cout << "orderId = " << orderId << std::endl;
			if (orderSymbols[orderId] == ticker) {
				std::cout << "deleting order of " << ticker << std::endl;
				book.del(orderId);
				printBook();
			}
			orderSymbols.erase(orderId);
		}
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