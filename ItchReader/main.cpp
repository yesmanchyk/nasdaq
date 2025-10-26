//
//  main.cpp
//  ItchReader
//
//  Created by Alex Yesmanchyk on 10/18/25.
//

#include <iostream>
#include <fstream>
#include <ranges>
#include <string_view>
#include <chrono>
#include <vector>
#include <algorithm>
#include <unordered_map>
#include <array>
#include <atomic>
#include <thread>

enum ReadType {
    deserialize,
    packed,
    threaded
};

//constexpr ReadType readType = packed;
constexpr ReadType readType = threaded;


//
//struct Int32Buffer {
//    char value[4];
//    constexpr int size() const { return sizeof(value); }
//    constexpr char operator[](int i) const { return value[i]; }
//};

template<int N>
struct CharBuffer {
    char value[N];
    constexpr int size() const { return sizeof(value); }
    constexpr char operator[](int i) const { return value[i]; }
};

using Int32Buffer = CharBuffer<4>;

//class Int32BufferOffset {
//    const char* buffer;
//    int offset;
//public:
//    Int32BufferOffset(const char* buf, int off) : buffer(buf), offset(off) {}
//    constexpr int size() const { return 4; }
//    char operator[](int i) const { return buffer[offset+i]; }
//};

template<int N>
class BufferOffset {
    const char* buffer;
    int offset;
public:
    BufferOffset(const char* buf, int off) : buffer(buf), offset(off) {}
    constexpr int size() const { return N; }
    char operator[](int i) const { return buffer[offset+i]; }
};

using Int32BufferOffset = BufferOffset<4>;



template<typename T, typename R>
R bigEndianRead(const T& buf) {
    R price = 0;
    using std::ranges::views::iota;
    for (auto j : iota(0, (int)buf.size())) {
        int h = buf[j] & 0x0ff;
        //h &= 255;
        price <<= 8;
        price |= h;
    }
    return price;
}

template<typename T, typename R>
class BigEndian {
    T buf;
    
public:
    R get() {
        return bigEndianRead<T, R>(buf);
    }
};

template<>
class BigEndian<BufferOffset<2>, int16_t> {
    const BufferOffset<2>& buf;
    
public:
    BigEndian(const BufferOffset<2>& ob) : buf(ob) {}
    
    int32_t get() {
        return bigEndianRead<BufferOffset<2>, int16_t>(buf);
    }
};

template<>
class BigEndian<Int32BufferOffset, int32_t> {
    const Int32BufferOffset& buf;
    
public:
    BigEndian(const Int32BufferOffset& ob) : buf(ob) {}
    
    int32_t get() {
        return bigEndianRead<Int32BufferOffset, int32_t>(buf);
    }
};


class TradeMessageLayout {
public:
    static constexpr int FieldCount = 10;
    
    enum Fields {
        type = 0,
        symbolLocate = 1,
        trackingNumber = 2,
        timestamp = 3,
        orderId = 4,
        side = 5,
        quantity = 6,
        symbol = 7,
        price = 8,
        matchId = 9
    };
    
    static constexpr std::array<int, FieldCount> offsets = {
        0, 1, 3, 5, 11, 19, 20, 24, 32, 36
    };
    
    static constexpr std::array<int, FieldCount> sizes = {
        1, 2, 2, 6, 8, 1, 4, 8, 4, 8
    };

private:
    const char* buffer;
    
    
    struct Field {
        const char* bf;
        int sz;
        
        constexpr int size() const { return sz; }
        char operator[](int i) const { return bf[i]; }
    };
    
public:
    TradeMessageLayout(const char* buf) : buffer(buf) {}
    
    template<typename R>
    R get(Fields fieldId) {
        Field field {
            .bf = buffer + offsets[fieldId],
            .sz = sizes[fieldId]
        };
        return bigEndianRead<Field, R>(field);
    }

    static constexpr int getOffset(Fields fieldId) { return offsets[fieldId]; }
public:
    int16_t getSymbolLocate() {
        return get<int16_t>(Fields::symbolLocate);
    }
    
    int32_t getPrice() {
        return get<int32_t>(Fields::price);
    }
    
    int32_t getVolume() {
        return get<int32_t>(Fields::quantity);
    }
};

// as per https://stackoverflow.com/questions/1537964/visual-c-equivalent-of-gccs-attribute-packed
#ifdef _MSC_VER
__pragma(pack(push, 1)) // MSVC
#endif
struct 
#ifdef __GNUC__
    __attribute__((packed)) // Clang
#endif
    TradeMessage {
    char type;
    // char unused[44 - 8 - 4 - 8 - 4 - 1 - 8 - 6 - 2 - 2 - 1];
    BigEndian<CharBuffer<2>, int16_t> symbolLocate;
    BigEndian<CharBuffer<2>, int16_t> trackingNumber;
    BigEndian<CharBuffer<6>, int64_t> timestamp;
    BigEndian<CharBuffer<8>, int64_t> orderId;
    char side;
    BigEndian<Int32Buffer, int32_t> quantity;
    char symbol[8];
    BigEndian<Int32Buffer, int32_t> price;
    BigEndian<CharBuffer<8>, int64_t> matchId;
};
#ifdef _MSC_VER
__pragma(pack(pop)) // MSVC
#endif



std::unordered_map<std::string, long> symbolTrades;

int main(int argc, const char * argv[]) {
    using std::chrono::steady_clock;
    using std::chrono::duration_cast;
    using std::chrono::nanoseconds;
    using std::ranges::views::iota;
    using std::cout;
    using std::endl;
    
    std::cout << *argv << std::endl;
    std::string path =
    //"07302019.NASDAQ_ITCH50";
    "12302019.NASDAQ_ITCH50";
    if (argc > 1) path = argv[1];
    std::ifstream f{path, std::ios::binary};
    if (!f.good()) {
        cout << "usage: ItchReader <ITCH50-file-from-https://emi.nasdaq.com/ITCH/Nasdaq%20ITCH/>" << endl;
        return 1;
    }
    union {
        uint16_t n;
        char b[2];
    } u16;
    char buf[1024];
    int counts[256]{};
    std::string_view ticker = "GOOG    ";
    std::vector<int64_t> durations;
    //int got = 0;
    for (int i : iota(0, 256)) counts[i] = 0;
    int tslas = 0;
    std::atomic<double> totalValueTraded = 0;
    constexpr int calcBufferSize = 1024;
    std::array<std::array<char, 4>, calcBufferSize> prices;
    //char prices[calcBufferSize][4];
    std::array<std::array<char, 4>, calcBufferSize> volumes;
    struct {
        std::atomic<int> produced;
        std::atomic<int> consumed;
        std::atomic<bool> done;
    } work{0, 0, readType != threaded};
    std::jthread worker([&]() {
            double threadTotalValueTraded = 0;
            using namespace std::literals;
            cout << "worker thread started" << endl;
            int consumed = 0;
            while (!work.done.load()) {
                int produced = work.produced.load();
                //int consumed = work.consumed.load();
                //cout << produced << " vs " << consumed << endl;
                if (consumed < produced) {
                    int32_t price = bigEndianRead<std::array<char, 4>, int32_t>(prices[consumed % calcBufferSize]);
                    int32_t volume = bigEndianRead<std::array<char, 4>, int32_t>(volumes[consumed % calcBufferSize]);
                    //work.consumed.fetch_add(1);
                    ++consumed;
                    //cout << consumed << ": " << price << " * " << volume << endl;
                    threadTotalValueTraded += price * volume;
                } else {
                    std::this_thread::sleep_for(1ms);
                }
            }
            cout << "worker thread ended" << endl;
            totalValueTraded.store(threadTotalValueTraded);
        });
    for (int i : iota(0, 1e9)) {
        u16.n = 0;
        if (!f.good()) break;
        f.read(u16.b + 1, 1);
        f.read(u16.b, 1);
        auto n = u16.n;
        //std::cout << n << "=" << std::hex << n << std::endl;
        if (sizeof(buf) < n) break;
        if (!f.good()) break;
        f.read(buf, n);
        //if (got < 1) break;
        char type = *buf;
        ++counts[type];
        if ('P' == type) {
            std::string_view symbol{&buf[44 - 8 - 4 - 8], 8};
            std::string key{ symbol };
            ++symbolTrades[key];
            if (symbol == ticker) {
                ++tslas;

                auto start = steady_clock::now();
                
                int price = 0, volume = 0, trackingNumber = 0;
                if constexpr (readType == deserialize) {
                    TradeMessageLayout layout(buf);
//                    Int32BufferOffset bufferOffset(buf, 44 - 8 - 4);
//                    BigEndian<Int32BufferOffset, int32_t> priceReader(bufferOffset);
//                    price = priceReader.get();
                    price = layout.getPrice();
//                    Int32BufferOffset volumeOffset(buf, 44 - 8 - 4 - 8 - 4);
//                    BigEndian<Int32BufferOffset, int32_t> volumeReader(volumeOffset);
//                    volume = volumeReader.get();
                    volume = layout.getVolume();
//                    BufferOffset<2> trackingNumberOffset(buf, 1);
//                    BigEndian<BufferOffset<2>, int16_t> trackinNumberReader(trackingNumberOffset);
//                    trackingNumber = trackinNumberReader.get();
                    //trackingNumber = layout.getSymbolLocate();
                    totalValueTraded += price * volume;
                    // TODO use SoA of unconverted prices and volumes to parrallelize bigEndianRead()
                } else if constexpr (readType == packed) {
                    auto message = reinterpret_cast<TradeMessage*>(buf);
                    //auto message = new (buf) TradeMessage;
                    price = message->price.get();
                    volume = message->quantity.get();
                    //trackingNumber = message->symbolLocate.get();
                    totalValueTraded += price * volume;
                }
                else if constexpr (readType == threaded) {
                    int produced = work.produced.load();
                    int o = TradeMessageLayout::getOffset(TradeMessageLayout::Fields::price);
                    for (int j : iota(0, 4)) prices[produced % calcBufferSize][j] = buf[o + j];
                    o = TradeMessageLayout::getOffset(TradeMessageLayout::Fields::quantity);
                    for (int j : iota(0, 4)) volumes[produced % calcBufferSize][j] = buf[o + j];
                    work.produced.fetch_add(1);
                }
                // TODO get raw price and put into a circular buffer for a separate thread
                
                auto end = steady_clock::now();
                auto duration = duration_cast<nanoseconds>(end - start);
                durations.push_back(duration.count());
                
                int32_t expectedPrice = 0;
                // big endian int to little endian
                /*for (auto j : iota(0, 4)) {
                    int h = buf[44 - 8 - 4 + j];
                    h &= 255;
                    expectedPrice <<= 8;
                    expectedPrice |= h;
                }
                
                if (price != expectedPrice) {
                    cout << price << " != " << expectedPrice << " at message " << i << endl;
                }*/
                
                // hex dump
                for (auto j : std::ranges::views::iota(0u, n)) {
                    int h = buf[j];
                    h &= 255;
                    //cout << std::hex << std::setfill('0') << std::setw(2) << h << " ";
                }
                //cout << std::dec << endl;
                
                double todayPrice = price / 1e4;
                todayPrice /= 20.0; // split in 2022
                if (i % 100 == 0) {
                    cout << symbol << " "
                         << std::dec << todayPrice << " " << volume << endl;
                    // << message->price.get()
                    // << priceReader.get()
                    // << endl;
                    // << endl;
                }
            }
            
        }
        (void)i;
    }
    work.done.store(true);
    for (auto e : symbolTrades) {
        if (e.second < 2e3) continue;
        cout << e.first << ": " << e.second << endl;
    }
    
    int n = 0;
    for (int i : iota(0, 256))
        if (counts[i] > 0) {
            n += counts[i];
            u16.n = i;
            std::cout << std::dec << u16.b[0] << ":" << counts[i] << std::endl;
        }
    cout << "Total: " << n << ", " << ticker << " Trades: " << tslas << ", Total Value Traded: " << totalValueTraded.load() << endl;
    std::sort(durations.begin(), durations.end());
    int64_t totalTime = 0;
    for (auto duration : durations) totalTime += duration;
    cout << "Total time taken: " << totalTime << " us, "
         << "Median time taken: " << durations[durations.size() / 2] << " us"
         << endl;
    return 0;
}
