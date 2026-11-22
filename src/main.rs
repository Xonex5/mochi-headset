//! Headset battery: the battery of a USB headset whose dongle doesn't give it
//! to the kernel, so UPower never sees it, like a Logitech PRO X Wireless.
//!
//! The backend asks `headsetcontrol -b -o json` on a timer and publishes the
//! headsets it reports as `devices`, shaped like the battery module's own
//! `devices`. The plugin's `battery/Card` override adds them to the battery
//! card, under the mouse. A headset dropping to `low` shows a notice once,
//! and again only after it charges.

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use mochi_sdk::{ActivitySpec, ModuleCommand, ModuleCtx, ModuleEvent, Priority, Value, json};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::{Notify, mpsc};

/// How long a notice stays, as the battery module's.
const NOTICE: Duration = Duration::from_millis(3000);
/// How long `headsetcontrol` may take before it counts as no answer.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How far over `low` a headset climbs before it can be low again, as the
/// battery module does for its devices.
const RECOVERED: u32 = 5;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    /// Seconds between two readings.
    interval: u64,
    /// Percent at which a notice says the headset is low; 0 turns it off.
    low: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            interval: 30,
            low: 15,
        }
    }
}

/// One headset with a battery, as `headsetcontrol` reports it.
#[derive(Debug, Clone, PartialEq)]
struct Headset {
    /// `headsetcontrol:<vendor id>:<product id>`: stays the same across
    /// connections.
    id: String,
    name: String,
    level: u32,
    charging: bool,
    /// Minutes until empty, when it says.
    minutes_left: Option<u64>,
}

impl Headset {
    fn low(&self, low: u32) -> bool {
        !self.charging && self.level <= low
    }
}

fn main() -> std::process::ExitCode {
    mochi_sdk::run(run)
}

async fn run(mut ctx: ModuleCtx) -> Result<(), mochi_sdk::Error> {
    let settings: Settings = ctx.settings()?;
    if settings.low > 100 {
        return Err(format!("low = {} is over 100%", settings.low).into());
    }
    let interval = Duration::from_secs(settings.interval.max(5));

    let (sender, mut readings) = mpsc::unbounded_channel();
    let refresh = Arc::new(Notify::new());
    tokio::spawn(poll(sender, interval, refresh.clone()));

    let mut headsets: Vec<Headset> = Vec::new();
    let mut warnings = Warnings::default();
    // The devices the battery module lists from UPower, by lowercased name.
    // It sends its own notice for those.
    let mut upower: HashSet<String> = HashSet::new();
    // Nothing yet: the card shows UPower's devices alone until the first
    // reading.
    ctx.publish_state(state(&headsets, settings.low));

    loop {
        tokio::select! {
            event = ctx.next_event() => match event {
                // mochid closed the connection: time to go.
                None => return Ok(()),
                Some(ModuleEvent::Command(command)) => {
                    handle(command, &headsets, &refresh);
                }
                Some(ModuleEvent::State { module, state }) if module == "battery" => {
                    upower = names(&state);
                }
                Some(_) => {}
            },
            Some(output) = readings.recv() => {
                let reading = parse(&output, &headsets);
                for headset in warnings.apply(&reading, settings.low) {
                    if !upower.contains(&headset.name.to_lowercase()) {
                        notify(&ctx, headset);
                    }
                }
                if reading != headsets {
                    headsets = reading;
                    ctx.publish_state(state(&headsets, settings.low));
                }
            }
        }
    }
}

/// The lowercased names of the devices in the battery module's state.
fn names(state: &Value) -> HashSet<String> {
    state["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|device| device["name"].as_str())
        .map(str::to_lowercase)
        .collect()
}

fn handle(command: ModuleCommand, headsets: &[Headset], refresh: &Notify) {
    match command.action.as_str() {
        "status" => command.answer(Ok(status(headsets))),
        "refresh" => {
            refresh.notify_one();
            command.reply(Ok(()));
        }
        _ => command.reply(Err("no such action".into())),
    }
}

/// Runs `headsetcontrol` every `interval`, or at once when asked, and sends
/// what it printed until nobody listens.
async fn poll(sender: mpsc::UnboundedSender<Vec<u8>>, interval: Duration, refresh: Arc<Notify>) {
    loop {
        if sender.send(read().await).is_err() {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = refresh.notified() => {}
        }
    }
}

/// Runs `headsetcontrol -b -o json`. No program, an error or no answer in
/// time all mean no headset.
async fn read() -> Vec<u8> {
    let child = Command::new("headsetcontrol")
        .args(["-b", "-o", "json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(TIMEOUT, child).await {
        // It exits with an error when no headset is plugged in, and prints
        // the JSON all the same.
        Ok(Ok(output)) => output.stdout,
        Ok(Err(error)) => {
            eprintln!("headsetcontrol: {error}");
            Vec::new()
        }
        Err(_) => {
            eprintln!("headsetcontrol gave no answer in {} s", TIMEOUT.as_secs());
            Vec::new()
        }
    }
}

/// The headsets with a battery in `headsetcontrol`'s JSON. A headset that's
/// off reports `BATTERY_UNAVAILABLE`, and is left out. Some headsets report
/// a negative level while charging: they keep their level from `previous`,
/// or are left out until a reading has one.
fn parse(output: &[u8], previous: &[Headset]) -> Vec<Headset> {
    let Ok(json) = serde_json::from_slice::<Value>(output) else {
        return Vec::new();
    };
    let Some(devices) = json["devices"].as_array() else {
        return Vec::new();
    };
    let mut headsets: Vec<Headset> = devices
        .iter()
        .filter(|device| device["status"] == "success")
        .filter_map(|device| {
            let battery = &device["battery"];
            let charging = match battery["status"].as_str()? {
                "BATTERY_AVAILABLE" => false,
                "BATTERY_CHARGING" => true,
                _ => return None,
            };
            let text = |key: &str| device[key].as_str().unwrap_or_default().trim().to_owned();
            let id = format!(
                "headsetcontrol:{}:{}",
                text("id_vendor"),
                text("id_product")
            );
            let level = match battery["level"].as_f64()? {
                level if level >= 0.0 => level.min(100.0).round() as u32,
                _ => previous.iter().find(|headset| headset.id == id)?.level,
            };
            let name = [text("product"), text("device")]
                .into_iter()
                .find(|name| !name.is_empty())
                .unwrap_or_else(|| "Headset".to_owned());
            Some(Headset {
                id,
                name,
                level,
                charging,
                minutes_left: battery["time_to_empty_min"]
                    .as_u64()
                    .filter(|minutes| *minutes > 0),
            })
        })
        .collect();
    headsets.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    headsets
}

/// The plugin's state: its headsets, each shaped like an entry of the
/// battery module's `devices`, so the card draws them the same way.
fn state(headsets: &[Headset], low: u32) -> Value {
    json!({
        "devices": headsets
            .iter()
            .map(|headset| json!({
                "id": headset.id,
                "name": headset.name,
                "kind": "Headset",
                "icon": "headset_mic",
                "level": headset.level,
                "charging": headset.charging,
                "low": headset.low(low),
                "left": headset.minutes_left.map(duration),
            }))
            .collect::<Vec<_>>(),
    })
}

/// What `mochi ipc headset status` prints.
fn status(headsets: &[Headset]) -> String {
    if headsets.is_empty() {
        return "no headset reports its battery".to_owned();
    }
    headsets
        .iter()
        .map(|headset| {
            let mut line = format!("{} (headset): {}%", headset.name, headset.level);
            if headset.charging {
                line.push_str(", charging");
            } else if let Some(minutes) = headset.minutes_left {
                line.push_str(&format!(", {} left", duration(minutes)));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `3 h 20 min`, `45 min`, from minutes.
fn duration(minutes: u64) -> String {
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

/// Remembers which headsets were said to be low, by id, so the notice comes
/// once per discharge. The first reading is only a baseline: a headset
/// already low when Mochi starts says nothing, as the battery module does.
#[derive(Debug, Default)]
struct Warnings {
    started: bool,
    warned: HashSet<String>,
}

impl Warnings {
    /// Each headset that just got low.
    fn apply<'a>(&mut self, headsets: &'a [Headset], low: u32) -> Vec<&'a Headset> {
        let first = !self.started;
        self.started = true;
        if low == 0 {
            return Vec::new();
        }
        let mut notices = Vec::new();
        for headset in headsets {
            if headset.charging || headset.level > low + RECOVERED {
                self.warned.remove(&headset.id);
                continue;
            }
            if headset.level > low || !self.warned.insert(headset.id.clone()) || first {
                continue;
            }
            notices.push(headset);
        }
        notices
    }
}

/// A short notice on the island, as the battery module shows for a mouse.
fn notify(ctx: &ModuleCtx, headset: &Headset) {
    ctx.present(
        ActivitySpec::new("Notice")
            .key("low")
            .priority(Priority::HIGH)
            .passive()
            .fleeting()
            .timeout(NOTICE)
            .payload(json!({
                "text": notice(headset),
                "level": headset.level,
                "icon": "headset_mic",
            })),
    );
}

/// `PRO X Wireless Gaming Headset battery low · 15%`.
fn notice(headset: &Headset) -> String {
    format!("{} battery low · {}%", headset.name, headset.level)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRO_X: &str = r#"{
      "devices": [{
        "status": "success",
        "device": "Logitech G PRO Series",
        "vendor": "Logitech",
        "product": "PRO X Wireless Gaming Headset",
        "id_vendor": "0x046d",
        "id_product": "0x0aba",
        "battery": { "status": "BATTERY_AVAILABLE", "level": 26, "time_to_empty_min": 234 }
      }]
    }"#;

    fn headset(level: u32, charging: bool) -> Headset {
        Headset {
            id: "headsetcontrol:0x046d:0x0aba".into(),
            name: "PRO X".into(),
            level,
            charging,
            minutes_left: None,
        }
    }

    #[test]
    fn reads_the_pro_x() {
        let headsets = parse(PRO_X.as_bytes(), &[]);
        assert_eq!(
            headsets,
            vec![Headset {
                id: "headsetcontrol:0x046d:0x0aba".into(),
                name: "PRO X Wireless Gaming Headset".into(),
                level: 26,
                charging: false,
                minutes_left: Some(234),
            }]
        );
        let state = state(&headsets, 15);
        assert_eq!(state["devices"][0]["icon"], "headset_mic");
        assert_eq!(state["devices"][0]["low"], false);
        assert_eq!(state["devices"][0]["left"], "3 h 54 min");
        assert_eq!(
            status(&headsets),
            "PRO X Wireless Gaming Headset (headset): 26%, 3 h 54 min left"
        );
    }

    #[test]
    fn leaves_out_a_headset_that_is_off() {
        let off = PRO_X.replace("BATTERY_AVAILABLE", "BATTERY_UNAVAILABLE");
        assert!(parse(off.as_bytes(), &[]).is_empty());
        assert!(parse(b"", &[]).is_empty());
        assert!(parse(br#"{"devices": []}"#, &[]).is_empty());
        let charging = PRO_X.replace("BATTERY_AVAILABLE", "BATTERY_CHARGING");
        assert!(parse(charging.as_bytes(), &[])[0].charging);
        // A negative level while charging keeps the last one, or waits.
        let unknown = charging.replace("\"level\": 26", "\"level\": -1");
        assert!(parse(unknown.as_bytes(), &[]).is_empty());
        let before = parse(PRO_X.as_bytes(), &[]);
        let now = parse(unknown.as_bytes(), &before);
        assert_eq!(now[0].level, 26);
        assert!(now[0].charging);
    }

    #[test]
    fn says_once_when_the_headset_gets_low() {
        let mut warnings = Warnings::default();
        // Low at the start: only a baseline.
        assert!(warnings.apply(&[headset(10, false)], 15).is_empty());
        assert!(warnings.apply(&[headset(9, false)], 15).is_empty());
        // Charged past low + 5, it can be low again.
        assert!(warnings.apply(&[headset(30, false)], 15).is_empty());
        let low = [headset(15, false)];
        let notices = warnings.apply(&low, 15);
        assert_eq!(notices.len(), 1);
        assert_eq!(notice(notices[0]), "PRO X battery low · 15%");
        assert!(warnings.apply(&[headset(14, false)], 15).is_empty());
        // Off, then back on still low: no second notice.
        assert!(warnings.apply(&[], 15).is_empty());
        assert!(warnings.apply(&[headset(13, false)], 15).is_empty());
        // Charging rearms it.
        assert!(warnings.apply(&[headset(13, true)], 15).is_empty());
        assert_eq!(warnings.apply(&[headset(13, false)], 15).len(), 1);
        // 0 turns the notices off.
        let mut off = Warnings::default();
        off.apply(&[], 0);
        assert!(off.apply(&[headset(5, false)], 0).is_empty());
    }
}
