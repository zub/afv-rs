# afv-rs — Design

Rust implementation of an Audio for VATSIM client. Wire-compatible with the AFV servers
(see [PROTOCOL.md](PROTOCOL.md)); everything above the wire is designed fresh.

## Goals

- Byte-exact protocol compatibility, proven by golden-vector tests.
- One model for pilot, ATC and ATIS clients instead of three separate client classes.
- Audio devices are optional: the library works on sample buffers; a `cpal` backend is a feature.
- Real-time safe audio path (no locks contended with the network, no allocation in callbacks).
- Faithful radio simulation (crackle, noise, hardware EQ, blocking tone, squelch click).
- Async (tokio) networking; typed errors; events as a stream instead of callbacks.

## Crate layout (workspace)

```
crates/
  afv-proto   pure, no I/O: CryptoDTO framing, AEAD, replay window, voice DTOs,
              REST request/response types (serde). Deps: chacha20poly1305, rmp, serde.
  afv-audio   pure DSP, no devices, no network: Opus codec wrapper, jitter buffer,
              radio simulation (biquads, compressors, effects mixer), AGC, mic chain.
  afv         the client: REST + UDP session (tokio, reqwest/rustls), radio model,
              transceiver sync, PTT gating, audio engine glue, events.
              feature "cpal": device I/O backend.
examples/     pilot CLI, ATC CLI, fake voice server for local testing.
```

`afv-proto` and `afv-audio` have no async runtime dependency and are testable in isolation.

## Core model

Pilots, ATC and ATIS differ only in how radios map to transceivers, so the client has one
concept — a **radio** — and pilots/ATC are just different configurations:

```rust
pub struct RadioConfig {
    pub frequency: Frequency,        // newtype over u32 Hz, parse/display "122.800"
    pub sites: Sites,                // Sites::Own (client position) | Sites::Fixed(Vec<Site>)
    pub rx: bool,
    pub tx: bool,
    pub coupling: Coupling,          // None | Sites (xc, own group) | Across (group 0)
    pub output: Output,              // which output bus: Headset/Speaker, Left/Right/Both
    pub gain: f32,
    pub hardware: Hardware,          // Schmid ED-137B | Rockwell Collins 2100 | Garex 220
    pub effects: bool,
}
```

The client owns transceiver ID assignment, building the transceiver and cross-couple POSTs,
periodic refresh, and PTT gating — callers never see transceiver IDs.

## Public API sketch

```rust
let client = afv::Client::builder()
    .credentials(cid, password)
    .callsign("SAS123")
    .client_name("myclient/1.0")
    .build()?;

let session = client.connect().await?;          // auth + voice session; auto-reconnects

session.set_position(Position { lat, lon, alt_msl_m, alt_agl_m });
let com1 = session.add_radio(RadioConfig::pilot(Frequency::mhz(122.800)));
let com2 = session.add_radio(RadioConfig::pilot(Frequency::mhz(121.500)));
session.radio(com1).set_tx(true);

session.set_ptt(true);                           // opens once transceivers are synced

let mut events = session.events();               // impl Stream<Item = Event>
while let Some(ev) = events.next().await {
    match ev {
        Event::RxStarted { radio, callsign } => ...,
        Event::PttOpened => ...,
        Event::ConnectionDegraded => ...,
        ...
    }
}

// ATC: station data
let sites = session.station_transceivers("ENGM_TWR").await?;   // plain async fns, not callbacks
let vccs  = session.vccs("ENGM_TWR").await?;

session.disconnect().await?;
```

Changes made via `session.radio(id)` are applied to local state at once and pushed to the
server, debounced, by a background task.

### Audio

The audio engine is decoupled from devices:

```rust
let audio = session.audio();                       // cheap handle, Send + Sync
audio.capture(&mic_samples, sample_rate);          // from any mic source, any buffer size
audio.render(Bus::Headset, &mut out, sample_rate, channels);  // pull for an output
```

Internally this reframes to 48 kHz / 960-sample frames (resampling when needed). With the `cpal`
feature, `afv::devices::attach(&session, DeviceConfig { ... })` does the wiring and reports
device errors as events.

Threading:

- Network task (tokio) → per-callsign packet queue (lock-free SPSC) → render callback, which
  runs the jitter buffer, Opus decoding and radio simulation.
- Capture callback → mic chain → Opus encode → SPSC → network task sends `AT`. The 20 ms
  `SequenceCounter` ticks in the capture path, as the protocol requires.
- Radio configuration reaches the audio thread through a lock-free snapshot (e.g. `arc-swap`).

## Things replaced, not ported

| afv-native | afv-rs |
|---|---|
| Speex jitter buffer | Own jitter buffer keyed on `SequenceCounter` (20 ms ticks), with the same flush-after-500 ms and PLC behaviour |
| Speex preprocessor (mic) | Pluggable; default candidates: `nnnoiseless` (pure Rust) and a simple AGC |
| POCO, libcurl, OpenSSL, nlohmann, msgpack-c | tokio, reqwest+rustls, RustCrypto, serde_json, rmp |
| ChainedCallback / EventBus | `Stream` of `Event` |
| Effect WAVs loaded from a path | Caller supplies them (`EffectSamples`); white noise can be synthesised |

The small DSP blocks (biquads, ChunkWare SimpleComp/SimpleLimit, sndfilter compressor) are ported
directly. They are MIT (sndfilter, ChunkWare) and BSD-3 (afv-native); keep their notices in
`THIRD_PARTY.md`.

## Milestones

1. **afv-proto** — framing, AEAD, replay window, DTOs, REST types; golden vectors from an
   independent encoder.
2. **Session** — auth/JWT refresh, voice session, heartbeat/degraded/timeout state machine,
   reconnect; tested against an in-process fake voice server built on `afv-proto`.
3. **afv-audio** — Opus, jitter buffer, radio simulation; offline tests that render WAVs for listening.
4. **Client facade** — radio model, transceiver sync, PTT gating, events; `cpal` backend; examples.
5. **Live test** against the VATSIM servers (needs real credentials; manual).
