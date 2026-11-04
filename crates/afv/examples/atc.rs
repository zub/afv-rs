//! An ATC client that does its own audio device handling.
//!
//! ```text
//! cargo run -p afv --example fake_server
//! cargo run -p afv --example atc -- --api-url <printed URL> --mic speech.wav
//! AFV_CID=1234567 AFV_PASSWORD=... cargo run -p afv --example atc -- --station ENGM_TWR --seconds 60
//! ```
//!
//! The controller works `--station`: its frequency on its own antenna sites, on the headset, and
//! the first neighbour on its VCCS panel monitored on the loudspeaker.
//!
//! Most ATC clients already have an audio engine, so this one does not use the `cpal` backend.
//! It brings its own audio API (here a stand-in, [`app_audio`], whose "hardware" is WAV files)
//! and connects it through [`afv::realtime::bridge`]: the API's real-time callbacks only move
//! samples through lock-free rings, and the bridge's worker thread does the decoding, mixing,
//! encoding and sending. The devices run in whatever format suits them: here a mono microphone at
//! the recording's rate, a stereo headset at 44.1 kHz and a mono loudspeaker at 48 kHz, each
//! calling back with its own buffer size.
//!
//! With `--mic`, the controller keys up once and says the recording; against the fake server it
//! comes back half a second later as `ECHO`. Only transmit on a live network on a frequency you
//! are controlling. The run lasts `--seconds` (default 10, longer if the recording and its echo
//! need it); the headset and loudspeaker are then in `headset.wav` and `speaker.wav` in `--out`
//! (default `.`).

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use afv::radio::{Coupling, Output, OutputBus, Pan, Position, RadioConfig, Sites};
use afv::realtime::{self, Format};
use afv::{Client, EffectSamples, Event, Frequency, Session, Timing};

/// Where the controller is, for a station without antenna sites of its own.
const DEFAULT_POSITION: Position = Position { lat_deg: 60.19, lon_deg: 11.10, height_msl_m: 210.0, height_agl_m: 0.0 };
const HEADSET: Format = Format { sample_rate: 44_100, channels: 2 };
const SPEAKER: Format = Format { sample_rate: 48_000, channels: 1 };
/// How often each device calls back: what its driver's buffer size works out to.
const HEADSET_PERIOD: Duration = Duration::from_millis(10);
const SPEAKER_PERIOD: Duration = Duration::from_millis(5);
const MIC_PERIOD: Duration = Duration::from_millis(10);
/// Kept running after the transmission and its echo (the fake server replays it 0.5 s after it
/// ends), so the end of the echo is heard.
const ECHO_MARGIN: Duration = Duration::from_secs(2);

struct Args {
    station: String,
    callsign: Option<String>,
    frequency: Option<Frequency>,
    api_url: Option<String>,
    effects: Option<String>,
    mic: Option<PathBuf>,
    seconds: u64,
    out: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut values = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let name = flag.strip_prefix("--").ok_or_else(|| format!("unexpected argument {flag:?}"))?.to_owned();
        values.insert(name, args.next().ok_or_else(|| format!("{flag} needs a value"))?);
    }
    Ok(Args {
        station: values.remove("station").unwrap_or_else(|| "ENGM_TWR".into()),
        callsign: values.remove("callsign"),
        frequency: values.remove("freq").map(|f| f.parse().map_err(|e| format!("--freq: {e}"))).transpose()?,
        api_url: values.remove("api-url"),
        effects: values.remove("effects"),
        mic: values.remove("mic").map(PathBuf::from),
        seconds: values.remove("seconds").map_or(Ok(10), |s| s.parse().map_err(|_| "--seconds: not a number"))?,
        out: values.remove("out").map_or_else(|| PathBuf::from("."), PathBuf::from),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let result = match parse_args() {
        Ok(args) => run(args).await,
        Err(e) => Err(e.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let (cid, password) = match (std::env::var("AFV_CID"), std::env::var("AFV_PASSWORD")) {
        (Ok(cid), Ok(password)) => (cid, password),
        _ if args.api_url.is_some() => ("1234567".into(), "hunter2".into()),
        _ => return Err("set AFV_CID and AFV_PASSWORD, or --api-url for a fake server".into()),
    };
    let effects = match &args.effects {
        Some(dir) => common::load_effects(dir.as_ref())?,
        None => EffectSamples::default(),
    };
    let mut builder = Client::builder()
        .credentials(cid, password)
        .callsign(args.callsign.as_deref().unwrap_or(&args.station))
        .client_name(concat!("afv-rs atc example ", env!("CARGO_PKG_VERSION")))
        // ATC refreshes its transceivers less often than a moving aircraft.
        .timing(Timing::atc())
        .effects(effects);
    if let Some(url) = &args.api_url {
        builder = builder.api_url(url);
    }
    let client = builder.connect().await?;
    println!("connected as {}", client.callsign());
    let mut events = client.events();
    client.set_position(DEFAULT_POSITION);

    // The primary frequency: transmit and receive on every antenna site of the station, coupled
    // so that pilots near one site hear those near another (an "XC" button). A controller
    // combining several frequencies would set `Coupling::Across` on each of them instead.
    let station = client.session().station(&args.station).await?.ok_or(format!("no station {}", args.station))?;
    let frequency = args
        .frequency
        .or(station.frequency.map(Frequency::from_hz))
        .ok_or("the station has no frequency; pass --freq")?;
    let primary = client.add_radio(RadioConfig {
        tx: true,
        coupling: Coupling::Sites,
        ..RadioConfig::atc(frequency, station_sites(client.session(), &args.station).await?)
    });
    let mut names = HashMap::from([(primary, format!("{} {frequency}", args.station))]);
    println!("{}", names[&primary]);

    // A neighbour from the station's VCCS panel, receive only, on the loudspeaker.
    let neighbour = client.session().vccs(&args.station).await?.into_iter().find(|s| s.frequency.is_some());
    if let Some(neighbour) = neighbour {
        let frequency = Frequency::from_hz(neighbour.frequency.expect("filtered"));
        let monitor = client.add_radio(RadioConfig {
            output: Output { bus: OutputBus::Speaker, pan: Pan::Both },
            ..RadioConfig::atc(frequency, station_sites(client.session(), &neighbour.name).await?)
        });
        names.insert(monitor, format!("{} {frequency} (monitor)", neighbour.name));
        println!("{}", names[&monitor]);
    }

    // The application's own devices, connected through the bridge.
    let speaking = Arc::new(AtomicBool::new(false));
    let (mic, mic_format, recording) = match &args.mic {
        Some(path) => {
            let mic = app_audio::Microphone::open(path, speaking.clone())?;
            let (format, length) = (mic.format(), mic.length());
            (Some(mic), Some(format), length)
        }
        None => (None, None, Duration::ZERO),
    };
    let audio = client.audio();
    let (bridge, capture, mut playback) =
        realtime::bridge(&audio, mic_format, &[(OutputBus::Headset, HEADSET), (OutputBus::Speaker, SPEAKER)])?;
    let (mut headset, mut speaker) = (playback.remove(0), playback.remove(0));
    // Each callback is all the real-time code there is: hand the buffer to the bridge.
    let devices = [
        app_audio::open_output(&args.out.join("headset.wav"), HEADSET, HEADSET_PERIOD, move |out| headset.fill(out))?,
        app_audio::open_output(&args.out.join("speaker.wav"), SPEAKER, SPEAKER_PERIOD, move |out| speaker.fill(out))?,
    ];
    let mic = match (mic, capture) {
        (Some(mic), Some(mut capture)) => Some(mic.start(MIC_PERIOD, move |samples| capture.push(samples))),
        _ => None,
    };

    // Key up once with a recording; PTT opens when the server knows the transceivers.
    if mic.is_some() {
        client.set_ptt(true);
    }
    let run_for = Duration::from_secs(args.seconds).max(2 * recording + ECHO_MARGIN);
    let end = tokio::time::sleep(run_for);
    tokio::pin!(end);
    let mut poll = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            _ = &mut end => break,
            _ = poll.tick() => {
                // The controller has finished speaking: release PTT.
                if client.ptt_open() && !speaking.load(Relaxed) {
                    client.set_ptt(false);
                }
            }
            Some(event) = events.next() => match event {
                Event::PttOpened => {
                    println!("transmitting");
                    speaking.store(true, Relaxed);
                }
                Event::PttClosed => println!("stopped transmitting"),
                Event::RxStarted { radio, callsign } => println!("{}: {callsign}", names[&radio]),
                Event::RxEnded { radio, callsign } => println!("{}: {callsign} ended", names[&radio]),
                Event::TransceiversSynced => println!("transceivers synced"),
                Event::TransceiverSyncFailed(e) => println!("transceiver update failed: {e}"),
                Event::Session(state) => println!("session: {state:?}"),
                _ => {}
            },
        }
    }

    // Stop the devices before the bridge they feed, then leave the network.
    drop(mic);
    drop(devices);
    drop(bridge);
    client.disconnect().await;
    println!("wrote headset.wav and speaker.wav to {}", args.out.display());
    Ok(())
}

/// A station's antenna sites; the controller's own position for one without any.
async fn station_sites(session: &Session, name: &str) -> afv::Result<Sites> {
    let sites = session.station_transceivers(name).await?;
    Ok(if sites.is_empty() { Sites::Own } else { Sites::from_station(&sites) })
}

/// A stand-in for the application's own audio API. Each device calls back on a thread of its own,
/// at a fixed period, as a driver's real-time callback would; the "hardware" behind it is a WAV
/// file. Nothing here knows about AFV.
mod app_audio {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use afv::realtime::Format;

    /// A running device. Dropping it stops the callbacks.
    pub struct Device {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Device {
        fn drop(&mut self) {
            self.stop.store(true, Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Runs `tick` every `period` until the device is dropped, without drifting.
    fn clocked(period: Duration, mut tick: impl FnMut() + Send + 'static) -> Device {
        let stop = Arc::new(AtomicBool::new(false));
        let running = stop.clone();
        let thread = thread::spawn(move || {
            let mut next = Instant::now();
            while !running.load(Relaxed) {
                tick();
                next += period;
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        Device { stop, thread: Some(thread) }
    }

    fn frames(format: Format, period: Duration) -> usize {
        (format.sample_rate as f64 * period.as_secs_f64()).round() as usize * format.channels as usize
    }

    /// An output that asks `callback` for interleaved samples every `period` and "plays" them into
    /// a 32-bit float WAV file at `path`.
    pub fn open_output(
        path: &Path,
        format: Format,
        period: Duration,
        mut callback: impl FnMut(&mut [f32]) + Send + 'static,
    ) -> Result<Device, hound::Error> {
        let spec = hound::WavSpec {
            channels: format.channels,
            sample_rate: format.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut file = Some(hound::WavWriter::create(path, spec)?);
        let name = path.display().to_string();
        let mut buffer = vec![0.0; frames(format, period)];
        Ok(clocked(period, move || {
            callback(&mut buffer);
            if let Some(writer) = &mut file
                && buffer.iter().try_for_each(|&s| writer.write_sample(s)).is_err()
            {
                eprintln!("cannot write {name}; that output is lost");
                file = None;
            }
        }))
    }

    /// A microphone that hears a recording while `speaking` is set, and silence otherwise. It
    /// clears `speaking` when the recording ends.
    pub struct Microphone {
        format: Format,
        recording: Vec<f32>,
        speaking: Arc<AtomicBool>,
    }

    impl Microphone {
        /// Loads the recording; the microphone runs at its rate and channel count.
        pub fn open(path: &Path, speaking: Arc<AtomicBool>) -> Result<Self, hound::Error> {
            let (spec, recording) = crate::common::read(path)?;
            let format = Format { sample_rate: spec.sample_rate, channels: spec.channels };
            Ok(Self { format, recording, speaking })
        }

        pub fn format(&self) -> Format {
            self.format
        }

        pub fn length(&self) -> Duration {
            let frames = self.recording.len() / self.format.channels as usize;
            Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
        }

        /// Starts calling `callback` with what the microphone hears every `period`.
        pub fn start(self, period: Duration, mut callback: impl FnMut(&[f32]) + Send + 'static) -> Device {
            let Self { format, recording, speaking } = self;
            let mut buffer = vec![0.0; frames(format, period)];
            let mut position = 0;
            clocked(period, move || {
                buffer.fill(0.0);
                if speaking.load(Relaxed) {
                    let n = buffer.len().min(recording.len() - position);
                    buffer[..n].copy_from_slice(&recording[position..position + n]);
                    position += n;
                    if position == recording.len() {
                        speaking.store(false, Relaxed);
                    }
                }
                callback(&buffer);
            })
        }
    }
}
