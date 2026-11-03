//! A minimal pilot client on the default audio devices.
//!
//! ```text
//! AFV_CID=1234567 AFV_PASSWORD=... cargo run -p afv --features cpal --example pilot -- \
//!     --callsign SAS123 --freq 122.800 --lat 60.19 --lon 11.10 --alt 200 [--effects DIR] [--api-url URL]
//! cargo run -p afv --features cpal --example pilot -- --list-devices
//! ```
//!
//! Press Enter to toggle PTT, type a frequency (`121.500`) to retune, `q` to quit. Without
//! credentials it connects to `--api-url` with the fake server's test login, so it pairs with
//! `cargo run -p afv --example fake_server`.

use std::collections::HashMap;
use std::process::ExitCode;

use afv::devices::{self, DeviceConfig};
use afv::radio::{Position, RadioConfig};
use afv::{Client, EffectSamples, Event, Frequency};

struct Args {
    callsign: String,
    frequency: Frequency,
    position: Position,
    api_url: Option<String>,
    effects: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut values = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--list-devices" {
            return Ok(None);
        }
        let name = flag.strip_prefix("--").ok_or_else(|| format!("unexpected argument {flag:?}"))?.to_owned();
        values.insert(name, args.next().ok_or_else(|| format!("{flag} needs a value"))?);
    }
    let number = |name: &str, default: f64| -> Result<f64, String> {
        values.get(name).map_or(Ok(default), |v| v.parse().map_err(|_| format!("--{name}: not a number")))
    };
    Ok(Some(Args {
        callsign: values.get("callsign").cloned().unwrap_or_else(|| "AFVRS1".into()),
        frequency: values.get("freq").map_or("122.800", String::as_str).parse().map_err(|e| format!("--freq: {e}"))?,
        position: Position {
            lat_deg: number("lat", 60.19)?,
            lon_deg: number("lon", 11.10)?,
            height_msl_m: number("alt", 200.0)?,
            height_agl_m: 0.0,
        },
        api_url: values.get("api-url").cloned(),
        effects: values.get("effects").cloned(),
    }))
}

fn list_devices() -> Result<(), devices::DeviceError> {
    for (kind, list) in [("input", devices::input_devices()?), ("output", devices::output_devices()?)] {
        for d in list {
            println!("{kind:6} {}  ({})", d.name, d.id);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return list_devices().map_or(ExitCode::FAILURE, |_| ExitCode::SUCCESS),
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match run(args).await {
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
        Some(dir) => EffectSamples::load_dir(dir)?,
        None => EffectSamples::default(),
    };
    let mut builder = Client::builder()
        .credentials(cid, password)
        .callsign(&args.callsign)
        .client_name(concat!("afv-rs pilot example ", env!("CARGO_PKG_VERSION")))
        .effects(effects);
    if let Some(url) = &args.api_url {
        builder = builder.api_url(url);
    }
    let client = builder.connect().await?;
    println!("connected as {}", client.callsign());

    let mut events = client.events();
    client.set_position(args.position);
    let com1 = client.add_radio(RadioConfig { tx: true, ..RadioConfig::pilot(args.frequency) });
    let _devices = devices::attach(&client.audio(), DeviceConfig::defaults())?;
    println!("COM1 {}. Enter toggles PTT, a frequency retunes, q quits.", args.frequency);

    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut ptt = false;
    loop {
        tokio::select! {
            line = lines.recv() => match line.as_deref().map(str::trim) {
                None | Some("q") => break,
                Some("") => {
                    ptt = !ptt;
                    client.set_ptt(ptt);
                    println!("PTT {}", if ptt { "pressed" } else { "released" });
                }
                Some(text) => match text.parse::<Frequency>() {
                    Ok(f) => {
                        client.update_radio(com1, |r| r.frequency = f);
                        println!("COM1 {f}");
                    }
                    Err(e) => println!("{e}"),
                },
            },
            Some(event) = events.next() => match event {
                Event::RxStarted { callsign, .. } => println!("RX {callsign}"),
                Event::RxEnded { callsign, .. } => println!("RX {callsign} ended"),
                Event::PttOpened => println!("transmitting"),
                Event::PttClosed => println!("stopped transmitting"),
                Event::Session(state) => println!("session: {state:?}"),
                Event::TransceiverSyncFailed(e) => println!("transceiver update failed: {e}"),
                Event::DeviceError(e) => println!("audio device: {e}"),
                _ => {}
            },
        }
    }
    client.disconnect().await;
    Ok(())
}
