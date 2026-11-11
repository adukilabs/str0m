//! BWE recovery after a bandwidth dip when the sender follows the estimate (an SFU choosing layers).
//!
//! After the dip the sender keeps to its lowest layer, so the acked rate (and the 1.5x cap on an increase) stays low and
//! the sender is not application limited enough for ALR: only a probe can find that the link is back
//! (`RtcConfig::set_bwe_wanted_probes`).

use std::time::Duration;

use netem::{DataSize, NetemConfig};
use str0m::RtcError;
use str0m::bwe::Bitrate;

use crate::common::{BweTestContext, Step, connect_with_bwe_wanted, init_crypto_default, init_log};

/// A screen share's temporal layers: what each costs when all below it go too.
static LAYERS: [Bitrate; 3] = [Bitrate::kbps(40), Bitrate::kbps(90), Bitrate::kbps(1_200)];

fn link(rate: Bitrate) -> NetemConfig {
    NetemConfig::new()
        .latency(Duration::from_millis(20))
        .link(rate, DataSize::kbytes(20))
        .seed(42)
}

#[test]
pub fn bwe_recovers_after_dip_with_wanted_probes() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    // Without wanted probes the estimate stayed at ~150 kb/s in about half the runs (the deadlock).
    let plan = vec![
        Step::Conditions {
            description: "2 Mb/s",
            config: link(Bitrate::mbps(2)),
        },
        Step::Layers {
            description: "Layered sender wanting its top layer",
            desired_bitrate: Bitrate::kbps(1_500),
            layers: &LAYERS,
        },
        Step::Run {
            description: "Reach the top layer",
            duration: Duration::from_secs(10),
        },
        Step::Check {
            description: "Top layer fits",
            at_least: Bitrate::kbps(1_200),
        },
        Step::Conditions {
            description: "Dip to 300 kb/s",
            config: link(Bitrate::kbps(300)),
        },
        Step::Run {
            description: "Hold the dip",
            duration: Duration::from_secs(10),
        },
        Step::CheckBelow {
            description: "Below the top layer during the dip",
            below: Bitrate::kbps(1_200),
        },
        Step::Conditions {
            description: "Back to 2 Mb/s",
            config: link(Bitrate::mbps(2)),
        },
        Step::Run {
            description: "A few seconds to recover",
            duration: Duration::from_secs(5),
        },
        Step::Check {
            description: "Top layer fits again",
            at_least: Bitrate::kbps(1_200),
        },
    ];
    let (mut l, mut r) = connect_with_bwe_wanted(Bitrate::kbps(1_500), Bitrate::kbps(1_500), true);
    let mut ctx = BweTestContext::new(&mut l, &mut r);
    ctx.run_plan(&mut l, &mut r, &plan)
}
