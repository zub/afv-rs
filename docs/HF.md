# How HF radio is simulated

Aviation HF voice sounds very different from VHF. This page explains, in plain terms, what causes
that and how afv-rs imitates it. None of this is in afv-native, which plays HF like VHF apart from
a noise recording; the AFV servers only tell the client *who* is transmitting on *which*
frequency, so everything below happens in the receiving client.

The code is in `crates/afv-audio/src/dsp/hf/` (propagation in `channel.rs`, noise, static and
sounders in `atmospherics.rs`) and the HF set in `dsp/hardware.rs`; the strength of every effect
is set by `HfConditions` (`client.audio().set_hf_conditions(...)`).

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

**Model.** Each station has its own fixed tuning error, drawn at random when it is first heard,
anywhere within ±`offset_hz`. On top, the error drifts slowly by up to ±`drift_hz`, heading for a
new random value every 2–6 seconds. The shift itself is done exactly: a Hilbert filter pair turns
the audio into an "analytic" signal, which is then rotated by Δf.

**How far off is realistic.** Aviation HF is channelised, so nobody tunes by ear; the error comes
from the equipment and from motion. Regulations allow an aeronautical station 10 Hz and an aircraft
20 Hz (FCC 47 CFR 87.133, matching the ITU Radio Regulations; ITU-R M.1458 for HF data link), so
sets within tolerance are at most 30–40 Hz apart. An airliner's speed adds up to about 8 Hz of
Doppler at 10 MHz and 15 Hz at 18 MHz, and the ionosphere a few hertz of slowly changing Doppler.
`GOOD` stays close to that; the other presets go further, set by ear to the mistuned voices pilots
and controllers report, as from sets out of tolerance.

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
| ITU-R F.1487 low latitudes, moderate | 2 ms | 1.5 Hz |

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
quieter, it makes it sink into the noise. An HF receiver's AGC holds its gain through the short
pauses between words ("hang"), so during a transmission the hiss stays where the signal put it;
it rises only in fades, and after the transmission ends.

**Model.** White noise ahead of the receiving set. `noise_level` is the noise relative to an
unfaded voice, both measured in a 300–3000 Hz voice channel: 1 is 0 dB signal-to-noise, 0.1 is
20 dB. A set with a narrower filter hears a little less of it, as a real one does. ITU-R grades
SSB telephony by the least signal-to-noise each grade needs (F.339, quoted in F.240): 33 dB for
good commercial quality, 15 dB for marginally commercial and 6 dB for just usable. Circuits are
planned to do better than their grade's minimum, so the presets sit inside the grades: `GOOD` at
43 dB, `FAIR` at 29 dB (between the two commercial grades), `POOR` at 19 dB (just above
marginally commercial) and `ABYSMAL` at 14 dB (just below it). Just usable is a `noise_level` of 0.5. Measured as active speech against the noise
at the output of the HF set (section 6), the setting and the result agree within about 1 dB.

The set's hang AGC holds the gain between words, so the hiss does not swell in every pause. It
rises in fades, and between transmissions by up to the set's reserve of gain. The transmitting
station's speech processor (a compressor, as on HF SSB transmitters) works on the voice alone,
before propagation, so it never pumps the noise.

## 4. Static crashes: crackle and blips

Lightning anywhere within thousands of kilometres radiates a very short, very broadband radio
pulse. Thousands of storms are active at any time, so an HF receiver hears a steady stream of
them: mostly small pops, the occasional loud crash, and often a run of crackling after a big one.

**Why they sound like blips rather than clicks.** The pulse itself is far shorter than a
millisecond. What you hear is the receiver's response to it. An SSB receiver has a steep crystal
filter that lets through only the voice band, about 300–2500 Hz. A filter that steep "rings" when
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
and both pass through the HF set's **sideband filter** (section 6), which makes the blips. Its AGC
turns down for a big crash and holds, so the voice dips for a moment after it; the leading edge of
a crash outruns the AGC's attack, and a look-ahead limiter at the end of the set stops it from
clipping.

## 5. Ionospheric sounders

Ionospheric sounders ("chirp sounders") measure propagation by sweeping a transmitter across the
whole HF band, typically at 100 kHz per second, and listening for the echo. A receiver tuned
anywhere in the band hears each sweep go by. At 100 kHz/s it crosses the 2.2 kHz voice passband in
about 22 ms, which comes out of the speaker as a short tone sliding up in pitch: a "blip" (or
"bloop" for the few that sweep downwards). Dozens of sounders run around the world, so on a busy
day they pass every few seconds.

**Model.** Sweeps arrive at random, `sounder_rate_hz` per second on average. Most go up at
100 kHz/s, some at 50 or 125 kHz/s, one in five downwards. Their strength varies over 20 dB up to
`sounder_level`, and half arrive with an echo 1–4 ms later over a second path. Sounders are
transmitters of their own, so they do not depend on the station being heard: every preset but
`OFF` has the same, 0.1 per second at level 0.8. Like the static they
go through the sideband filter, which shapes their start and end.

## 6. The receiving set: Collins HFS-900D

HF frequencies are always heard through an HF set (`RadioSettings::hf_hardware`), never through
the VHF sets, which have no business on HF. The one simulated is the Collins HFS-900D, an
ARINC 719 HF voice and data (HFDL) radio common in airliners, the 737NG among them.

**Selectivity (published).** Collins' data sheet gives the SSB passband as varying less than
4 dB from 350 to 2500 Hz, at least 35 dB down at 2900 Hz and on the carrier side, and 60 dB down
beyond 3300 Hz. afv-rs meets that mask with a 10th-order Chebyshev low-pass (2 dB ripple, ending at
2500 Hz: 40 dB down at 2900 Hz, 61 dB at 3300 Hz) and a 6th-order Butterworth high-pass at 300 Hz.
The upper edge falls 35 dB in 400 Hz: the set sounds narrow, with nothing above 2.5 kHz, and its
filter rings a little on static.

**AGC (not published; typical of its class).** The gain falls to the loudest peaks with a 3 ms
attack (looking 2 ms ahead), holds for a quarter of a second after the signal drops, so it does
not pump up between words, then recovers at 20 dB per second. It can rise at most 8 dB above what
an unfaded voice needs, like the RF sensitivity control turned down to where crews usually leave
it: moderate fades are made up for, and between transmissions the hiss comes up by that much. The
Marconi 915, an airborne HF set of the same class, specifies at most 12 ms attack and 1.5 s
recovery.

**Audio.** The response is flat; typical speech plays about 7 dB below the VHF sets. The radio's
volume comes after the set, as on the real one.

**Not modelled.** The set's own frequency error (±20 Hz, from an oven-controlled oscillator)
is well inside the per-station tuning errors of section 1. Its transmit audio response is not
applied: the hardware only shapes what you hear. AM reception, SELCAL and the 100 Hz channel steps
are not simulated.

## Where it all happens

```
each station:   voice ─ decoder ─ speech processor ─ mistuning + multipath fading ─┐
                                                                                  mix ─ + noise, static, sounders ─ HF set ─ volume ─ out
each HF radio:                                                                     ┘
HF set:         sideband filter ─ hang AGC ─ limiter
```

Only HF radios (below 30 MHz) with `effects` on are affected.

Aeronautical HF sets usually have no squelch, so unlike afv-native (and unlike VHF radios) an HF
radio plays its background all the time it is on: noise, static and sounders, between
transmissions as well as during them. For the same reason there is no squelch click at the end of
a transmission. It falls silent while you transmit or when the radio is muted. `hf_squelch`
restores the afv-native behaviour: no background at all, and the click. afv-native's optional HF
noise recording, if loaded, plays after the audio stage whenever the background does.

## Presets

| | Offset | Drift | Path delay | Doppler spread | Depth | Slow fading | Noise | Crashes/s | Static | Sounders/s |
|---|---|---|---|---|---|---|---|---|---|---|
| `OFF` | 0 | 0 | — | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `GOOD` | ±25 Hz | ±3 Hz | 0.5 ms | 0.1 Hz | 0.8 | 2 dB | 0.007 (43 dB S/N) | 0.5 | 0.3 | 0.1 |
| `FAIR` (default) | ±100 Hz | ±15 Hz | 1 ms | 0.5 Hz | 1 | 9 dB | 0.035 (29 dB) | 2 | 0.6 | 0.1 |
| `POOR` | ±250 Hz | ±40 Hz | 2 ms | 1 Hz | 1 | 14 dB | 0.11 (19 dB) | 6 | 1 | 0.1 |
| `ABYSMAL` | ±375 Hz | ±50 Hz | 2 ms | 1.5 Hz | 1 | 16 dB | 0.2 (14 dB) | 10 | 1 | 0.1 |
| `FLUTTER` | ±100 Hz | ±15 Hz | 0.5 ms | 10 Hz | 1 | 9 dB | 0.035 (29 dB) | 2 | 0.6 | 0.1 |

The fast fading follows Watterson's reference channels (and, for `ABYSMAL`, ITU-R F.1487's
low-latitude moderate channel), and the noise floors sit within ITU-R's quality grades. The tuning
offsets (section 1), slow fading, static and sounder figures are set by ear to match what pilots
and controllers actually hear, not taken from measurements.

## Sources

- C. C. Watterson, J. R. Juroshek, W. D. Bensema, ["Experimental confirmation of an HF channel
  model"](https://forohistorico.coit.es/index.php/biblioteca/articulos-seminales/item/experimental-confirmation-of-an-hf-channel-model),
  IEEE Trans. Communication Technology 18(6), 1970: the model itself.
- MathWorks, ["HF Ionospheric Channel Models"](https://www.mathworks.com/help/comm/ug/hf-ionospheric-channel-models.html):
  a readable explanation of the Watterson model and its ITU-R F.1487 profiles, including the
  low-latitude moderate channel (2 ms, 1.5 Hz).
- W. Kelley (KC7WW), ["A Low-Cost HF Channel Simulator for Testing and Evaluating HF Digital
  Systems"](https://web.tapr.org/meetings/DCC_1999/DCC1999-HFchannelSimulator-KC7WW.pdf), TAPR DCC
  1999: the CCIR 520 good and poor channels in practice.
- Motorola, [US patent 4,466,129](https://patents.google.com/patent/US4466129), "Noise reducing
  circuitry for single sideband receivers": crystal filters stretching static impulses.
- Wikipedia, ["Ionosonde"](https://en.wikipedia.org/wiki/Ionosonde), and G3PLX, ["Chirp
  sounders"](https://jcoppens.com/radio/prop/g3plx/chirps.en.php): most sweep upwards at 100 kHz/s,
  heard here through a 2.2 kHz passband.
- Collins (Rockwell Collins), ["HFS-900D High Speed Data Radio"](https://www.radiomanual.info/schemi/Surplus_Civil/Collins_HFS-900D_broch_2015.pdf),
  data sheet 147-0795-001, 2015: selectivity, sensitivity, frequency stability, RF sensitivity
  control.
- Marconi, ["915 HF Airborne Transceiver"](http://pdf.textfiles.com/manuals/SCANNERS-F-R/Marconi_915%20HF%20Airborne%20Transceivers_Datasheet.pdf)
  data sheet: AGC timing of an airborne HF set of the same class.
- ITU-R, [Recommendation F.240-6](https://www.itu.int/dms_pubrec/itu-r/rec/f/R-REC-F.240-6-199203-S!!PDF-E.pdf)
  and [F.339-8](https://www.itu.int/dms_pubrec/itu-r/rec/f/R-REC-F.339-8-201302-I!!PDF-E.pdf):
  33, 15 and 6 dB audio signal-to-noise for good commercial, marginally commercial and just usable
  SSB telephony.
- US FCC, [47 CFR 87.133](https://hallikainen.org/org/FCC/FccRules/2006/87/133), frequency
  tolerance (2006 edition), and ITU-R [Recommendation M.1458](https://www.itu.int/dms_pubrec/itu-r/rec/m/R-REC-M.1458-0-200005-I!!PDF-E.pdf):
  10 Hz for aeronautical stations and 20 Hz for aircraft on HF single sideband.
- Wikipedia, ["Donald Duck talk"](https://en.wikipedia.org/wiki/Donald_Duck_talk), and a 1970 Naval
  Postgraduate School report ([calhoun.nps.edu](https://calhoun.nps.edu/entities/publication/f341cc35-61c1-484b-a130-bbc41fa9e149))
  on how closely SSB can be tuned by ear (about 20–30 Hz).

The moderate (1 ms, 0.5 Hz) and flutter (0.5 ms, 10 Hz) figures are the commonly quoted CCIR 520
values; the good and poor ones are confirmed in the sources above.
