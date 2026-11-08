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
    // The first packet is a PAT: look for two packets 188 bytes apart, and only then
    // (a segment shorter than two packets) for a single sync byte.
    let head = segment.len().min(PACKET);
    let start = (0..head)
        .find(|&i| segment[i] == 0x47 && segment.get(i + PACKET) == Some(&0x47))
        .or_else(|| (0..head).find(|&i| segment[i] == 0x47))
        .ok_or("no transport packets (0x47) in the segment")?;
    let packets = (segment.len() - start) / PACKET;
    if packets == 0 {
        return Err("the segment is shorter than one transport packet".into());
    }
    let mut pmt_pid = None;
    let mut streams: Vec<(u16, u8)> = Vec::new();
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
            streams = parse_pmt(payload)?.unwrap_or_default();
        } else if let Some((audio_pid, _)) = audio_of(&streams)
            && pid == audio_pid
        {
            parse_pes(pusi, payload, &mut left, &mut out);
        }
    }
    match audio_of(&streams) {
        Some(_) => {
            if out.is_empty() {
                return Err("the TS has an audio stream but no audio packets".into());
            }
            Ok(out)
        }
        None => Err(format!("the TS carries {}, which is not supported", describe(&streams))),
    }
}

/// The first audio elementary stream (PID, stream type) of a PMT's streams.
fn audio_of(streams: &[(u16, u8)]) -> Option<(u16, u8)> {
    streams.iter().copied().find(|&(_, t)| matches!(t, 0x0F | 0x03 | 0x04))
}

/// The PMT's stream that explains a refusal best (a named codec if there is one).
fn describe(streams: &[(u16, u8)]) -> String {
    let known = |t: u8| matches!(t, 0x11 | 0x81 | 0x87 | 0x1B | 0x24);
    let stream_type = streams.iter().map(|&(_, t)| t).find(|&t| known(t)).or_else(|| streams.first().map(|&(_, t)| t));
    match stream_type {
        Some(0x11) => "AAC in LATM".to_string(),
        Some(0x81) => "AC-3".to_string(),
        Some(0x87) => "E-AC-3".to_string(),
        Some(0x1B) => "H.264 video".to_string(),
        Some(0x24) => "HEVC video".to_string(),
        Some(t) => format!("a stream of type {t:#04x}"),
        None => "no elementary streams".to_string(),
    }
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

/// The elementary streams (PID, stream type) a PMT section names, if it is there.
///
/// The PMT section is: table id, length, program, version, section numbers, PCR PID,
/// program info length, the program's descriptors, then the streams - so the streams
/// start at byte 12, after `program_info_length` bytes.
fn parse_pmt(payload: &[u8]) -> Result<Option<Vec<(u16, u8)>>, String> {
    let Some(section) = section(payload) else { return Ok(None) };
    if section.first() != Some(&0x02) || section.len() < 12 {
        return Ok(None);
    }
    let len = usize::from(section[1] & 0x0F) << 8 | usize::from(section[2]);
    // 9 bytes of the section header (program through the program info length) and a
    // 4-byte CRC at least.
    if len < 13 || section.len() < 3 + len - 4 {
        return Ok(None);
    }
    let end = 3 + len - 4;
    let program_info = (usize::from(section[10]) & 0x0F) << 8 | usize::from(section[11]);
    let mut i = 12 + program_info;
    let mut streams = Vec::new();
    while i + 5 <= end {
        let stream_type = section[i];
        let pid = (u16::from(section[i + 1] & 0x1F) << 8) | u16::from(section[i + 2]);
        let es_info = (usize::from(section[i + 3]) & 0x0F) << 8 | usize::from(section[i + 4]);
        streams.push((pid, stream_type));
        i += 5 + es_info;
    }
    Ok(Some(streams))
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
        // MPEG-2 PES (the audio streams of HLS muxers); an MPEG-1 PES header has no
        // header length byte, so a few bytes of audio are skipped and the framer
        // resynchronises.
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

    /// A real PMT: program, version, section numbers, PCR PID, program info length,
    /// then the streams (see ISO 13818-1 2.4.4.8).
    fn pmt(pcr: u16, program_info: &[u8], streams: &[(u16, u8)]) -> Vec<u8> {
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00]; // program, version, sections
        body.extend_from_slice(&[0xE0 | ((pcr >> 8) as u8), pcr as u8]);
        let info = program_info.len();
        body.extend_from_slice(&[0xF0 | ((info >> 8) as u8 & 0x0F), info as u8]);
        body.extend_from_slice(program_info);
        for &(pid, stream_type) in streams {
            body.extend_from_slice(&[stream_type, 0xE0 | ((pid >> 8) as u8), pid as u8, 0xF0, 0x00]);
        }
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
        segment.extend(pmt(0x0100, &[], &[(0x0101, 0x0F)]));
        segment.extend(packet(0x0101, true, &pes(&adts[..40], adts.len())));
        segment.extend(packet(0x0101, false, &adts[40..]));
        segment.extend(vec![0xFF; 7]); // a partial packet at the end is ignored
        let es = audio_es(&segment).unwrap();
        assert_eq!(es, adts);
    }

    #[test]
    fn skips_a_program_descriptor_and_a_video_stream() {
        let adts: Vec<u8> = (0..10).flat_map(|_| [0xFF, 0xF1, 0x00, 0x00, 0x00, 0x00]).collect();
        let mut segment = Vec::new();
        segment.extend(pat(0x1000));
        // Four descriptor bytes after the program info length; the video comes first.
        segment.extend(pmt(0x0100, &[0x05, 0x02, 0xAA, 0xBB], &[(0x0100, 0x1B), (0x0101, 0x0F)]));
        segment.extend(packet(0x0101, true, &pes(&adts, adts.len())));
        let es = audio_es(&segment).unwrap();
        assert_eq!(es, adts);
    }

    #[test]
    fn rejects_a_stream_without_audio() {
        let mut segment = Vec::new();
        segment.extend(pat(0x1000));
        segment.extend(pmt(0x0100, &[], &[(0x0101, 0x1B)])); // H.264: no audio
        segment.extend(packet(0x0101, true, &pes(&[0; 32], 32)));
        let e = audio_es(&segment).unwrap_err();
        assert!(e.contains("H.264"), "{e}");
    }

    #[test]
    fn names_latm_as_unsupported() {
        let mut segment = Vec::new();
        segment.extend(pat(0x1000));
        segment.extend(pmt(0x0100, &[], &[(0x0101, 0x11)]));
        segment.extend(packet(0x0101, true, &pes(&[0; 32], 32)));
        let e = audio_es(&segment).unwrap_err();
        assert!(e.contains("LATM"), "{e}");
    }

    #[test]
    fn names_ac3_as_unsupported() {
        let mut segment = Vec::new();
        segment.extend(pat(0x1000));
        segment.extend(pmt(0x0100, &[], &[(0x0101, 0x87)]));
        segment.extend(packet(0x0101, true, &pes(&[0; 32], 32)));
        let e = audio_es(&segment).unwrap_err();
        assert!(e.contains("E-AC-3"), "{e}");
    }
}
