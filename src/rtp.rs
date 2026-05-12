use std::net::{ToSocketAddrs, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::{EncodedFrame, EngineConfig, FRAME_MS, RTP_CLOCK_INCREMENT};

pub const RTP_HEADER_LEN: usize = 12;

pub fn build_rtp_packet(
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut packet = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
    packet.push(0x80);
    packet.push(payload_type & 0x7f);
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

pub fn frame_payloads_to_rtp(payloads: Vec<Vec<u8>>) -> Vec<EncodedFrame> {
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, payload)| EncodedFrame {
            sequence: index as u16,
            timestamp: (index as u32).wrapping_mul(RTP_CLOCK_INCREMENT),
            payload,
        })
        .collect()
}

pub fn send_rtp_frames<A: ToSocketAddrs>(
    addr: A,
    frames: &[EncodedFrame],
    config: &EngineConfig,
    realtime: bool,
) -> Result<usize> {
    let socket = UdpSocket::bind(("127.0.0.1", 0)).context("bind local RTP sender")?;
    let start = Instant::now();
    let frame_duration = Duration::from_millis(FRAME_MS as u64);
    let mut sent = 0;
    for (index, frame) in frames.iter().enumerate() {
        if realtime {
            let target = start + frame_duration * index as u32;
            let now = Instant::now();
            if target > now {
                thread::sleep(target - now);
            }
        }
        let packet = build_rtp_packet(
            config.payload_type,
            frame.sequence,
            frame.timestamp,
            config.ssrc,
            &frame.payload,
        );
        socket.send_to(&packet, &addr).context("send RTP packet")?;
        sent += 1;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_plain_rtp_header() {
        let packet = build_rtp_packet(120, 0x1234, 0x10203040, 0x55667788, &[1, 2, 3]);
        assert_eq!(
            &packet[..12],
            &[
                0x80, 120, 0x12, 0x34, 0x10, 0x20, 0x30, 0x40, 0x55, 0x66, 0x77, 0x88
            ]
        );
        assert_eq!(&packet[12..], &[1, 2, 3]);
    }

    #[test]
    fn assigns_discord_opus_clock_timestamps() {
        let frames = frame_payloads_to_rtp(vec![vec![1], vec![2], vec![3]]);
        assert_eq!(frames[0].sequence, 0);
        assert_eq!(frames[0].timestamp, 0);
        assert_eq!(frames[1].sequence, 1);
        assert_eq!(frames[1].timestamp, 960);
        assert_eq!(frames[2].sequence, 2);
        assert_eq!(frames[2].timestamp, 1920);
    }
}
