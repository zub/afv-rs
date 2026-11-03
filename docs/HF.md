# How HF radio is simulated

Aviation HF voice sounds very different from VHF. This page explains, in plain terms, what causes
that and how afv-rs imitates it. None of this is in afv-native, which plays HF like VHF apart from
a noise recording; the AFV servers only tell the client *who* is transmitting on *which*
frequency, so everything below happens in the receiving client.

The code is in `crates/afv-audio/src/dsp/hf.rs`; the strength of every effect is set by
`HfConditions` (`client.audio().set_hf_conditions(...)`).

## 1. Mistuning: the "Donald Duck" voice

Aviation HF uses **single sideband** (upper sideband, USB). An AM signal carries a strong carrier
that the receiver can lock onto; SSB leaves the carrier out to save power, so the receiver has to
re-create it on its own. If the receiver's idea of the frequency is off from the transmitter's by
Δf, **every tone in the voice moves by Δf hertz**.

That is not the same as speeding up a recording. Speeding up multiplies every frequency by the
same factor, which keeps the harmonics of the voice in tune with each other. A fixed shift adds the
same number of hertz to each harmonic, so they no longer line up: shifted up the voice turns
squeaky and thin, shifted down it growls. A few tens of hertz already sound odd; hundreds of hertz
make speech hard to follow.

**Model.** Each station has its own fixed tuning error, drawn once from its callsign, anywhere
within ±`offset_hz`. On top, the error drifts slowly by up to ±`drift_hz`, heading for a new random
value every 2–6 seconds. The shift itself is done exactly: a Hilbert filter pair turns the audio
into an "analytic" signal, which is then rotated by Δf.

## 2. Multipath fading: the watery, warbling sound

HF reaches far by bouncing off the ionosphere, and usually by more than one route at once, for
example one and two hops. The copies arrive a few milliseconds apart. Where they meet out of
phase they cancel. With a delay τ between them, that happens at frequencies 1/τ apart, so a 1 ms
delay puts a notch every 1000 Hz. The ionosphere is always moving, so the strength and phase of
each path keep changing: the notches sweep through the voice, the overall level swells and sinks,
and the changing phases make the pitch waver.

**Model.** This is the standard **Watterson model** (Watterson, Juroshek and Bensema, 1970),
adopted by CCIR Recommendation 520 and ITU-R F.1487 for testing HF modems. It has:

- **Two paths**, the second delayed by `multipath_delay_ms`, sharing the power equally.
- **A random complex gain on each path**, changing in level and phase. Its spectrum is a Gaussian
  as wide as the **Doppler spread**, `fading_rate_hz`: 0.1 Hz means slow swells over tens of
  seconds, 1 Hz a restless signal, 10 Hz the rapid "flutter" heard on polar routes.
- **`fading_depth`**: 1 means pure scatter (Rayleigh fading, with occasional deep nulls); lower
  values leave a steady component (Rician fading), so the deep nulls get rarer.

| Watterson reference channel | Path delay | Doppler spread |
|---|---|---|
| Good | 0.5 ms | 0.1 Hz |
| Moderate | 1 ms | 0.5 Hz |
| Poor | 2 ms | 1 Hz |
| Flutter | 0.5 ms | 10 Hz |

**Slow fading.** On top of that fast fading, the overall strength of an HF signal wanders by many
decibels over tens of seconds to minutes, as the ionosphere's absorption and focusing change with
the sun and the time of day. Propagation studies usually describe this as *log-normal*: the level
in dB varies like a bell curve around its average. afv-rs adds it with a standard deviation of
`slow_fading_db` and an 8-second time constant, limited to 6 dB above the nominal level (deep
fades are not limited). This is what makes a station sink almost into the noise for a while and
then come back. With the presets, a station spends about a quarter of the time more than 10 dB
below its nominal level under `FAIR` (7 % more than 20 dB below), and a third of the time under
`POOR` (15 % more than 20 dB below).

## 3. The noise floor

An HF receiver never goes quiet: the sum of all the world's distant lightning, plus man-made
noise, forms a constant hiss. The voice rides on top of it, so a fade does not just make the voice
quieter, it makes it sink into the noise. And because a receiver's gain control turns the gain up
when the signal weakens, the hiss rises in the pauses and fades.

**Model.** White noise ahead of the sideband filter and the radio's compressor, so the compressor
pumps it as a receiver's gain control would. `noise_level` is relative to an unfaded voice as
heard: 1 is 0 dB signal-to-noise, 0.45 is 7 dB, 0.2 is 14 dB, 0.08 is 22 dB. That is calibrated
on the default radio (Schmid ED-137B); the Rockwell and Garex presets come out 4–6 dB cleaner,
because their equalisers lift noise less.

## 4. Static crashes: crackle and blips

Lightning anywhere within thousands of kilometres radiates a very short, very broadband radio
pulse. Thousands of storms are active at any time, so an HF receiver hears a steady stream of
them: mostly small pops, the occasional loud crash, and often a run of crackling after a big one.

**Why they sound like blips rather than clicks.** The pulse itself is far shorter than a
millisecond. What you hear is the receiver's response to it. An SSB receiver has a steep crystal
filter that lets through only the voice band, about 300–2700 Hz. A filter that steep "rings" when
it is hit by an impulse, so a click comes out as a short burst at the filter's own frequencies,
lasting a few milliseconds. That is the blip. A Motorola patent on SSB noise blanking describes
exactly this: a sub-microsecond spike stretched to several milliseconds by the receiver's filters.
After a big crash, the receiver's automatic gain control briefly turns everything down, so the
voice dips too.

The frequency shift does **not** change the blips: a broadband click shifted by a few hundred hertz
is still a broadband click, and the receiver's filter is what gives it its pitch.

**Model.** Impulses 0.03–0.33 ms long arrive at random, `crackle_rate_hz` per second on average,
with sizes skewed so that most are small. A large one starts a 50–250 ms run in which they come ten
times as often. `crackle_level` sets their loudness. The static is added to the received signal,
and both pass through an 8th-order 300–2700 Hz **sideband filter**, as in a real set. That filter
makes the blips, and then the radio's compressor does the ducking that a receiver's gain control
would.

## 5. Ionospheric sounders

Ionospheric sounders ("chirp sounders") measure propagation by sweeping a transmitter across the
whole HF band, typically at 100 kHz per second, and listening for the echo. A receiver tuned
anywhere in the band hears each sweep go by. At 100 kHz/s it crosses the 2.4 kHz voice passband in
about 25 ms, which comes out of the speaker as a short tone sliding up in pitch: a "blip" (or
"bloop" for the few that sweep downwards). Dozens of sounders run around the world, so on a busy
day they pass every few seconds.

**Model.** Sweeps arrive at random, `sounder_rate_hz` per second on average. Most go up at
100 kHz/s, some at 50 or 125 kHz/s, one in five downwards. Their strength varies over 20 dB up to
`sounder_level`, and half arrive with an echo 1–4 ms later over a second path. Like the static they
go through the sideband filter, which shapes their start and end.

## Where it all happens

```
each station:   voice ─ decoder ─ mistuning + multipath fading ─┐
                                                               mix ─ + noise, static, sounders ─ SSB filter ─ hardware EQ ─ compressor ─ out
each HF radio:                                                  ┘
```

Only HF radios (below 30 MHz) with `effects` on are affected.

Aeronautical HF sets usually have no squelch, so unlike afv-native (and unlike VHF radios) an HF
radio plays its background all the time it is on: noise, static and sounders, between
transmissions as well as during them. For the same reason there is no squelch click at the end of
a transmission. It falls silent while you transmit or when the radio is muted. `hf_squelch`
restores the afv-native behaviour: no background at all, and the click. afv-native's optional HF
noise recording, if loaded, plays after the compressor whenever the background does.

## Presets

| | Offset | Drift | Path delay | Doppler spread | Depth | Slow fading | Noise | Crashes/s | Static | Sounders/s |
|---|---|---|---|---|---|---|---|---|---|---|
| `OFF` | 0 | 0 | — | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `GOOD` | ±40 Hz | ±5 Hz | 0.5 ms | 0.1 Hz | 0.8 | 2 dB | 0.08 (22 dB S/N) | 0.5 | 0.3 | 0.03 |
| `FAIR` (default) | ±200 Hz | ±25 Hz | 1 ms | 0.5 Hz | 1 | 9 dB | 0.2 (14 dB) | 2 | 0.6 | 0.06 |
| `POOR` | ±500 Hz | ±60 Hz | 2 ms | 1 Hz | 1 | 14 dB | 0.45 (7 dB) | 6 | 1 | 0.1 |
| `FLUTTER` | ±200 Hz | ±25 Hz | 0.5 ms | 10 Hz | 1 | 9 dB | 0.2 (14 dB) | 2 | 0.6 | 0.06 |

The fast fading follows Watterson's reference channels. The tuning offsets, slow fading, noise,
static and sounder figures are set by ear to match what pilots and controllers actually hear, not
taken from measurements.

## Sources

- C. C. Watterson, J. R. Juroshek, W. D. Bensema, ["Experimental confirmation of an HF channel
  model"](https://forohistorico.coit.es/index.php/biblioteca/articulos-seminales/item/experimental-confirmation-of-an-hf-channel-model),
  IEEE Trans. Communication Technology 18(6), 1970: the model itself.
- MathWorks, ["HF Ionospheric Channel Models"](https://www.mathworks.com/help/comm/ug/hf-ionospheric-channel-models.html):
  a readable explanation of the Watterson model and its ITU-R F.1487 profiles.
- W. Kelley (KC7WW), ["A Low-Cost HF Channel Simulator for Testing and Evaluating HF Digital
  Systems"](https://web.tapr.org/meetings/DCC_1999/DCC1999-HFchannelSimulator-KC7WW.pdf), TAPR DCC
  1999: the CCIR 520 good and poor channels in practice.
- Motorola, [US patent 4,466,129](https://patents.google.com/patent/US4466129), "Noise reducing
  circuitry for single sideband receivers": crystal filters stretching static impulses.
- Wikipedia, ["Ionosonde"](https://en.wikipedia.org/wiki/Ionosonde), and G3PLX, ["Chirp
  sounders"](https://jcoppens.com/radio/prop/g3plx/chirps.en.php): most sweep upwards at 100 kHz/s,
  heard here through a 2.4 kHz passband.
- Wikipedia, ["Donald Duck talk"](https://en.wikipedia.org/wiki/Donald_Duck_talk), and a 1970 Naval
  Postgraduate School report ([calhoun.nps.edu](https://calhoun.nps.edu/entities/publication/f341cc35-61c1-484b-a130-bbc41fa9e149))
  on how closely SSB can be tuned by ear (about 20–30 Hz).

The moderate (1 ms, 0.5 Hz) and flutter (0.5 ms, 10 Hz) figures are the commonly quoted CCIR 520
values; the good and poor ones are confirmed in the sources above.
