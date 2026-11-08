//! MPEG-TS (ISO/IEC 13818-1) audio extraction for HLS segments.
//!
//! A TS segment carries small fixed packets of 188 bytes. The PAT (PID 0) names the
//! PMT, the PMT names the elementary streams, and the audio stream's PES packets carry
//! the audio elementary stream - for DRM radio streams ADTS AAC (stream type 0x0F) or
//! MPEG audio (0x03/0x04), which the web stream decoder already handles. This module
//! finds the audio PID and concatenates its PES payloads, nothing more: HLS segments
//! are short and self-contained, so the demuxer is stateless between them.

/// Size of a transport packet.
const PACKET: usize = 188;

/// The audio elementary stream of a TS segment (ADTS AAC or MPEG audio bytes).
pub(crate) fn audio_es(segment: &[u8]) -> Result<Vec<u8>, String> {
    let start = (0..segment.len().min(PACKET))
        .find(|&i| segment[i] == 0x47 && segment.len() <= i + PACKET || segment.get(i + PACKET) == Some(&0x47))
        .ok_or("no transport packets (0x47) in the segment")?;
    let packets = (segment.len() - start) / PACKET;
    if packets == 0 {
        return Err("the segment is shorter than one transport packet".into());
    }
    let mut pmt_pid = None;
    let mut audio: Option<(u16, u8)> = None;
    let mut left: Option<usize> = None;
    let mut out = Vec::with_capacity(segment.len());
    let (packets, _) = segment[start..start + packets * PACKET].as_chunks::<PACKET>();
    for packet in packets {
        if packet[0] != 0x47 {
            continue;
        }
        let pusi = packet[1] & 0x40 != 0;
        let pid = (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2]);
        let afc = (packet[3] >> 4) & 0x3;
        let mut offset = 4usize;
        if afc & 0x2 != 0 {
            // The adaptation field: its own length byte, then that many bytes.
            offset = 5 + usize::from(packet[4]);
        }
        if afc & 0x1 == 0 || offset >= PACKET {
            continue;
        }
        let payload = &packet[offset..];
        if pid == 0 && pusi {
            pmt_pid = parse_pat(payload);
        } else if Some(pid) == pmt_pid && pusi {
            audio = parse_pmt(payload)?;
        } else if let Some((audio_pid, _)) = audio
            && pid == audio_pid
        {
            parse_pes(pusi, payload, &mut left, &mut out);
        }
        if audio.is_none() && pmt_pid.is_none() {
            // Nothing found yet; keep scanning (a PAT may follow a null packet burst).
        }
    }
    let Some((_, stream_type)) = audio else {
        return Err("the TS carries no MPEG audio or AAC stream".into());
    };
    if stream_type == 0x11 {
        return Err("the TS carries AAC in LATM, which is not supported".into());
    }
    if out.is_empty() {
        return Err("the TS has an audio stream but no audio packets".into());
    }
    Ok(out)
}

/// The PMT PID named by a PAT section, if it is there.
fn parse_pat(payload: &[u8]) -> Option<u16> {
    let section = section(payload)?;
    if section.first() != Some(&0x00) {
        return None;
    }
    let len = usize::from(section.get(1)? & 0x0F) << 8 | usize::from(*section.get(2)?);
    if len < 9 || section.len() < 3 + len - 4 {
        return None;
    }
    // After the 8-byte section header, 4-byte entries of (program, PID).
    let mut i = 8;
    while i + 4 <= len + 3 - 4 {
        let program = u16::from_be_bytes([section[i], section[i + 1]]);
        let pid = (u16::from(section[i + 2] & 0x1F) << 8) | u16::from(section[i + 3]);
        if program != 0 {
            return Some(pid);
        }
        i += 4;
    }
    None
}

/// The audio stream (PID, stream type) named by a PMT section, if it is there.
fn parse_pmt(payload: &[u8]) -> Result<Option<(u16, u8)>, String> {
    let Some(section) = section(payload) else { return Ok(None) };
    if section.first() != Some(&0x02) {
        return Ok(None);
    }
    let len = usize::from(section.get(1).copied().unwrap_or(0) & 0x0F) << 8
        | usize::from(section.get(2).copied().unwrap_or(0));
    if len < 11 || section.len() < 3 + len - 4 {
        return Ok(None);
    }
    let end = 3 + len - 4;
    let program_info = (usize::from(section[8]) & 0x0F) << 8 | usize::from(section[9]);
    let mut i = 10 + program_info;
    let mut other = None;
    while i + 5 <= end {
        let stream_type = section[i];
        let pid = (u16::from(section[i + 1] & 0x1F) << 8) | u16::from(section[i + 2]);
        let es_info = usize::from(section[i + 3]) << 8 | usize::from(section[i + 4]);
        if matches!(stream_type, 0x0F | 0x03 | 0x04) {
            return Ok(Some((pid, stream_type)));
        }
        if stream_type == 0x11 {
            other = Some((pid, stream_type));
        }
        i += 5 + es_info;
    }
    Ok(other)
}

/// One table section of a payload whose pointer field and length are there.
fn section(payload: &[u8]) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    payload.get(1 + pointer..)
}

/// Append a PES packet's elementary payload; `pusi` marks the packet's start and
/// `left` counts the elementary bytes the PES header declared (for segments whose
/// last packet is padded with 0xFF).
fn parse_pes(pusi: bool, payload: &[u8], left: &mut Option<usize>, out: &mut Vec<u8>) {
    let es = if !pusi {
        payload
    } else {
        // 00 00 01 sid, packet length, '10' flags, header length, the PES header.
        if payload.len() < 9 || payload[..3] != [0x00, 0x00, 0x01] {
            // Not a PES start: a continuation of a stream without PES headers.
            payload
        } else {
            let packet_length = usize::from(payload[4]) << 8 | usize::from(payload[5]);
            let header = 9 + usize::from(payload[8]);
            *left = (packet_length != 0).then(|| packet_length.saturating_sub(3 + usize::from(payload[8])));
            &payload[header.min(payload.len())..]
        }
    };
    let take = match *left {
        Some(n) => {
            let take = es.len().min(n);
            *left = Some(n - take);
            take
        }
        None => es.len(),
    };
    out.extend_from_slice(&es[..take]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport packet with `payload` for `pid`; the rest of the packet is filled
    /// with an adaptation field of stuffing (payload padding would be audio to the
    /// demuxer when a PES declares its length).
    fn packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= PACKET - 4);
        let flags = (if pusi { 0x40u8 } else { 0 }) | (pid >> 8) as u8;
        let stuffing = PACKET - 4 - payload.len();
        let mut p = vec![0x47, flags, pid as u8, 0x30]; // adaptation field and payload
        p.push((stuffing - 1) as u8); // the flags byte, then stuffing bytes
        p.extend(std::iter::repeat_n(0xFF, stuffing - 1));
        p.extend_from_slice(payload);
        assert_eq!(p.len(), PACKET);
        p
    }

    /// One table section packet: pointer 0, then the section body (after the table id
    /// and length field, before the CRC).
    fn table(pid: u16, table_id: u8, body: &[u8]) -> Vec<u8> {
        let len = body.len() + 4; // the body and the CRC
        let mut payload = vec![0x00, table_id, 0xB0 | ((len >> 8) as u8 & 0x0F), len as u8];
        payload.extend_from_slice(body);
        payload.extend_from_slice(&[0, 0, 0, 0]);
        packet(pid, true, &payload)
    }

    fn pat(pmt: u16) -> Vec<u8> {
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00]; // tsid, version, sections
        body.extend_from_slice(&[0x00, 0x01, 0xE0 | ((pmt >> 8) as u8), pmt as u8]);
        table(0, 0x00, &body)
    }

    fn pmt(audio: u16, stream_type: u8) -> Vec<u8> {
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00]; // program, version, PCR PID 0
        body.extend_from_slice(&[0xF0, 0x00]); // program_info_length 0
        body.extend_from_slice(&[stream_type, 0xE0 | ((audio >> 8) as u8), audio as u8, 0xF0, 0x00]);
        table(0x1000, 0x02, &body)
    }

    /// A PES packet with `es` in this packet and the elementary stream `declared`
    /// bytes long (the rest follows in continuation packets).
    fn pes(es: &[u8], declared: usize) -> Vec<u8> {
        // The declared length covers the 2 flag bytes, the header length byte and the
        // elementary bytes: continuations stop exactly at the end (no 0xFF padding).
        let len = 3 + declared;
        let mut p = vec![0x00, 0x00, 0x01, 0xC0, (len >> 8) as u8, len as u8, 0x80, 0x00, 0x00];
        p.extend_from_slice(es);
        p
    }

    #[test]
    fn finds_the_audio_pes_payload() {
        let adts: Vec<u8> = (0..30).flat_map(|_| [0xFF, 0xF1, 0x00, 0x00, 0x00, 0x00]).collect();
        let mut segment = Vec::new();
        segment.extend(pat(0x1000));
        segment.extend(pmt(0x0101, 0x0F));
        segment.extend(packet(0x0101, true, &pes(&adts[..40], adts.len())));
        segment.extend(packet(0x0101, false, &adts[40..]));
        segment.extend(vec![0xFF; 7]); // a partial packet at the end is ignored
        let es = audio_es(&segment).unwrap();
        assert_eq!(es, adts);
    }

    #[test]
    fn rejects_a_stream_without_audio() {
        let mut segment = Vec::new();
        segment.extend(pat(0x1000));
        segment.extend(pmt(0x0101, 0x1B)); // H.264: no audio
        segment.extend(packet(0x0101, true, &pes(&[0; 32], 32)));
        let e = audio_es(&segment).unwrap_err();
        assert!(e.contains("no MPEG audio"), "{e}");
    }

    #[test]
    fn names_latm_as_unsupported() {
        let mut segment = Vec::new();
        segment.extend(pat(0x1000));
        segment.extend(pmt(0x0101, 0x11));
        segment.extend(packet(0x0101, true, &pes(&[0; 32], 32)));
        let e = audio_es(&segment).unwrap_err();
        assert!(e.contains("LATM"), "{e}");
    }
}
