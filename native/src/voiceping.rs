//! How the Discord voice connection is doing: its delay, for the hint and the window over the signal icon.
//! All of it is measured on our own connection and stays here.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Local;

/// A delay is kept every few seconds, for about five minutes.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);
const MAX_SAMPLES: usize = 60;
/// The chart's own coordinates (`viewbox` in the window).
const CHART_W: f32 = 300.0;
const CHART_H: f32 = 80.0;

#[derive(Default)]
struct State {
    samples: VecDeque<u32>,
    sampled_at: Option<Instant>,
    server: String,
    loss: Option<f32>,
    encrypted: bool,
    code: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    f(STATE.lock().unwrap().get_or_insert_with(State::default))
}

/// "c-ord12-04bb0383.discord.media:443" says "c-ord12-04bb0383".
fn server_name(endpoint: &str) -> String {
    endpoint.trim_start_matches("wss://").split(['.', ':', '/']).next().unwrap_or("").to_string()
}

pub fn set_server(endpoint: &str) {
    with(|s| s.server = server_name(endpoint));
}

pub fn set_privacy(encrypted: bool, code: Option<&str>) {
    with(|s| {
        s.encrypted = encrypted;
        if s.code.as_str() != code.unwrap_or("") {
            s.code = code.unwrap_or("").to_string();
        }
    });
}

/// A server's report on one RTCP packet, when it speaks of our audio: the share lost, in percent.
pub fn loss_in_report(plain: &[u8], ours: u32) -> Option<f32> {
    let mut at = 0;
    while at + 8 <= plain.len() {
        let (count, kind) = ((plain[at] & 0x1f) as usize, plain[at + 1]);
        let length = (u16::from_be_bytes([plain[at + 2], plain[at + 3]]) as usize + 1) * 4;
        let first = match kind {
            200 => Some(at + 28),
            201 => Some(at + 8),
            _ => None,
        };
        if let Some(first) = first {
            for n in 0..count {
                let block = first + n * 24;
                let Some(bytes) = plain.get(block..block + 5) else { break };
                if u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) == ours {
                    return Some(bytes[4] as f32 / 256.0 * 100.0);
                }
            }
        }
        at += length;
    }
    None
}

pub fn note_loss(percent: f32) {
    with(|s| s.loss = Some(percent));
}

/// The call is over.
pub fn reset() {
    songbird::driver::video::LAST_PING_MS.store(0, Ordering::Relaxed);
    *STATE.lock().unwrap() = None;
}

/// What the window shows.
#[derive(Default, Debug, PartialEq)]
pub struct Shown {
    pub last: u32,
    pub average: u32,
    pub path: String,
    pub top: u32,
    pub start: String,
    pub end: String,
    pub server: String,
    pub loss: Option<f32>,
    pub encrypted: bool,
    pub code: String,
}

/// The highest line of the chart: a round number over the highest delay, at least 100.
fn chart_top(samples: &[u32]) -> u32 {
    let highest = samples.iter().copied().max().unwrap_or(0);
    (highest.saturating_mul(5) / 4 / 50 + 1).max(2) * 50
}

/// The delays as a line, oldest on the left.
fn chart_path(samples: &[u32], top: u32) -> String {
    let point = |at: usize, value: u32| {
        let x = if samples.len() < 2 { 0.0 } else { at as f32 / (samples.len() - 1) as f32 * CHART_W };
        let y = CHART_H * (1.0 - value.min(top) as f32 / top.max(1) as f32);
        format!("{x:.1} {y:.1}")
    };
    let mut path = String::new();
    for (at, value) in samples.iter().enumerate() {
        path.push_str(if at == 0 { "M" } else { " L" });
        path.push_str(&point(at, *value));
    }
    path
}

/// Takes in the newest delay and says how things stand.
pub fn tick() -> Shown {
    let last = songbird::driver::video::LAST_PING_MS.load(Ordering::Relaxed);
    let now = Instant::now();
    with(|s| {
        if last > 0 && s.sampled_at.is_none_or(|at| now - at >= SAMPLE_EVERY) {
            s.sampled_at = Some(now);
            s.samples.push_back(last);
            while s.samples.len() > MAX_SAMPLES {
                s.samples.pop_front();
            }
        }
        let samples: Vec<u32> = s.samples.iter().copied().collect();
        let average = if samples.is_empty() { 0 } else { samples.iter().sum::<u32>() / samples.len() as u32 };
        let top = chart_top(&samples);
        let clock_now = Local::now();
        let back = chrono::Duration::seconds((samples.len().saturating_sub(1) as u64 * SAMPLE_EVERY.as_secs()) as i64);
        Shown {
            last,
            average,
            path: chart_path(&samples, top),
            top,
            start: (clock_now - back).format("%H:%M").to_string(),
            end: clock_now.format("%H:%M").to_string(),
            server: s.server.clone(),
            loss: s.loss,
            encrypted: s.encrypted,
            code: s.code.clone(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_is_named_without_its_address() {
        assert_eq!(server_name("c-ord12-04bb0383.discord.media:443"), "c-ord12-04bb0383");
        assert_eq!(server_name("wss://c-ams1.discord.media/?v=8"), "c-ams1");
        assert_eq!(server_name(""), "");
    }

    #[test]
    fn the_chart_keeps_its_line_inside() {
        assert_eq!(chart_top(&[]), 100);
        assert_eq!(chart_top(&[60]), 100);
        assert_eq!(chart_top(&[120]), 200);
        assert_eq!(chart_path(&[], 100), "");
        assert_eq!(chart_path(&[0, 100], 100), "M0.0 80.0 L300.0 0.0");
        assert_eq!(chart_path(&[500], 100), "M0.0 0.0", "over the top stays at the top");
    }

    #[test]
    fn a_report_is_read_only_for_our_audio() {
        let block = |ssrc: u32, lost: u8| {
            let mut b = ssrc.to_be_bytes().to_vec();
            b.push(lost);
            b.extend_from_slice(&[0; 19]);
            b
        };
        // A receiver report with two blocks.
        let mut rr = vec![0x82, 201, 0, 12, 0, 0, 0, 9];
        rr.extend(block(5, 0));
        rr.extend(block(7, 64));
        assert_eq!(loss_in_report(&rr, 7), Some(25.0));
        assert_eq!(loss_in_report(&rr, 8), None);
        // A sender report before it: its block starts later.
        let mut both = vec![0x80, 200, 0, 6];
        both.extend_from_slice(&[0; 24]);
        both.extend(rr);
        assert_eq!(loss_in_report(&both, 7), Some(25.0));
        // Cut short: no panic, nothing read.
        assert_eq!(loss_in_report(&[0x81, 201, 0, 7, 0, 0], 7), None);
    }
}
