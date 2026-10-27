# Audio for VATSIM (AFV) — Client Protocol Description

Derived from reading the `afv-native` C++ implementation (`/Users/buz/Development/afv-native`,
commit `fc7d216`). This describes what goes over the wire and the behaviour a client must exhibit
to interoperate with the AFV servers. The C++ API surface is intentionally ignored.

Where the C++ code has a quirk or bug that affects the wire, it is called out in a
**Quirk** note so the Rust implementation can decide deliberately whether to replicate it.

---

## 1. Overview

AFV has two planes:

| Plane | Transport | Purpose |
|---|---|---|
| Control | HTTPS + JSON (REST) | Authentication, voice-session allocation, transceiver/position updates, cross-coupling, station database lookups |
| Voice | UDP, "CryptoDTO" framing, ChaCha20-Poly1305, MessagePack payloads | Heartbeats, Opus-encoded voice (TX and RX) |

The control plane hands the client the UDP endpoint **and the symmetric keys** for the voice plane.
There is no handshake on UDP: the first datagram the client sends is already encrypted with the key
received over HTTPS.

The server is the authority on *who hears what*. The client tells the server where its transceivers
are and what frequency each is on; the server does the propagation/range calculation and forwards
voice to every client with a transceiver in range on the same frequency, annotated with a signal
strength (`DistanceRatio`). The client only does local audio presentation (mixing, radio effects).

Default API base URL: `https://voice1.vatsim.net`.

```
 Client                               API server (HTTPS)                 Voice server (UDP)
   | POST /api/v1/auth  ------------------>|                                     |
   |<----------------------- JWT (text) ---|                                     |
   | POST /api/v1/users/{u}/callsigns/{cs}>|                                     |
   |<-- {voiceServer:{addr, keys, tag}} ---|                                     |
   | POST .../transceivers  -------------->|                                     |
   |                                                                             |
   | "H"  heartbeat  every 3 s --------------------------------------------------->|
   |<---------------------------------------------------------------- "HA" ack ---|
   | "AT" audio tx (Opus, 20 ms frames) ------------------------------------------>|
   |<------------------------------------------------- "AR" audio rx (Opus) ------|
   | DELETE /api/v1/users/{u}/callsigns/{cs} ->|                                 |
```

---

## 2. Control plane (REST)

### 2.1 Common HTTP conventions

- All requests to `{baseUrl}/api/v1/...`.
- JSON bodies are sent with `Content-Type: application/json; charset=UTF-8`.
- `User-Agent: AFV-Native/1.0` (the official clients presumably send their own; probably not checked).
- Authenticated requests carry `Authorization: Bearer <jwt>`.
- Redirects followed only for GET.
- Timeouts used by the C++ client: auth = 5 s connect / 15 s total; everything else 10 s / 30 s.
- Path segments (`username`, `callsign`, station names) are inserted **without URL-encoding**.
  Callsigns are `[A-Z0-9_-]` in practice, so this is harmless, but a Rust client should encode.
- JSON key casing is inconsistent between endpoints (see each DTO). The server is ASP.NET and
  almost certainly deserialises case-insensitively, but replicate the casing below to be safe.

### 2.2 Authentication — `POST /api/v1/auth`

Request body:

```json
{ "username": "1234567", "password": "secret", "client": "AFV-Native" }
```

- `username`: VATSIM CID (as a string).
- `client`: free-form client name string (identifies the client software).

Response:

- `200` — body is the **raw JWT as plain text** (not JSON, not quoted).
- `400` — bad request / client incompatible.
- `401` — bad password.
- `403` — credentials rejected (e.g. suspended).
- anything else — generic failure.

Token handling:

- The JWT is decoded **without signature verification**, only to read the `exp` claim (Unix seconds).
- If `exp - now <= 60` → treat as error ("check your system clock").
- Re-authenticate (repeat the same POST) at `exp - 60 s`. If there is no `exp` claim, re-auth after 59 min.
- Re-authentication does **not** tear down the voice session; the new token is simply used for
  subsequent REST calls.

> There is a constant `NetworkVersion = "3a5ddc6d-cf5d-4319-bd0e-d184f772db80"` in `afv/params.h`
> but it is **never sent** anywhere. It is a leftover from the original .NET client.

### 2.3 Allocate voice session — `POST /api/v1/users/{username}/callsigns/{callsign}`

Authenticated. **Empty body.** Response `200`:

```json
{
  "voiceServer": {
    "addressIpV4": "1.2.3.4:50000",
    "addressIpV6": "...",
    "channelConfig": {
      "channelTag": "<string>",
      "aeadReceiveKey": "<base64, 32 bytes>",
      "aeadTransmitKey": "<base64, 32 bytes>",
      "hmacKey": null
    }
  }
}
```

- `addressIpV4` is `host:port`; the client opens a UDP socket to it. `addressIpV6` is ignored by
  the C++ client (always uses v4).
- Keys are named **from the client's perspective**:
  - `aeadTransmitKey` — client encrypts outgoing datagrams with this.
  - `aeadReceiveKey` — client decrypts incoming datagrams with this.
- `channelTag` — opaque string; placed in every outgoing header and must match on every incoming header.
- `hmacKey` — unused (always null).
- A new session = new keys and tag, so nonces (derived from the TX sequence) need not be reset.

Any non-200 or unparsable body → voice session error.

### 2.4 Release voice session — `DELETE /api/v1/users/{username}/callsigns/{callsign}`

Authenticated, no body, expect `200`. Sent on deliberate disconnect and before an automatic
reconnect after a heartbeat timeout.

### 2.5 Transceiver update — `POST /api/v1/users/{username}/callsigns/{callsign}/transceivers`

Authenticated. Body is a JSON array of transceivers:

```json
[
  { "ID": 0, "Frequency": 122800000, "LatDeg": 59.9, "LonDeg": 10.7,
    "HeightMslM": 120.0, "HeightAglM": 0.0, "rco": false }
]
```

| Field | Type | Meaning |
|---|---|---|
| `ID` | u16 | Client-chosen transceiver id, referenced from `AT` packets and cross-couple groups. |
| `Frequency` | u32 | Hz (e.g. 122.800 MHz = `122800000`). |
| `LatDeg`, `LonDeg` | f64 | WGS84 degrees. |
| `HeightMslM` | f64 | Antenna height above mean sea level, metres. |
| `HeightAglM` | f64 | Antenna height above ground, metres (drives radio horizon). |
| `rco` | bool | Remote Controlled Outlet flag (station transceivers only). Note lowercase key. |

Rules / behaviour:

- Transceivers with `Frequency == 0` are filtered out before sending.
- The array **replaces** the full transceiver set for this callsign.
- Sent immediately when the set changes, and periodically as a keep-alive / position update:
  every **20 s** for pilots, **60 s** for ATC and ATIS.
- A `200` response means the server now knows the new frequencies. Clients gate PTT on this (§5.1).

### 2.6 Cross-couple groups (ATC) — `POST .../callsigns/{callsign}/crossCoupleGroups`

Authenticated. Body:

```json
[
  { "ID": 0, "TransceiverIDs": [3, 4, 5] },
  { "ID": 1, "TransceiverIDs": [0, 1] },
  { "ID": 2, "TransceiverIDs": [2] }
]
```

Semantics (server-side): audio received on any transceiver in a group is re-transmitted by the
server on all other transceivers in the same group ("XC"/frequency coupling, so pilots on different
frequencies hear each other).

How the ATC client builds it:

- Group `0` is **always** present (possibly empty) and holds the transceivers of every frequency
  with "cross-couple across" enabled — i.e. all those frequencies are coupled together.
- Each frequency with plain `xc` enabled gets its **own** group (`ID` 1, 2, …) containing only
  that frequency's transceivers — this couples the multiple sites of one frequency together.
- Only frequencies with both `tx` and `rx` enabled and at least one transceiver qualify.
- Sent together with every transceiver update (immediately after it, without waiting for its response).

### 2.7 Station database (ATC tooling)

All authenticated GETs, all optional for a pilot client.

| Endpoint | Response |
|---|---|
| `GET /api/v1/stations/aliased` | `[Station]` — stations that have a frequency alias (e.g. "fake" 199.998 frequencies mapped to real ones). |
| `GET /api/v1/stations/byName/{name}` | `Station` object, or `404` if not found. |
| `GET /api/v1/stations/byName/{name}/transceivers/allDistinctObeyExclusions` | `[StationTransceiver]` — physical antenna sites for that station. |
| `GET /api/v1/stations/byName/{name}/vccsStations` | `[Station]` — the station's VCCS (voice comms) panel list. **Array order is meaningful** (panel order). |

`Station`:

```json
{ "id": "...", "name": "ENGM_TWR", "frequency": 118300000, "frequencyAlias": 199998000 }
```

`frequency` / `frequencyAlias` may be absent or non-integer (treat as 0 / none).

`StationTransceiver`:

```json
{ "id": "...", "name": "...", "latDeg": 60.19, "lonDeg": 11.10,
  "heightMslM": 210.0, "heightAglM": 25.0, "rco": false }
```

(`rco` optional, default false.) The ATC client converts these into `Transceiver` entries on the
station's frequency (assigning its own `ID`s), so a controller transmits/receives from the real
antenna sites rather than their own position.

`aliased` is fetched automatically once the API session is up.

---

## 3. Voice plane — UDP CryptoDTO framing

### 3.1 Socket

- One UDP socket per voice session, bound to an ephemeral local port, `connect()`ed to `addressIpV4`.
- Datagrams up to 64 KiB are accepted; in practice they are small (<300 bytes).
- No fragmentation/reassembly layer; one DTO per datagram.

### 3.2 Datagram layout

All multi-byte length fields are **little-endian u16** (the C++ code `memcpy`s host order; every
supported platform is LE).

```
offset  size        field
0       2           header_len (u16 LE)
2       header_len  header  — MessagePack array (see 3.3)
2+hl    N           body    — plaintext (mode 1) or ciphertext||tag (mode 2)
```

### 3.3 Header

MessagePack **array** (not map) of exactly 3 elements:

```
[ ChannelTag: str, Sequence: uint (u64), Mode: int ]
```

- `ChannelTag` — the tag from `channelConfig`. Incoming datagrams with a different tag are dropped.
- `Sequence` — per-direction 64-bit packet counter (see 3.6).
- `Mode`:
  - `0` undefined (invalid)
  - `1` None — body is plaintext. **Rejected on receive** by the client; never sent.
  - `2` ChaCha20-Poly1305 — the only mode used.

msgpack-c encodes integers in the smallest form (e.g. sequence 0 → `0x00`, mode 2 → `0x02`).
A decoder must accept any integer width.

### 3.4 Encryption (mode 2)

Standard IETF ChaCha20-Poly1305 (RFC 8439), 256-bit key, 96-bit nonce, 128-bit tag.

- **Key**: `aeadTransmitKey` for sending, `aeadReceiveKey` for receiving.
- **Nonce** (12 bytes): `00 00 00 00` followed by `Sequence` as **u64 little-endian**.
- **AAD**: the first `2 + header_len` bytes of the datagram, i.e. the length prefix *and* the
  msgpack header. Header tampering therefore fails authentication.
- **Body on the wire**: `ciphertext || tag(16)`; ciphertext length == plaintext length.
- Receive rejects bodies of ≤ 16 bytes.

### 3.5 Plaintext payload (inside the AEAD)

```
offset  size       field
0       2          name_len (u16 LE)
2       name_len   dto_name  — ASCII, e.g. "H", "HA", "AT", "AR"
..      2          dto_len (u16 LE)
..      dto_len    dto       — MessagePack-encoded DTO
```

Receiver checks `dto_len == remaining bytes`; mismatch → drop. DTOs > 65535 bytes cannot be sent.

### 3.6 Sequence numbers & replay protection

- **TX**: a u64 counter starting at 0, incremented by one for every datagram sent (heartbeats and
  audio share the counter).
  **Quirk:** the C++ client never resets it across reconnects of the same client object. Harmless
  because each session has fresh keys; the server evidently does not require starting at 0.
- **RX**: a sliding window replay filter with a 64-entry bitmap.
  - `seq < min` → drop (too old / duplicate).
  - `seq == min` → accept, advance `min` past all consecutively-received sequences.
  - `min < seq <= min + 64` → accept unless its bit is already set (duplicate).
  - `seq > min + 64` → accept ("overflow"), slide the window forward; if the jump exceeds the
    window, reset it to start just after `seq`.
  - Window resets when the channel is closed or the receive key changes.
  - Replay check runs only *after* successful decryption.

### 3.7 Worked example — heartbeat

Channel tag `"abc"`, sequence 0, callsign `"TEST"`:

```
header msgpack : 93 a3 61 62 63 00 02            (7 bytes)
plaintext      : 01 00  48  06 00  91 a4 54 45 53 54
                 |name=1| H |len=6| ["TEST"]
datagram       : 07 00 | 93 a3 61 62 63 00 02 | <11 bytes ciphertext> <16 bytes tag>
nonce          : 00 00 00 00 00 00 00 00 00 00 00 00
AAD            : 07 00 93 a3 61 62 63 00 02
```

---

## 4. Voice-plane DTOs

All DTOs are MessagePack **arrays** with positional fields (no field names on the wire).

### 4.1 `H` — Heartbeat (client → server)

```
[ Callsign: str ]
```

### 4.2 `HA` — Heartbeat ack (server → client)

Body content is ignored by the client (can be empty, `dto_len` may be 0). Its arrival is the signal.

### 4.3 `AT` — Audio TX on transceivers (client → server)

```
[ Callsign: str,
  SequenceCounter: uint (u32),
  Audio: bin,                     // one Opus packet (20 ms)
  LastPacket: bool,
  Transceivers: [ [ID: uint (u16)], ... ] ]   // array of 1-element arrays
```

- `Transceivers` lists the transceiver IDs (from the last transceiver update) to transmit on.
  Pilots: the single selected radio. ATC: every transceiver of every frequency with TX enabled.
- `Audio` is encoded as msgpack `bin` (msgpack-c packs `std::vector<unsigned char>` as bin).

### 4.4 `AR` — Audio RX on transceivers (server → client)

```
[ Callsign: str,                  // who is talking
  SequenceCounter: uint (u32),
  Audio: bin,                     // one Opus packet (decoder should also accept str)
  LastPacket: bool,
  Transceivers: [ [ID: u16, Frequency: u32 Hz, DistanceRatio: f32], ... ] ]
```

- `Transceivers` lists **the receiving client's** transceivers that pick up this transmission.
  `ID` is the receiver's own transceiver id; `Frequency` is that transceiver's frequency.
- `DistanceRatio` ∈ (0, 1]: signal strength. 1.0 = close, ~0.1 = edge of range. Used only for
  local effects (static/crackle amount).
- Accept float32 or float64 (and ints) for `DistanceRatio`.

### 4.5 Unused / legacy

The C++ tree contains an `AudioOnDirect` shape (`[Callsign, SequenceCounter, Audio, LastPacket, ToCallsign]`)
and an `IAudioVoiceRoom` (`VoiceRoom: str`) mixin, neither with a DTO name nor used. They are
remnants of the original .NET client's direct-call / voice-room features. Unknown DTO names
received from the server are logged and ignored.

---

## 5. Voice semantics

### 5.1 Audio format & transmit cadence

| Parameter | Value |
|---|---|
| Codec | Opus, `OPUS_APPLICATION_VOIP`, mono |
| Sample rate | 48 000 Hz |
| Frame | 20 ms = 960 samples |
| Bitrate | 16 384 bit/s (`OPUS_SET_BITRATE`) |
| Max encoded frame buffer | 102 400 bytes (sizing only; actual ≈ 40 bytes) |

Transmit procedure (driven by the microphone capture clock, one call per 20 ms frame):

1. Every captured frame increments `SequenceCounter` (u32) **whether or not PTT is pressed**.
   The counter is therefore a 20 ms wall-clock tick, which the receiver uses as the jitter-buffer
   timestamp. (Reset to 0 only on a full client reset.)
2. While PTT is pressed: encode the frame, send `AT` with `LastPacket = false`.
3. On the first frame after PTT release: encode and send one final `AT` with `LastPacket = true`.
4. Afterwards send nothing (heartbeats continue).

Optional mic chain before encoding: Speex preprocessor (denoise, dereverb, AGC target 30000,
max gain 12 dB, decrement −60, noise suppress −30 dB), then mic gain, hard clip to ±1.

**PTT gating on transceiver sync**: when PTT is requested, if the client's desired frequencies
differ from what the server last acknowledged (200 on transceiver POST), or a transceiver POST is
in flight, PTT is deferred: a transceiver update is sent immediately and PTT opens when it
completes. This avoids transmitting on the old frequency. (Pilot client compares frequencies;
ATC client only waits for in-flight updates.)

### 5.2 Receive procedure

For each `AR` packet:

1. Key the stream by `Callsign`. Each remote callsign gets its own Opus decoder + jitter buffer.
2. Replace the callsign's "current transceivers" with the packet's `Transceivers` list.
3. If > 500 ms since that callsign's previous packet: flush its jitter buffer and reset its Opus
   decoder (new transmission).
4. Insert the Opus packet into the jitter buffer with `timestamp = SequenceCounter`, `span = 1`.
5. If `LastPacket`, remember its `SequenceCounter` as the end of the transmission.

Playback (per 20 ms output frame, per active callsign):

- Pull from the jitter buffer:
  - packet available → decode;
  - missing → if ending and past the end sequence, stream closed; else Opus PLC (`decode(NULL)`);
- After 10 consecutive empty frames the stream is considered inactive.
- Streams with no activity for 60 s are purged.
- Optional per-stream RMS AGC (target −18 dBFS, ±12 dB, attack 5 ms, release 150 ms).

Routing to local radios: a stream is heard on a local radio/frequency if any entry in its current
`Transceivers` list has `Frequency == radio.frequency`. The ATC client uses the **max**
`DistanceRatio` among matching entries.

The ATC client additionally drops `AR` packets unless at least one listed frequency is an added
frequency with RX enabled (and tracks per-frequency "who is talking" from `LastPacket`, with a
2 s voice timeout).

### 5.3 Client-side radio presentation (not wire protocol)

Not needed for interop, but part of what users expect. Per radio, when ≥ 1 stream is received and
effects are enabled:

- `crackle = clamp(exp(d) * d^-4 / 350 − 0.00776652, 0, 0.20)` with `d = DistanceRatio`.
- VHF (≥ 30 MHz): voice gain `1 − 3.7·crackle`, crackle loop gain `2·crackle`, VHF white noise 0.17,
  AC bus hum 0.005.
- HF (< 30 MHz): voice gain 0.20, HF white noise 0.16 (0 if HF squelch), AC bus 0.001, no crackle.
- Mix → clip → band-pass "hardware" EQ (Schmid ED-137B / Rockwell Collins 2100 / Garex 220
  biquad presets) → compressor.
- ≥ 2 simultaneous streams on one radio → 180 Hz "blocking" sine at gain 0.25.
- End of reception → "click" sample at gain 1.3.
- A radio that is currently transmitting does not output received audio.
- Effect samples: `Click_f32.wav`, `Crackle_f32.wav`, `AC_Bus_f32.wav`, `WhiteNoise_f32.wav`,
  `HF_WhiteNoise_f32.wav` (48 kHz float32 mono).

---

## 6. Session lifecycle & timers

### 6.1 Connect sequence

1. `POST /api/v1/auth` → JWT. Schedule re-auth.
2. `POST /api/v1/users/{u}/callsigns/{cs}` → voice server + keys.
3. Open UDP socket, install keys/tag, start heartbeat timer.
4. Voice "connected": start audio, immediately `POST .../transceivers`
   (and `.../crossCoupleGroups` for ATC). Also `GET /stations/aliased`.
5. Steady state: heartbeats every 3 s, transceiver refresh every 20 s (pilot) / 60 s (ATC).

The callsign cannot be changed while the voice session is open.

### 6.2 Heartbeats & liveness

| Constant | Value |
|---|---|
| Heartbeat interval | 3 000 ms |
| Silence timeout (no valid packet of any kind) | 20 000 ms |
| Consecutive unacked heartbeats → degraded | 5 |
| Consecutive unacked heartbeats → timeout | 10 |

- Every 3 s send `H`; increment "missed" counter.
- `HA` received → missed = 0; leave degraded state if in it.
- **Any** valid decrypted packet (HA or AR) restarts the 20 s silence timer.
- Effective behaviour: disconnect after 20 s of total silence, or after 10 heartbeats (~30 s) without
  an `HA` even if audio is flowing.
- On timeout: `DELETE` the callsign, close UDP, then re-run the whole connect sequence (re-auth included).

### 6.3 UDP error handling

ICMP-derived socket errors (`ECONNRESET`, `ECONNREFUSED`, `ENOTCONN`, `ECONNABORTED`) are counted;
3 within 10 s is fatal → disconnect + reconnect. Other errors are reported but non-fatal.
Malformed / undecryptable / wrong-tag / replayed datagrams are silently dropped.

### 6.4 Disconnect

`DELETE` the callsign (fire-and-forget), stop timers, close socket, drop the bearer token.

---

## 7. Client profiles

| | Pilot | ATC | ATIS |
|---|---|---|---|
| Transceivers | One per radio (`ID` = radio index 0..n-1), all at aircraft position. | Per added frequency: either the station's real antenna sites (from the station DB) or one at controller position. IDs assigned sequentially 0.. over all RX-enabled frequencies at each update. | One transceiver, `ID` 0. |
| `AT` transceivers | `[selected TX radio]` | All transceivers of TX-enabled frequencies | `[0]` |
| Cross-coupling | — | Yes (§2.6) | — |
| Update interval | 20 s | 60 s | 60 s |

**ATC quirks (C++):**
- Transceiver IDs are only (re)assigned when a transceiver DTO is built; a frequency with `tx` on
  but `rx` off is omitted from the transceiver POST, yet its stale IDs may still be put in `AT`.
- ATIS `AT` packets never set `LastPacket` (uninitialised bool) and loop pre-encoded frames forever.

---

## 8. Error surface (what an application needs to see)

- API: connection error, bad request / incompatible client (400), bad password (401),
  rejected credentials (403), other HTTP error, invalid token, token expiry in the past (clock skew).
- Voice: UDP channel error (with errno), bad response from API server on session allocation,
  heartbeat timeout; plus a *degraded* (warning) and *resumed* state.
- Events useful to UIs: PTT opened/closed (after sync gating), RX begin/end per frequency and per
  callsign (ATC), station data/VCCS/transceivers received.

---

## 9. Notes for the Rust implementation

These are observations, not requirements:

- The wire protocol is small: ~4 REST calls, 4 UDP DTOs, one AEAD construction. It can be a
  pure, I/O-free, fully unit-testable `proto` module (framing + DTO codecs + replay window),
  with a separate async session layer (HTTP + UDP + timers) and a separate audio/radio layer.
  Nothing in the protocol requires threads beyond an async runtime plus the audio callback.
- The C++ client couples TX sequencing to the audio capture thread. In Rust the encoder can run
  on a dedicated task fed by a lock-free ring from the audio callback; the `SequenceCounter` only
  needs to tick once per 20 ms frame.
- Candidate crates: `chacha20poly1305`, `rmp`/`rmp-serde` (careful: DTOs are positional arrays;
  `Audio` must be `serde_bytes`), `reqwest`, `tokio`, `opus` / `audiopus`, `cpal`, `jsonwebtoken`
  or plain base64 + `serde_json` for reading `exp`.
- Golden-vector tests: capture a few real datagrams + keys from the C++ client (or the server) and
  assert byte-exact encode/decode.
- Things to verify against a live server before relying on them:
  - Whether the server accepts `Audio` as msgpack `str` as well as `bin` (we should send `bin`).
  - Whether the TX `Sequence` must be monotonically increasing only, or start at 0.
  - Exact `HA` body content (currently ignored).
  - Whether `addressIpV6` is ever populated / usable.
  - Whether JSON key casing matters on `/transceivers` and `/crossCoupleGroups`.
