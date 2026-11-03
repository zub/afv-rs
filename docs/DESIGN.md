# afv-rs — Design

Rust implementation of an Audio for VATSIM client. Wire-compatible with the AFV servers
(see [PROTOCOL.md](PROTOCOL.md)); everything above the wire is designed fresh.

## Goals

- Byte-exact protocol compatibility, proven by golden-vector tests.
- One model for pilot, ATC and ATIS clients instead of three separate client classes.
- Audio devices are optional: the library works on sample buffers; a `cpal` backend is a feature.
- Real-time safe audio path (no locks contended with the network, no allocation in callbacks).
- Faithful radio simulation (crackle, noise, hardware EQ, squelch click), with simultaneous VHF
  transmissions modelled as beating AM carriers instead of afv-native's fixed blocking tone.
- Async (tokio) networking; typed errors; events as a stream instead of callbacks.

## Crate layout (workspace)

```
crates/
  afv-proto   pure, no I/O: CryptoDTO framing, AEAD, replay window, voice DTOs,
              REST request/response types (serde). Deps: chacha20poly1305, serde.
  afv-audio   pure DSP, no devices, no network: Opus codec wrapper, jitter buffer,
              radio simulation (biquads, compressors, effects mixer), AGC, rate conversion
              (rubato). Example: render_double_tx.
  afv         the client: REST + UDP session (tokio, reqwest/rustls), radio model,
              transceiver sync, PTT gating, audio engine, real-time bridge, events.
              feature "cpal": device I/O backend. Examples: pilot, fake_server (with echo).
```

`afv-proto` and `afv-audio` have no async runtime dependency and are testable in isolation.

## Core model

Pilots, ATC and ATIS differ only in how radios map to transceivers, so the client has one
concept — a **radio** — and pilots/ATC are just different configurations:

```rust
pub struct RadioConfig {
    pub frequency: Frequency,        // u32 Hz; parses and displays "122.800"
    pub sites: Sites,                // Own (client position) | Fixed(Vec<Site>), e.g. from_station(..)
    pub rx: bool,
    pub tx: bool,
    pub coupling: Coupling,          // None | Sites (own group) | Across (group 0)
    pub output: Output,              // bus: Headset | Speaker, pan: Both | Left | Right
    pub gain: f32,
    pub hardware: Hardware,          // Schmid ED-137B | Rockwell Collins 2100 | Garex 220
    pub effects: bool,
    pub hf_squelch: bool,
}
```

The client owns transceiver ID assignment (dense, in radio then site order), building the
transceiver and cross-couple POSTs, and PTT gating — callers never see transceiver IDs. A radio
with `tx` but not `rx` still gets transceivers (afv-native omitted them but kept transmitting on
stale IDs). Position-only changes are stored and go out with the periodic refresh.

## Public API

```rust
let client = afv::Client::builder()
    .credentials(cid, password)
    .callsign("SAS123")
    .client_name("myclient/1.0")
    .effects(EffectSamples::load_dir(dir)?)       // optional
    .connect().await?;                            // auth + voice session; auto-reconnects

client.set_position(Position { lat_deg, lon_deg, height_msl_m, height_agl_m });
let com1 = client.add_radio(RadioConfig { tx: true, ..RadioConfig::pilot("122.800".parse()?) });
client.update_radio(com1, |r| r.frequency = "121.500".parse().unwrap());

client.set_ptt(true);              // opens once the server has acknowledged the transceivers

let mut events = client.events();  // Events::next().await
while let Some(event) = events.next().await {
    match event {
        Event::RxStarted { radio, callsign } => ...,
        Event::PttOpened => ...,
        Event::Session(SessionState::Degraded) => ...,
        ...
    }
}

let sites = client.station_transceivers("ENGM_TWR").await?;   // plain async fns
client.disconnect().await;
```

Changes are applied to local audio at once; a background task sends them to the server,
debounced by 20 ms. PTT pressed while the server's view differs (or an update is in flight)
waits for the update; while transmitting, audio goes to the last acknowledged transceiver IDs.

`Session` remains public for applications that only want the network side.

### Audio

The audio engine is decoupled from devices:

```rust
let audio = client.audio();                                    // Clone, Send + Sync
audio.capture(&mic_samples, sample_rate, channels)?;           // any buffer size or rate
audio.render(OutputBus::Headset, &mut out, sample_rate, channels)?;
```

These lock the engine and decode, mix and encode inline, so they belong on an ordinary thread.
`afv::realtime::bridge` provides that thread for any audio API: real-time callbacks get
`Capture::push` / `Playback::fill` handles that only touch lock-free ring buffers (`rtrb`) and
wake the worker. With the `cpal` feature, `afv::devices::attach(&client.audio(), DeviceConfig)`
wires up actual devices on top of it and reports device errors as events.

Threading:

- Network task (tokio) → bounded queue of `AR` packets → worker, which runs the jitter buffers,
  Opus decoding and radio simulation per output bus and fills the output rings.
- Microphone ring → worker → resample to 48 kHz → 20 ms frames → Opus → `AT` sent directly
  (non-blocking UDP). The `SequenceCounter` ticks per captured frame, as the protocol requires.
- Radio configuration reaches the engine under a mutex that only the worker and API calls take.

## Things replaced, not ported

| afv-native | afv-rs |
|---|---|
| Speex jitter buffer | Own jitter buffer keyed on `SequenceCounter` (20 ms ticks), with the same flush-after-500 ms and PLC behaviour |
| Speex preprocessor (mic) | Pluggable; default candidates: `nnnoiseless` (pure Rust) and a simple AGC |
| POCO, libcurl, OpenSSL, nlohmann, msgpack-c | tokio, reqwest+rustls, RustCrypto, serde_json, own msgpack subset |
| ChainedCallback / EventBus | `Events` stream of `Event` |
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
