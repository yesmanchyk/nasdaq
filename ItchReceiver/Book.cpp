
#include <string_view>
#include <string>
#include <unordered_map>
#include <vector>
#include <iostream>

class Book {
public:
	using OrderIdType = long;
	using PriceType = long;
	using VolumeType = long;
	using SortedOrders = std::vector<std::tuple<OrderIdType, PriceType, VolumeType>>;

	void add(bool buy, OrderIdType orderId, PriceType price, VolumeType volume) {
		auto& orders = buy ? bids : asks;
		orders[orderId] = {price, volume};
	}

	void exec(OrderIdType orderId, VolumeType volume) {
		int misses = 0;
		std::array<Orders*, 2> books = { &asks, &bids };
		for (auto orders : books) {
			auto order = orders->find(orderId);
			if (orders->end() == order) {
				++misses;
				continue;
			}
			order->second.second -= volume;
			if (order->second.second <= 0) {
				orders->erase(order);
			}
		}
		if (misses > 1) {
			std::cout << "Failed to find order " << orderId << std::endl;
		}
	}

	void del(OrderIdType orderId) {
		int misses = 0;
		std::array<Orders*, 2> books = { &asks, &bids };
		for (auto orders : books) {
			auto order = orders->find(orderId);
			if (orders->end() == order) {
				++misses;
			}
			else {
				orders->erase(order);
			}
		}
		if (misses > 1) {
			std::cout << "Failed to find order " << orderId << std::endl;
		}
	}

	SortedOrders state(bool buy) {
		SortedOrders result;
		auto& orders = buy ? bids : asks;
		for (const auto& order : orders) {
			auto [price, volume] = order.second;
			result.emplace_back(std::make_tuple(
				order.first, price, volume
			));
		}
		std::sort(result.begin(), result.end(),
			[](const auto& lhs, const auto& rhs) {
				return std::get<1>(lhs) < std::get<1>(rhs);
			});
		return result;
	}
private:
	using Orders = std::unordered_map<OrderIdType, std::pair<PriceType, VolumeType>>;	
	Orders asks;
	Orders bids;
};