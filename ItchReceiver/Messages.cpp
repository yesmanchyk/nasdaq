#include <ranges>

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

template<int N>
struct CharBuffer {
    char value[N];
    constexpr int size() const { return sizeof(value); }
    constexpr char operator[](int i) const { return value[i]; }
};

using Int32Buffer = CharBuffer<4>;


// as per https://stackoverflow.com/questions/1537964/visual-c-equivalent-of-gccs-attribute-packed
#ifdef _MSC_VER
#define MESSAGE __pragma(pack(push, 1)) struct // MSVC
#else
//#ifdef __GNUC__
#define MESSAGE struct __attribute__((packed)) // Clang
#endif

#ifdef _MSC_VER
#define MESSAGEND ; __pragma(pack(pop)) // MSVC
#else
#define MESSAGEND ;
#endif


MESSAGE StockDirectoryHeader {
    char type;
    BigEndian<CharBuffer<2>, int16_t> symbolLocate;
    BigEndian<CharBuffer<2>, int16_t> trackingNumber;
    BigEndian<CharBuffer<6>, int64_t> timestamp;
    char symbol[8];
} MESSAGEND

MESSAGE AddOrder {
    char type;
    BigEndian<CharBuffer<2>, int16_t> symbolLocate;
    BigEndian<CharBuffer<2>, int16_t> trackingNumber;
    BigEndian<CharBuffer<6>, int64_t> timestamp;
    BigEndian<CharBuffer<8>, int64_t> orderId;
    char side;
    BigEndian<CharBuffer<4>, int32_t> quantity;
    char symbol[8];
    BigEndian<CharBuffer<4>, int32_t> price;
} MESSAGEND

MESSAGE OrderExec {
    char type;
    BigEndian<CharBuffer<2>, int16_t> symbolLocate;
    BigEndian<CharBuffer<2>, int16_t> trackingNumber;
    BigEndian<CharBuffer<6>, int64_t> timestamp;
    BigEndian<CharBuffer<8>, int64_t> orderId;
    BigEndian<CharBuffer<4>, int32_t> quantity;
    BigEndian<CharBuffer<8>, int64_t> matchId;
} MESSAGEND

MESSAGE CancelOrder{
    char type;
    BigEndian<CharBuffer<2>, int16_t> symbolLocate;
    BigEndian<CharBuffer<2>, int16_t> trackingNumber;
    BigEndian<CharBuffer<6>, int64_t> timestamp;
    BigEndian<CharBuffer<8>, int64_t> orderId;
    BigEndian<CharBuffer<4>, int32_t> quantity;
} MESSAGEND

MESSAGE DelOrder{
    char type;
    BigEndian<CharBuffer<2>, int16_t> symbolLocate;
    BigEndian<CharBuffer<2>, int16_t> trackingNumber;
    BigEndian<CharBuffer<6>, int64_t> timestamp;
    BigEndian<CharBuffer<8>, int64_t> orderId;
} MESSAGEND

