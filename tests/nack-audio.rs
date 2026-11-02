use std::time::Duration;

use netem::{NetemConfig, Probability, RandomLoss};
use str0m::RtcError;
use str0m::format::Codec;
use str0m::media::MediaKind;
use str0m::rtp::rtcp::Rtcp;
use str0m::rtp::{RawPacket, RtpWrite, Ssrc};

mod common;
use common::{connect_l_r, init_crypto_default, init_log, progress};

/// Audio has no RTX: with `set_resend_in_place`, a NACKed packet goes out again on the main SSRC with its own
/// sequence number, as libwebrtc retransmits audio, and every lost packet arrives in the end.
#[test]
pub fn audio_resend_in_place() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let (mut l, mut r) = connect_l_r();

    let mid = "aud".into();
    let ssrc_tx: Ssrc = 42.into();

    l.direct_api().declare_media(mid, MediaKind::Audio);
    l.direct_api().declare_stream_tx(ssrc_tx, None, mid, None);
    r.direct_api().declare_media(mid, MediaKind::Audio);
    r.direct_api().expect_stream_rx(ssrc_tx, None, mid, None);
    // Without RTX the receiver suppresses NACK by default; ask for it.
    r.direct_api().stream_rx(&ssrc_tx).unwrap().suppress_nack(false);
    l.direct_api()
        .stream_tx(&ssrc_tx)
        .unwrap()
        .set_resend_in_place(true);

    let max = l.last.max(r.last);
    l.last = max;
    r.last = max;

    let params = l.params_opus();
    assert_eq!(params.spec().codec, Codec::Opus);
    assert!(params.resend().is_none(), "opus has no RTX");
    let pt = params.pt();

    let num_packets: usize = 500;
    for index in 0..num_packets {
        let wallclock = l.start + l.duration();
        let mut direct = l.direct_api();
        let stream = direct.stream_tx(&ssrc_tx).unwrap();
        let time = (index * 960 + 1_000_000) as u32;
        let seq_no = (10_000 + index as u64).into();
        stream.write_rtp(RtpWrite::new(pt, seq_no, time, wallclock, [1, 2, 3, 4]).nackable(true));

        // loss only in the middle, so the ends settle (see MISORDER_DELAY in register.rs)
        if index == 10 {
            r.set_netem(
                NetemConfig::new()
                    .loss(RandomLoss::new(Probability::new(0.05)))
                    .seed(7),
            );
        } else if index == num_packets - 10 {
            r.set_netem(NetemConfig::new());
        }
        progress(&mut l, &mut r)?;
    }

    let settle = l.duration() + Duration::from_secs(5);
    while l.duration() < settle {
        progress(&mut l, &mut r)?;
    }

    let nacks = l
        .events
        .iter()
        .filter(|(_, e)| matches!(e.as_raw_packet(), Some(RawPacket::RtcpRx(Rtcp::Nack(_)))))
        .count();
    assert!(nacks > 0, "the receiver sent NACKs");

    // every packet arrived, on the main SSRC and PT, with its own sequence number
    let mut seqs: Vec<u16> = r
        .events
        .iter()
        .filter_map(|(_, e)| match e.as_raw_packet() {
            Some(RawPacket::RtpRx(h, _)) => {
                assert_eq!(h.ssrc, ssrc_tx, "resent on the main SSRC");
                assert_eq!(h.payload_type, pt, "resent with the main PT");
                Some(h.sequence_number)
            }
            _ => None,
        })
        .collect();
    // a lost packet reaches the receiver once, as its resend; the test without the setting shows the loss
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.first(), Some(&10_000));
    assert_eq!(seqs.last(), Some(&(10_000 + num_packets as u16 - 1)));
    assert_eq!(seqs.len(), num_packets, "every lost packet came again");

    Ok(())
}

/// Without `set_resend_in_place`, audio NACKs are ignored as before (no RTX, nothing cached).
#[test]
pub fn audio_without_resend_in_place() -> Result<(), RtcError> {
    init_log();
    init_crypto_default();

    let (mut l, mut r) = connect_l_r();

    let mid = "aud".into();
    let ssrc_tx: Ssrc = 42.into();

    l.direct_api().declare_media(mid, MediaKind::Audio);
    l.direct_api().declare_stream_tx(ssrc_tx, None, mid, None);
    r.direct_api().declare_media(mid, MediaKind::Audio);
    r.direct_api().expect_stream_rx(ssrc_tx, None, mid, None);
    r.direct_api().stream_rx(&ssrc_tx).unwrap().suppress_nack(false);

    let max = l.last.max(r.last);
    l.last = max;
    r.last = max;

    let pt = l.params_opus().pt();
    let num_packets: usize = 300;
    for index in 0..num_packets {
        let wallclock = l.start + l.duration();
        let mut direct = l.direct_api();
        let stream = direct.stream_tx(&ssrc_tx).unwrap();
        let time = (index * 960 + 1_000_000) as u32;
        let seq_no = (10_000 + index as u64).into();
        stream.write_rtp(RtpWrite::new(pt, seq_no, time, wallclock, [1, 2, 3, 4]).nackable(true));
        if index == 10 {
            r.set_netem(
                NetemConfig::new()
                    .loss(RandomLoss::new(Probability::new(0.05)))
                    .seed(7),
            );
        } else if index == num_packets - 10 {
            r.set_netem(NetemConfig::new());
        }
        progress(&mut l, &mut r)?;
    }
    let settle = l.duration() + Duration::from_secs(3);
    while l.duration() < settle {
        progress(&mut l, &mut r)?;
    }

    let mut seqs: Vec<u16> = r
        .events
        .iter()
        .filter_map(|(_, e)| match e.as_raw_packet() {
            Some(RawPacket::RtpRx(h, _)) => Some(h.sequence_number),
            _ => None,
        })
        .collect();
    let sent = seqs.len();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(sent, seqs.len(), "nothing was sent twice");
    assert!(seqs.len() < num_packets, "losses stay lost");

    Ok(())
}
