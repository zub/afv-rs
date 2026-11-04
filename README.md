# afv-rs

A Rust client library for [Audio for VATSIM](https://vatsim.net) (AFV), the voice network used by
VATSIM pilots and controllers. *This is an experiment, and is in no way guaranteed to work*. Its main motivation is to learn a new programming language, learn new development methods as well as put old signal processing know-how to good use. :)

*afv-rs is not API-compatible with afv-native*, and the API has been designed to be more ergonomic where possible.

Although it started as a port, large parts of the audio pipeline have been ripped out, and are no longer similar. Even though afv-rs does the same job, it will not be bit-for-bit identical (which is a non-goal). In particular, the radios are simulated through higher order butterworth and chebyshev filters, instead of simply biquadratic shelves.

An added feature is that afv-rs provides a quite realistic implementation of HF transmission, including atmospherics and distortion commonly encountered at polar latitudes, a feature lacking in previous implementations. There is also a separate compressor algorithm that is more similar to the ones frequently in use in HF transmissions, where the SNR is quite a bit lower than with VHF transmissions. HF is based on the Collins HFS-900 transceiver, frequently in use on B737 and A330 aircraft (amongst others). There are others, but this is the one I got hold of proper datasheets for. :D

> **Status:** pre-release. The protocol is checked against golden datagrams sealed by
> afv-native's own code, and everything runs end to end against an in-process fake server, but
> afv-rs has not yet been tried on the live network. Server behaviour that has not been confirmed
> by captured traffic is marked **[Unverified]** in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Crates

Most applications only need `afv`. The other two have no async runtime and are usable on their
own, for example in a test server or an offline renderer.

## Using it

Pilots, controllers and ATIS differ only in how radios map to antennas, so there is one concept,
the radio. A pilot's radios sit at the aircraft's position:

```rust
use afv::devices::{self, DeviceConfig};
use afv::radio::{Position, RadioConfig};
use afv::{Client, Event};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().credentials("1234567", "password").callsign("SAS123").connect().await?;
    client.set_position(Position { lat_deg: 60.19, lon_deg: 11.1, height_msl_m: 200.0, height_agl_m: 0.0 });
    client.add_radio(RadioConfig { tx: true, ..RadioConfig::pilot("122.800".parse()?) });

    // Default microphone and output device (feature `cpal`).
    let _devices = devices::attach(&client.audio(), DeviceConfig::defaults())?;

    let mut events = client.events();
    while let Some(event) = events.next().await {
        match event {
            Event::RxStarted { callsign, .. } => println!("{callsign} transmitting"),
            Event::PttOpened => println!("on air"),
            other => println!("{other:?}"),
        }
    }
    Ok(())
}
```

`client.set_ptt(true)` keys up. Transmission starts once the server knows the current
frequencies, which `Event::PttOpened` reports.

A controller's radios use a station's antenna sites instead:

```rust
use afv::radio::{RadioConfig, Sites};

let sites = Sites::from_station(&client.session().station_transceivers("ENGM_TWR").await?);
client.add_radio(RadioConfig { tx: true, ..RadioConfig::atc("118.300".parse()?, sites) });
```

### Bringing your own audio

Without the `cpal` feature, `afv` works on sample buffers in any format. `afv::realtime::bridge`
gives your audio API's real-time callbacks handles that only move samples through lock-free
rings; a worker thread does the decoding, mixing, encoding and sending. The `atc` example shows
this with a stand-in audio API whose devices are WAV files.

### Radio effects

The simulation's recorded sounds (squelch click, crackle, noise, AC bus hum) are not part of the
library. Pass them to `ClientBuilder::effects`, or point the examples at a directory with `--effects`;
they read afv-native's `Click_f32.wav`, `Crackle_f32.wav`, `AC_Bus_f32.wav`, `WhiteNoise_f32.wav`
and `HF_WhiteNoise_f32.wav`. Without them, radios still play voice through their radio set.

## Trying it locally

The fake server is the one the tests use. It plays every transmission back as station `ECHO`
half a second after it ends:

```bash
cargo run -p afv --example fake_server
```

Then, in another terminal, with the URL it prints:

```bash
cargo run -p afv --features cpal --example pilot -- --api-url http://127.0.0.1:PORT
```

Press Enter to toggle PTT, type a frequency such as `121.500` to retune, or `q` to quit. With
`AFV_CID` and `AFV_PASSWORD` set and no `--api-url`, the same example connects to the live
network; see the top of [`pilot.rs`](crates/afv/examples/pilot.rs) for its options.

The other examples:

- [`atc`](crates/afv/examples/atc.rs): a controller with its own audio devices, recording headset
  and loudspeaker to WAV files.
- [`render_double_tx`](crates/afv-audio/examples/render_double_tx.rs): renders two recordings as
  single and double transmissions, then under each HF condition preset and VHF radio set.

## Building

- Rust 1.95 or newer (edition 2024).
- CMake and a C compiler, to build libopus. The pinned CMake version is in `mise.toml`
  (`mise install`).
- Linux with the `cpal` feature: ALSA development files (`libasound2-dev` on Debian and Ubuntu).

```bash
cargo test --workspace
```

The decoders and the receive path have fuzz targets in [`fuzz/`](fuzz), run with nightly Rust and
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz):

```bash
cargo +nightly fuzz run plaintext
```

## Documentation

- [docs/PROTOCOL.md](docs/PROTOCOL.md): the AFV wire protocol and REST API, as reconstructed from
  afv-native.
- [docs/DESIGN.md](docs/DESIGN.md): crate layout, the radio model, threading and the receiver chain.
- [docs/HF.md](docs/HF.md): the HF propagation model and its sources.

## Licence

MIT; see [LICENSE](LICENSE). Parts of `afv-audio` are derived from afv-native (BSD-3-Clause) and
sndfilter (MIT), so that crate is `MIT AND BSD-3-Clause`; their notices are in
[THIRD_PARTY.md](THIRD_PARTY.md) and must accompany redistributions.
