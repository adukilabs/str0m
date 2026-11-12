//! A screen share forwarded by an SFU: a low idle rate, and now and then one large frame (a slide change, a keyframe)
//! written to the pacer at once. The sender cannot make the frame smaller, so how the pacer drains it and how the
//! estimate reacts to the queue it builds decide whether the viewer sees the next slide in a fraction of a second, or
//! the estimate collapses and the viewer freezes.
//!
//! Every run prints one line (`slides:`) with the estimate and frame delivery times, for comparing settings.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use netem::{DataSize, NetemConfig};
use str0m::bwe::Bitrate;
use str0m::format::Codec;
use str0m::media::MediaKind;
use str0m::rtp::{RawPacket, RtpWrite, Ssrc};
use str0m::{Rtc, RtcError};

use crate::common::{
    TestRtc, connect_l_r_with_rtc, get_last_bwe_estimate, init_crypto_default, init_log, progress,
};

const PACKET: usize = 1150;

/// A link with a queue that holds `queue` of its own time (300 ms in the L2 run), 20 ms each way.
fn link(rate: Bitrate, queue: Duration) -> NetemConfig {
    let bytes = (rate.as_f64() / 8.0 * queue.as_secs_f64()) as i64;
    NetemConfig::new()
        .latency(Duration::from_millis(20))
        .link(rate, DataSize::bytes(bytes))
        .seed(42)
}

#[derive(Clone, Copy)]
struct Settings {
    /// `RtcConfig::set_pacer_queue_limit`
    drain: Duration,
    /// `RtcConfig::set_bwe_delivery_floor`
    floor: bool,
    /// `RtcConfig::set_bwe_wanted_probes`
    wanted: bool,
    /// The RTX ratio cap of the send stream (aduki voice: 0.3 for video)
    ratio: Option<f32>,
}

#[derive(Debug)]
struct Outcome {
    /// Lowest and mean estimate from `settle` on.
    lowest: Bitrate,
    mean: Bitrate,
    /// Frame delivery: from written to its last packet received (first copy), per frame.
    times: Vec<Option<Duration>>,
    /// Packets of each frame never received.
    missing: Vec<usize>,
}

impl Outcome {
    fn worst(&self) -> Option<Duration> {
        self.times
            .iter()
            .try_fold(Duration::ZERO, |worst, t| t.map(|t| worst.max(t)))
    }

    fn median(&self) -> Option<Duration> {
        let mut all: Vec<_> = self.times.iter().flatten().copied().collect();
        all.sort();
        all.get(all.len() / 2).copied()
    }
}

/// Idle at `idle`, a frame of `frame` bytes every `every`, for `run` over `rate` with a 300 ms queue.
fn slides(
    settings: Settings,
    rate: Bitrate,
    idle: Bitrate,
    frame: usize,
    every: Duration,
    run: Duration,
    settle: Duration,
) -> Result<Outcome, RtcError> {
    let start = Instant::now();
    let rtc1 = Rtc::builder()
        .set_rtp_mode(true)
        .enable_bwe(Some(Bitrate::kbps(1_500)))
        .set_pacer_queue_limit(settings.drain)
        .set_bwe_wanted_probes(settings.wanted)
        .set_bwe_delivery_floor(settings.floor)
        .build(start);
    let rtc2 = Rtc::builder()
        .set_rtp_mode(true)
        .enable_raw_packets(true)
        .build(start);
    let (mut l, mut r) = connect_l_r_with_rtc(rtc1, rtc2);
    // an SFU wants a share's top rate (aduki voice `CEILING`)
    l.bwe().set_desired_bitrate(Bitrate::kbps(2_500));
    l.set_forced_time_advance(Duration::from_micros(100));
    r.set_forced_time_advance(Duration::from_micros(100));
    let normalized = l.start + Duration::from_millis(100);
    l.last = normalized;
    r.last = normalized;

    let mid = "vid".into();
    let ssrc_tx: Ssrc = 42.into();
    let ssrc_rtx: Ssrc = 44.into();
    l.direct_api().declare_media(mid, MediaKind::Video);
    l.direct_api()
        .declare_stream_tx(ssrc_tx, Some(ssrc_rtx), mid, None);
    r.direct_api().declare_media(mid, MediaKind::Video);
    r.direct_api()
        .expect_stream_rx(ssrc_tx, Some(ssrc_rtx), mid, None);
    r.direct_api().enable_twcc_feedback();
    // resends as aduki voice keeps them for video
    l.direct_api().stream_tx(&ssrc_tx).unwrap().set_rtx_cache(
        512,
        Duration::from_secs(1),
        settings.ratio,
    );
    let max = l.last.max(r.last);
    l.last = max;
    r.last = max;
    let params = l.params_vp8();
    assert_eq!(params.spec().codec, Codec::Vp8);
    let pt = params.pt();
    let resend = params.resend().expect("RTX");

    r.set_netem(link(rate, Duration::from_millis(300)));
    l.set_netem(link(Bitrate::mbps(10), Duration::from_millis(300)));

    let began = l.duration();
    let mut seq: u64 = 47_000;
    let mut budget = 0.0f64;
    let mut last = began;
    let mut next_frame = began + Duration::from_secs(2);
    // seq -> frame index; frame index -> (written at, packets)
    let mut owner: HashMap<u16, usize> = HashMap::new();
    let mut frames: Vec<(Duration, usize)> = Vec::new();
    let mut estimates: Vec<Bitrate> = Vec::new();
    let mut sampled = began;

    let write = |l: &mut TestRtc, seq: &mut u64| {
        let wallclock = l.start + l.duration();
        let time = (*seq * 1000 + 47_000_000) as u32;
        let mut direct = l.direct_api();
        let stream = direct.stream_tx(&ssrc_tx).unwrap();
        stream.write_rtp(
            RtpWrite::new(pt, (*seq).into(), time, wallclock, vec![0u8; PACKET]).nackable(true),
        );
        *seq += 1;
    };

    while l.duration() < began + run {
        let now = l.duration();
        budget += idle.as_f64() / 8.0 * now.saturating_sub(last).as_secs_f64();
        last = now;
        while budget >= PACKET as f64 {
            write(&mut l, &mut seq);
            budget -= PACKET as f64;
        }
        if now >= next_frame {
            next_frame += every;
            let index = frames.len();
            let packets = frame.div_ceil(PACKET);
            for _ in 0..packets {
                owner.insert(seq as u16, index);
                write(&mut l, &mut seq);
            }
            frames.push((now, packets));
        }
        progress(&mut l, &mut r)?;
        if now >= began + settle && now >= sampled + Duration::from_millis(100) {
            sampled = now;
            if let Some(e) = get_last_bwe_estimate(&l) {
                estimates.push(e);
            }
        }
    }

    let mut got: Vec<(usize, Option<Duration>)> = frames.iter().map(|(_, n)| (*n, None)).collect();
    let mut seen = std::collections::HashSet::new();
    // first arrival of each frame packet, sent or resent (RTX carries the original number first)
    for (at, event) in &r.events {
        let Some(RawPacket::RtpRx(header, body)) = event.as_raw_packet() else {
            continue;
        };
        let s = if header.payload_type == resend {
            let Some(osn) = body.get(0..2) else {
                continue;
            };
            u16::from_be_bytes([osn[0], osn[1]])
        } else {
            header.sequence_number
        };
        let Some(&index) = owner.get(&s) else {
            continue;
        };
        if !seen.insert(s) {
            continue;
        }
        let at = at.saturating_duration_since(l.start);
        let (left, done) = &mut got[index];
        *left -= 1;
        if *left == 0 {
            *done = Some(at);
        }
    }
    let times = got
        .iter()
        .zip(&frames)
        .map(|((_, done), (written, _))| done.map(|d| d.saturating_sub(*written)))
        .collect();
    let lowest = estimates
        .iter()
        .copied()
        .fold(Bitrate::INFINITY, |a, b| if b < a { b } else { a });
    let mean = Bitrate::from(
        estimates.iter().map(|e| e.as_f64()).sum::<f64>() / estimates.len().max(1) as f64,
    );
    let missing = got.iter().map(|(left, _)| *left).collect();
    Ok(Outcome {
        lowest,
        mean,
        times,
        missing,
    })
}

fn report(name: &str, settings: Settings, o: &Outcome) {
    let times: Vec<String> = o
        .times
        .iter()
        .map(|t| t.map_or("lost".into(), |t| format!("{}", t.as_millis())))
        .collect();
    println!(
        "slides: {name} drain={}ms floor={} wanted={} ratio={:?} lowest={} mean={} median={:?} worst={:?} frames_ms=[{}] missing={:?}",
        settings.drain.as_millis(),
        settings.floor,
        settings.wanted,
        settings.ratio,
        o.lowest,
        o.mean,
        o.median().map(|d| d.as_millis()),
        o.worst().map(|d| d.as_millis()),
        times.join(","),
        o.missing
    );
}

/// What aduki voice sets: str0m's default drain (2 s), the delivery floor, wanted probes, video resends uncapped (the
/// pacer keeps them within the estimate; a 0.3 cap cleared the resends of a keyframe on a narrow link, so it never
/// arrived whole).
const SFU: Settings = Settings {
    drain: Duration::from_secs(2),
    floor: true,
    wanted: true,
    ratio: None,
};

/// Slide changes (100 kB) every 3 s on a 2 Mb/s link with a 300 ms queue: the estimate holds and every slide arrives
/// within about a second.
#[test]
pub fn slide_changes_keep_the_estimate() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();
    let o = slides(
        SFU,
        Bitrate::mbps(2),
        Bitrate::kbps(60),
        100_000,
        Duration::from_secs(3),
        Duration::from_secs(30),
        Duration::from_secs(5),
    )?;
    report("2mbps", SFU, &o);
    assert!(
        o.lowest >= Bitrate::kbps(1_000),
        "estimate fell to {}",
        o.lowest
    );
    let worst = o.worst().expect("every slide arrives");
    assert!(
        worst <= Duration::from_millis(1_200),
        "a slide took {worst:?}"
    );
    Ok(())
}

/// A 1080p keyframe (100 kB) every 6 s through 500 kb/s with a 300 ms queue: paced at the estimate it arrives whole
/// in about two seconds, every time.
#[test]
pub fn keyframes_through_a_narrow_link() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();
    let o = slides(
        SFU,
        Bitrate::kbps(500),
        Bitrate::kbps(60),
        100_000,
        Duration::from_secs(6),
        Duration::from_secs(30),
        Duration::from_secs(5),
    )?;
    report("500kbps", SFU, &o);
    let worst = o.worst().expect("every keyframe arrives");
    assert!(worst <= Duration::from_secs(3), "a keyframe took {worst:?}");
    Ok(())
}

/// Compares settings (run with `--ignored --nocapture`): the drain and the floor on both links.
#[test]
#[ignore]
pub fn slides_compare() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();
    let only = std::env::var("SLIDES").ok();
    for drain in [100, 1_000, 2_000] {
        for floor in [false, true] {
            for wanted in [true, false] {
                for ratio in [Some(0.3), None] {
                    let s = Settings {
                        drain: Duration::from_millis(drain),
                        floor,
                        wanted,
                        ratio,
                    };
                    for (name, rate, every) in [
                        ("2mbps", Bitrate::mbps(2), Duration::from_secs(3)),
                        ("500kbps", Bitrate::kbps(500), Duration::from_secs(6)),
                    ] {
                        if only.as_deref().is_some_and(|o| o != name) {
                            continue;
                        }
                        let o = slides(
                            s,
                            rate,
                            Bitrate::kbps(60),
                            100_000,
                            every,
                            Duration::from_secs(30),
                            Duration::from_secs(5),
                        )?;
                        report(name, s, &o);
                    }
                }
            }
        }
    }
    Ok(())
}
