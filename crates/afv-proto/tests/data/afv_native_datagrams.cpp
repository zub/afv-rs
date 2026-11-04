// Seals fixed voice datagrams with afv-native's own CryptoDTO code, producing
// afv_native_datagrams.txt for tests/afv_native.rs. Built against afv-native commit fc7d216:
//
//   mkdir -p stub/nlohmann && echo 'namespace nlohmann { class json; }' > stub/nlohmann/json.hpp
//   N=path/to/afv-native SSL=$(brew --prefix openssl@3)
//   clang++ -std=c++17 -DMSGPACK_NO_BOOST -Istub -I$N/include -Ipath/to/msgpack-cxx-9.0.0/include \
//     -I$SSL/include afv_native_datagrams.cpp $N/src/cryptodto/Channel.cpp \
//     $N/src/cryptodto/SequenceTest.cpp $N/src/cryptodto/dto/Header.cpp -L$SSL/lib -lcrypto
//   ./a.out > afv_native_datagrams.txt
//
// The JSON stub is enough because ChannelConfig.h only declares its JSON converters.
// Output lines are "<label> <client|server> <hex datagram>"; client datagrams are sealed with the
// client's transmit key, server ones with the server's.
#include "afv-native/cryptodto/Channel.h"
#include "afv-native/cryptodto/dto/ChannelConfig.h"
#include "afv-native/afv/dto/voice_server/Heartbeat.h"
#include "afv-native/afv/dto/voice_server/AudioTxOnTransceivers.h"
#include "afv-native/afv/dto/voice_server/AudioRxOnTransceivers.h"
#include <cstdio>
#include <cstdarg>

using namespace afv_native;
using namespace afv_native::afv::dto;
using namespace afv_native::cryptodto;

void afv_native::__Log(const char *, int, const char *sub, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt); fprintf(stderr, "[%s] ", sub); vfprintf(stderr, fmt, ap); fprintf(stderr, "\n"); va_end(ap);
}

static Channel make(bool client) {
    dto::ChannelConfig cc;
    cc.ChannelTag = "a1b2c3d4-tag";
    std::array<unsigned char, 32> k1{}, k2{};
    for (int i = 0; i < 32; i++) { k1[i] = i; k2[i] = 0x80 + i; }
    // Client transmits with k1, receives with k2; the server the other way round.
    cc.AeadTransmitKey = client ? k1 : k2;
    cc.AeadReceiveKey  = client ? k2 : k1;
    Channel c;
    c.setChannelConfig(cc);
    return c;
}

static std::string hex(const unsigned char *p, size_t n) {
    static const char *d = "0123456789abcdef";
    std::string s;
    for (size_t i = 0; i < n; i++) { s += d[p[i] >> 4]; s += d[p[i] & 15]; }
    return s;
}

template <class T> static void emit(const char *label, bool client, uint64_t seq, const T &dto) {
    Channel c = make(client);
    std::vector<unsigned char> buf(maxPermittedDatagramSize);
    size_t n = c.Encapsulate(buf.data(), buf.size(), seq, CryptoModeChaCha20Poly1305, dto);
    printf("%s %s %s\n", label, client ? "client" : "server", hex(buf.data(), n).c_str());
}

static std::vector<unsigned char> bytes(std::initializer_list<int> l) { return std::vector<unsigned char>(l.begin(), l.end()); }

static void gen() {
    emit("hb", true, 0, Heartbeat("TEST"));

    AudioTxOnTransceivers at;
    at.Callsign = "SAS123"; at.SequenceCounter = 300; at.Audio = bytes({0xde, 0xad, 0xbe, 0xef}); at.LastPacket = false;
    at.Transceivers = {TxTransceiver(0), TxTransceiver(1)};
    emit("at", true, 1, at);

    at.SequenceCounter = 301; at.LastPacket = true; at.Transceivers = {TxTransceiver(0)};
    emit("at_last", true, 2, at);

    AudioTxOnTransceivers big;
    big.Callsign = std::string(40, 'X'); big.SequenceCounter = 70000; big.LastPacket = false;
    for (int i = 0; i < 300; i++) big.Audio.push_back(i & 0xff);
    for (int i = 0; i < 20; i++) big.Transceivers.emplace_back(i * 1000);
    emit("at_big", true, 1ULL << 40, big);

    AudioTxOnTransceivers edge;
    edge.Callsign = "ENGM_ATIS"; edge.SequenceCounter = 0xFFFFFFFFu; edge.LastPacket = true;
    emit("at_edge", true, 0xFFFFFFFFFFFFFFFEULL, edge);

    AudioRxOnTransceivers ar;
    ar.Callsign = "ENGM_TWR"; ar.SequenceCounter = 5; ar.Audio = bytes({1, 2, 3}); ar.LastPacket = false;
    RxTransceiver r1; r1.ID = 0; r1.Frequency = 122800000; r1.DistanceRatio = 0.5f;
    RxTransceiver r2; r2.ID = 3; r2.Frequency = 118300000; r2.DistanceRatio = 0.123f;
    ar.Transceivers = {r1, r2};
    emit("ar", false, 7, ar);
}

int main() {
    gen();
}
