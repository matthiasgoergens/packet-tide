use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};

use crate::AnyResult;

pub(crate) const MIN_GENERATION_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_GENERATION_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const SYMBOL_BYTES: u16 = 1_168;
const PACKET_ID_BYTES: usize = 4;
const ALIGNMENT: u8 = 8;

pub(crate) struct EncodedGeneration {
    pub(crate) oti: [u8; 12],
    pub(crate) source_packets: usize,
    pub(crate) packets: Vec<Vec<u8>>,
}

pub(crate) fn encode_generation(data: &[u8], repair_packets: u32) -> AnyResult<EncodedGeneration> {
    validate_generation_size(data.len())?;
    let oti = ObjectTransmissionInformation::new(data.len() as u64, SYMBOL_BYTES, 1, 1, ALIGNMENT);
    let encoder = Encoder::new(data, oti);
    let block = encoder
        .get_block_encoders()
        .first()
        .ok_or("RaptorQ encoder did not produce a source block")?;
    let sources = block.source_packets();
    let source_packets = sources.len();
    let total = source_packets
        .checked_add(repair_packets as usize)
        .ok_or("RaptorQ packet count overflow")?;
    let mut packets = Vec::with_capacity(total);
    packets.extend(sources.into_iter().map(|packet| packet.serialize()));
    packets.extend(
        block
            .repair_packets(0, repair_packets)
            .into_iter()
            .map(|packet| packet.serialize()),
    );
    if packets
        .iter()
        .any(|packet| packet.len() > PACKET_ID_BYTES + usize::from(SYMBOL_BYTES))
    {
        return Err("RaptorQ packet exceeds the authenticated UDP payload budget".into());
    }
    Ok(EncodedGeneration {
        oti: oti.serialize(),
        source_packets,
        packets,
    })
}

pub(crate) fn decode_generation(
    oti_bytes: [u8; 12],
    packets: impl IntoIterator<Item = Vec<u8>>,
) -> AnyResult<Option<Vec<u8>>> {
    let oti = ObjectTransmissionInformation::deserialize(&oti_bytes);
    validate_oti(oti)?;
    let mut decoder = Decoder::new(oti);
    for serialized in packets {
        if serialized.len() < PACKET_ID_BYTES
            || serialized.len() > PACKET_ID_BYTES + usize::from(SYMBOL_BYTES)
        {
            return Err("invalid bounded RaptorQ packet length".into());
        }
        let packet = EncodingPacket::deserialize(&serialized);
        if packet.payload_id().source_block_number() != 0 {
            return Err("RaptorQ packet names an unexpected source block".into());
        }
        if let Some(result) = decoder.decode(packet) {
            return Ok(Some(result));
        }
    }
    Ok(decoder.get_result())
}

fn validate_generation_size(bytes: usize) -> AnyResult<()> {
    if bytes == 0 || bytes > MAX_GENERATION_BYTES {
        return Err(format!(
            "RaptorQ generation must contain 1 through {MAX_GENERATION_BYTES} bytes"
        )
        .into());
    }
    Ok(())
}

fn validate_oti(oti: ObjectTransmissionInformation) -> AnyResult<()> {
    let bytes = usize::try_from(oti.transfer_length())?;
    validate_generation_size(bytes)?;
    if oti.symbol_size() != SYMBOL_BYTES
        || oti.source_blocks() != 1
        || oti.sub_blocks() != 1
        || oti.symbol_alignment() != ALIGNMENT
    {
        return Err(
            "RaptorQ transmission parameters are outside Packet Tide's bounded profile".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_bytes(length: usize) -> Vec<u8> {
        let mut state = 0xd1b5_4a32_d192_ed03_u64;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn systematic_packets_decode_without_repairs() {
        let data = deterministic_bytes(256 * 1024 + 17);
        let encoded = encode_generation(&data, 0).unwrap();
        assert_eq!(encoded.packets.len(), encoded.source_packets);
        assert_eq!(
            decode_generation(encoded.oti, encoded.packets).unwrap(),
            Some(data)
        );
    }

    #[test]
    fn repairs_cover_loss_duplication_and_reordering() {
        let data = deterministic_bytes(512 * 1024 + 31);
        let encoded = encode_generation(&data, 80).unwrap();
        let mut delivered: Vec<_> = encoded
            .packets
            .into_iter()
            .enumerate()
            .filter(|(index, _)| *index >= encoded.source_packets || index % 10 != 0)
            .map(|(_, packet)| packet)
            .collect();
        delivered.reverse();
        let duplicates: Vec<_> = delivered.iter().take(8).cloned().collect();
        delivered.extend(duplicates);
        assert_eq!(
            decode_generation(encoded.oti, delivered).unwrap(),
            Some(data)
        );
    }

    #[test]
    fn insufficient_symbols_do_not_claim_success() {
        let data = deterministic_bytes(128 * 1024);
        let encoded = encode_generation(&data, 0).unwrap();
        let delivered: Vec<_> = encoded
            .packets
            .into_iter()
            .enumerate()
            .filter(|(index, _)| index % 3 != 0)
            .map(|(_, packet)| packet)
            .collect();
        assert_eq!(decode_generation(encoded.oti, delivered).unwrap(), None);
    }

    #[test]
    fn profile_rejects_empty_oversized_and_unbounded_parameters() {
        assert!(encode_generation(&[], 0).is_err());
        assert!(validate_generation_size(MAX_GENERATION_BYTES + 1).is_err());
        let wrong = ObjectTransmissionInformation::new(1024, 1024, 1, 1, ALIGNMENT);
        assert!(decode_generation(wrong.serialize(), Vec::new()).is_err());
        assert!(
            decode_generation(
                ObjectTransmissionInformation::new(1024, SYMBOL_BYTES, 1, 1, ALIGNMENT).serialize(),
                vec![vec![0; PACKET_ID_BYTES + usize::from(SYMBOL_BYTES) + 1]],
            )
            .is_err()
        );
    }

    #[test]
    fn configured_generation_window_is_four_to_thirty_two_mib() {
        assert_eq!(MIN_GENERATION_BYTES, 4 * 1024 * 1024);
        assert_eq!(MAX_GENERATION_BYTES, 32 * 1024 * 1024);
        assert_eq!(usize::from(SYMBOL_BYTES) % usize::from(ALIGNMENT), 0);
    }
}
