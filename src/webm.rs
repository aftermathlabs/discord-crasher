use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::collections::HashMap;

const ID_SEGMENT: u32 = 0x18_53_80_67;
const ID_SEEK_HEAD: u32 = 0x11_4D_9B_74;
const ID_INFO: u32 = 0x15_49_A9_66;
const ID_TIMECODE_SCALE: u32 = 0x2A_D7_B1;
const ID_TRACKS: u32 = 0x16_54_AE_6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63_A2;
const ID_CODEC_DELAY: u32 = 0x56_AA;
const ID_AUDIO: u32 = 0xE1;
const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
const ID_CLUSTER: u32 = 0x1F_43_B6_75;
const ID_CLUSTERTIMECODE: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;
const ID_BLOCK_DURATION: u32 = 0x9B;
const ID_DISCARD_PADDING: u32 = 0x75_A2;
const ID_CRC32: u32 = 0xBF;
const ID_CUES: u32 = 0x1C_53_BB_6B;
const ID_VOID: u32 = 0xEC;

#[derive(Debug, Clone)]
struct Element {
    start: usize,
    id: u32,
    id_bytes: Vec<u8>,
    id_width: usize,
    size_width: usize,
    payload_start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct TrackInfo {
    number: u64,
    codec_id: String,
    codec_delay_frames: u64,
    sample_rate: f64,
    vorbis: VorbisConfig,
}

#[derive(Debug, Clone)]
struct VorbisConfig {
    mode_block_sizes: Vec<u64>,
}

#[derive(Debug, Clone)]
struct AudioBlock {
    index: usize,
    outer: Element,
    timestamp_ticks: i64,
    duration_ticks: Option<u64>,
    packet: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct WebmOptions {
    pub trigger_skip: u64,
    pub codec_delay_frames: Option<u64>,
    pub sample_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DualTriggerPlan {
    carrier_frames: u64,
    trigger_frames: u64,
    shared_skip_frames: u64,
    carried_frames: u64,
}

fn dual_trigger_plan(
    frame_counts: &[Option<u64>],
    codec_delay: u64,
    trigger_skip: u64,
) -> Result<DualTriggerPlan> {
    ensure!(
        frame_counts.len() >= 3,
        "need at least three Vorbis audio blocks"
    );
    ensure!(trigger_skip > 0, "--trigger-skip must be positive");

    let carrier_frames =
        frame_counts[1].context("cannot determine decoded frame count for Vorbis packet 1")?;
    let trigger_frames =
        frame_counts[2].context("cannot determine decoded frame count for Vorbis packet 2")?;
    ensure!(
        carrier_frames > codec_delay,
        "Vorbis packet 1 emits {carrier_frames} frames, which does not exceed the {codec_delay}-frame codec delay"
    );

    // Packet 0 is Vorbis priming and normally emits no PCM. Old Chromium saves
    // its padding for packet 1, while current Chromium drops it. Repeating the
    // same large skip on packet 1 makes both paths carry codec_delay + 1 frames
    // into packet 2, where any positive front skip reaches the fatal CHECK.
    let shared_skip_frames = carrier_frames
        .checked_add(1)
        .context("shared skip overflows u64")?;
    let usable_carrier_frames = carrier_frames - codec_delay;
    let carried_frames = shared_skip_frames - usable_carrier_frames;
    ensure!(
        carried_frames > codec_delay,
        "dual trigger does not exceed the codec delay"
    );
    ensure!(
        trigger_frames > carried_frames,
        "Vorbis packet 2 emits {trigger_frames} frames, but must exceed the {carried_frames}-frame carry"
    );

    Ok(DualTriggerPlan {
        carrier_frames,
        trigger_frames,
        shared_skip_frames,
        carried_frames,
    })
}

fn vint(data: &[u8], offset: usize, element_id: bool) -> Result<(u64, usize, bool)> {
    ensure!(offset < data.len(), "truncated EBML VINT at {offset:#x}");
    let first = data[offset];
    let Some(width) = (1..=8).find(|width| first & (0x80 >> (width - 1)) != 0) else {
        bail!("invalid EBML VINT at {offset:#x}");
    };
    ensure!(
        offset + width <= data.len(),
        "truncated EBML VINT at {offset:#x}"
    );
    let mut raw = 0_u64;
    for byte in &data[offset..offset + width] {
        raw = (raw << 8) | u64::from(*byte);
    }
    if element_id {
        ensure!(width <= 4, "EBML element ID is wider than four bytes");
        return Ok((raw, width, false));
    }
    let marker = 1_u64 << (7 * width);
    let value = raw & (marker - 1);
    Ok((value, width, value == marker - 1))
}

fn parse_elements(data: &[u8], start: usize, end: usize) -> Result<Vec<Element>> {
    ensure!(start <= end && end <= data.len(), "invalid EBML range");
    let mut result = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let element_start = cursor;
        let (id, id_width, _) = vint(data, cursor, true)?;
        cursor += id_width;
        let (size, size_width, unknown_size) = vint(data, cursor, false)?;
        cursor += size_width;
        let payload_start = cursor;
        let element_end = if unknown_size {
            end
        } else {
            payload_start
                .checked_add(usize::try_from(size).context("EBML element is too large")?)
                .context("EBML element end overflow")?
        };
        ensure!(element_end <= end, "EBML element extends past its parent");
        result.push(Element {
            start: element_start,
            id: u32::try_from(id).context("EBML element ID does not fit in u32")?,
            id_bytes: data[element_start..element_start + id_width].to_vec(),
            id_width,
            size_width,
            payload_start,
            end: element_end,
        });
        cursor = element_end;
        if unknown_size {
            break;
        }
    }
    ensure!(cursor == end, "unparsed EBML bytes at {cursor:#x}");
    Ok(result)
}

fn encode_size(value: usize, width: usize) -> Result<Vec<u8>> {
    ensure!((1..=8).contains(&width), "invalid EBML size width");
    let marker = 1_u64 << (7 * width);
    let value = u64::try_from(value).context("EBML size does not fit in u64")?;
    ensure!(
        value < marker - 1,
        "EBML payload does not fit its size width"
    );
    let mut raw = marker | value;
    let mut result = vec![0_u8; width];
    for byte in result.iter_mut().rev() {
        *byte = raw as u8;
        raw >>= 8;
    }
    Ok(result)
}

fn minimal_size_width(value: usize) -> Result<usize> {
    for width in 1..=8 {
        let marker = 1_u64 << (7 * width);
        if u64::try_from(value).unwrap_or(u64::MAX) < marker - 1 {
            return Ok(width);
        }
    }
    bail!("EBML payload is too large")
}

fn make_element(id: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    let width = minimal_size_width(payload.len())?;
    let mut result = Vec::with_capacity(id.len() + width + payload.len());
    result.extend_from_slice(id);
    result.extend_from_slice(&encode_size(payload.len(), width)?);
    result.extend_from_slice(payload);
    Ok(result)
}

fn rebuild_element(data: &[u8], element: &Element, payload: &[u8]) -> Result<Vec<u8>> {
    let size = match encode_size(payload.len(), element.size_width) {
        Ok(size) => size,
        Err(_) => encode_size(payload.len(), minimal_size_width(payload.len())?)?,
    };
    let mut result = Vec::with_capacity(element.id_width + size.len() + payload.len());
    result.extend_from_slice(&element.id_bytes);
    result.extend_from_slice(&size);
    result.extend_from_slice(payload);
    let _ = data;
    Ok(result)
}

fn uint_payload(data: &[u8], element: &Element) -> Result<u64> {
    ensure!(
        element.end >= element.payload_start,
        "invalid EBML integer element"
    );
    let payload = &data[element.payload_start..element.end];
    ensure!(payload.len() <= 8, "EBML integer is wider than eight bytes");
    let mut value = 0_u64;
    for byte in payload {
        value = (value << 8) | u64::from(*byte);
    }
    Ok(value)
}

fn float_payload(data: &[u8], element: &Element) -> Result<f64> {
    let payload = &data[element.payload_start..element.end];
    match payload.len() {
        4 => Ok(f64::from(f32::from_bits(u32::from_be_bytes(
            payload.try_into().unwrap(),
        )))),
        8 => Ok(f64::from_bits(u64::from_be_bytes(
            payload.try_into().unwrap(),
        ))),
        _ => bail!(
            "SamplingFrequency has {} bytes, expected four or eight",
            payload.len()
        ),
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read(&mut self, count: usize) -> Result<u64> {
        ensure!(count <= 64, "Vorbis bit field is wider than 64 bits");
        ensure!(
            self.bit.saturating_add(count) <= self.data.len().saturating_mul(8),
            "truncated Vorbis setup packet"
        );
        let mut value = 0_u64;
        for index in 0..count {
            let bit = (self.data[(self.bit + index) / 8] >> ((self.bit + index) % 8)) & 1;
            value |= u64::from(bit) << index;
        }
        self.bit += count;
        Ok(value)
    }

    fn skip(&mut self, count: usize) -> Result<()> {
        let mut remaining = count;
        while remaining != 0 {
            let chunk = remaining.min(64);
            let _ = self.read(chunk)?;
            remaining -= chunk;
        }
        Ok(())
    }
}

fn ilog(value: u64) -> usize {
    if value == 0 {
        0
    } else {
        (u64::BITS - value.leading_zeros()) as usize
    }
}

fn read_vorbis_codebook(reader: &mut BitReader<'_>) -> Result<()> {
    ensure!(
        reader.read(24)? == 0x56_43_42,
        "invalid Vorbis codebook sync"
    );
    let dimensions = usize::try_from(reader.read(16)?).unwrap();
    let entries = usize::try_from(reader.read(24)?).unwrap();
    let ordered = reader.read(1)? != 0;
    if ordered {
        let mut current = 0_usize;
        let mut length = usize::try_from(reader.read(5)?).unwrap() + 1;
        while current < entries {
            let bits = ilog(u64::try_from(entries - current).unwrap());
            let count = if bits == 0 {
                0
            } else {
                usize::try_from(reader.read(bits)?).unwrap()
            };
            ensure!(
                count > 0 && current + count <= entries,
                "invalid Vorbis codebook ordering"
            );
            current += count;
            length += 1;
        }
        let _ = length;
    } else {
        let sparse = reader.read(1)? != 0;
        for _ in 0..entries {
            if !sparse || reader.read(1)? != 0 {
                reader.skip(5)?;
            }
        }
    }
    let lookup_type = reader.read(4)?;
    ensure!(
        lookup_type <= 2,
        "unsupported Vorbis codebook lookup type {lookup_type}"
    );
    if lookup_type != 0 {
        reader.skip(32)?;
        reader.skip(32)?;
        let value_bits = usize::try_from(reader.read(4)?).unwrap() + 1;
        reader.skip(1)?;
        let lookup_values = if lookup_type == 1 {
            let mut value = 0_usize;
            while value
                .checked_add(1)
                .and_then(|candidate| candidate.checked_pow(dimensions as u32))
                .is_some_and(|power| power <= entries)
            {
                value += 1;
            }
            value
        } else {
            entries
                .checked_mul(dimensions)
                .context("Vorbis codebook lookup count overflow")?
        };
        reader.skip(
            lookup_values
                .checked_mul(value_bits)
                .context("Vorbis codebook value count overflow")?,
        )?;
    }
    Ok(())
}

fn read_vorbis_setup(
    setup_packet: &[u8],
    channels: usize,
    block_size_0: u64,
    block_size_1: u64,
) -> Result<VorbisConfig> {
    ensure!(
        setup_packet.len() >= 7 && setup_packet[0] == 5 && &setup_packet[1..7] == b"vorbis",
        "invalid Vorbis setup packet"
    );
    let mut reader = BitReader::new(&setup_packet[7..]);
    let codebook_count = usize::try_from(reader.read(8)?).unwrap() + 1;
    for _ in 0..codebook_count {
        read_vorbis_codebook(&mut reader)?;
    }
    let time_count = usize::try_from(reader.read(6)?).unwrap() + 1;
    for _ in 0..time_count {
        ensure!(
            reader.read(16)? == 0,
            "unsupported Vorbis time domain transform"
        );
    }
    let floor_count = usize::try_from(reader.read(6)?).unwrap() + 1;
    for _ in 0..floor_count {
        let floor_type = reader.read(16)?;
        match floor_type {
            0 => {
                reader.skip(8 + 16 + 16 + 6 + 8)?;
                let books = usize::try_from(reader.read(4)?).unwrap() + 1;
                reader.skip(books * 8)?;
            }
            1 => {
                let partitions = usize::try_from(reader.read(5)?).unwrap();
                let mut partition_classes = Vec::with_capacity(partitions);
                let mut max_class = 0_usize;
                for _ in 0..partitions {
                    let class = usize::try_from(reader.read(4)?).unwrap();
                    max_class = max_class.max(class);
                    partition_classes.push(class);
                }
                let mut class_dimensions = vec![0_usize; max_class + 1];
                for dimensions_slot in class_dimensions.iter_mut().take(max_class + 1) {
                    let dimensions = usize::try_from(reader.read(3)?).unwrap() + 1;
                    let subclasses = usize::try_from(reader.read(2)?).unwrap();
                    if subclasses > 0 {
                        reader.skip(8)?;
                    }
                    for _ in 0..(1_usize << subclasses) {
                        reader.skip(8)?;
                    }
                    *dimensions_slot = dimensions;
                }
                reader.skip(2)?;
                let rangebits = usize::try_from(reader.read(4)?).unwrap();
                for class in partition_classes {
                    reader.skip(
                        class_dimensions[class]
                            .checked_mul(rangebits)
                            .context("Vorbis floor post count overflow")?,
                    )?;
                }
            }
            _ => bail!("unsupported Vorbis floor type {floor_type}"),
        }
    }
    let residue_count = usize::try_from(reader.read(6)?).unwrap() + 1;
    for _ in 0..residue_count {
        let residue_type = reader.read(16)?;
        ensure!(
            residue_type <= 2,
            "unsupported Vorbis residue type {residue_type}"
        );
        reader.skip(24 + 24 + 24)?;
        let classifications = usize::try_from(reader.read(6)?).unwrap() + 1;
        reader.skip(8)?;
        let mut cascades = Vec::with_capacity(classifications);
        for _ in 0..classifications {
            let low_bits = reader.read(3)?;
            let high_bits = if reader.read(1)? != 0 {
                reader.read(5)?
            } else {
                0
            };
            cascades.push(low_bits | (high_bits << 3));
        }
        for cascade in cascades {
            for bit in 0..8 {
                if cascade & (1 << bit) != 0 {
                    reader.skip(8)?;
                }
            }
        }
    }
    let mapping_count = usize::try_from(reader.read(6)?).unwrap() + 1;
    for _ in 0..mapping_count {
        let mapping_type = reader.read(16)?;
        ensure!(
            mapping_type == 0,
            "unsupported Vorbis mapping type {mapping_type}"
        );
        let submaps_flag = reader.read(1)? != 0;
        let submaps = if submaps_flag {
            usize::try_from(reader.read(4)?).unwrap() + 1
        } else {
            1
        };
        let coupling_flag = reader.read(1)? != 0;
        if coupling_flag {
            let steps = usize::try_from(reader.read(8)?).unwrap() + 1;
            let channel_bits = ilog(u64::try_from(channels.saturating_sub(1)).unwrap());
            reader.skip(steps * channel_bits * 2)?;
        }
        ensure!(reader.read(2)? == 0, "reserved Vorbis mapping bits are set");
        if submaps > 1 {
            reader.skip(channels * 4)?;
        }
        reader.skip(submaps * 24)?;
    }
    let mode_count = usize::try_from(reader.read(6)?).unwrap() + 1;
    let mut mode_block_sizes = Vec::with_capacity(mode_count);
    for _ in 0..mode_count {
        let block_flag = reader.read(1)? != 0;
        reader.skip(16 + 16 + 8)?;
        mode_block_sizes.push(if block_flag {
            block_size_1
        } else {
            block_size_0
        });
    }
    ensure!(
        reader.read(1)? != 0,
        "Vorbis setup framing bit is not set at bit {}",
        reader.bit - 1
    );
    Ok(VorbisConfig { mode_block_sizes })
}

fn split_codec_private(private: &[u8]) -> Result<Vec<&[u8]>> {
    ensure!(!private.is_empty(), "Vorbis CodecPrivate is empty");
    let packet_count = usize::from(private[0]) + 1;
    let mut cursor = 1;
    let mut lengths = Vec::with_capacity(packet_count - 1);
    for _ in 0..packet_count - 1 {
        let mut length = 0_usize;
        loop {
            let byte = *private
                .get(cursor)
                .context("truncated Vorbis Xiph lacing")?;
            cursor += 1;
            length = length
                .checked_add(usize::from(byte))
                .context("Vorbis header length overflow")?;
            if byte != 255 {
                break;
            }
        }
        lengths.push(length);
    }
    let mut packets = Vec::with_capacity(packet_count);
    for length in lengths {
        let end = cursor
            .checked_add(length)
            .context("Vorbis header end overflow")?;
        packets.push(
            private
                .get(cursor..end)
                .context("truncated Vorbis CodecPrivate")?,
        );
        cursor = end;
    }
    packets.push(
        private
            .get(cursor..)
            .context("truncated Vorbis setup packet")?,
    );
    ensure!(
        packets.len() == 3,
        "expected three Vorbis CodecPrivate packets"
    );
    Ok(packets)
}

fn parse_vorbis_config(private: &[u8]) -> Result<VorbisConfig> {
    let packets = split_codec_private(private)?;
    ensure!(
        packets[0].len() >= 30 && packets[0][0] == 1 && &packets[0][1..7] == b"vorbis",
        "invalid Vorbis identification packet"
    );
    let channels = usize::from(packets[0][11]);
    let blocksize = packets[0][28];
    let block_size_0 = 1_u64 << (blocksize & 0x0F);
    let block_size_1 = 1_u64 << (blocksize >> 4);
    ensure!(
        channels > 0 && block_size_0 <= block_size_1,
        "invalid Vorbis block sizes"
    );
    read_vorbis_setup(packets[2], channels, block_size_0, block_size_1)
}

fn vorbis_packet_block_size(packet: &[u8], config: &VorbisConfig) -> Result<u64> {
    ensure!(!packet.is_empty(), "empty Vorbis audio packet");
    let mut reader = BitReader::new(packet);
    ensure!(reader.read(1)? == 0, "Vorbis packet is not an audio packet");
    let bits = ilog(u64::try_from(config.mode_block_sizes.len().saturating_sub(1)).unwrap());
    let mode = if bits == 0 {
        0
    } else {
        usize::try_from(reader.read(bits)?).unwrap()
    };
    let block_size = *config
        .mode_block_sizes
        .get(mode)
        .context("Vorbis packet references an invalid mode")?;
    Ok(block_size)
}

fn find_child(children: &[Element], id: u32) -> Option<&Element> {
    children.iter().find(|element| element.id == id)
}

fn parse_track(data: &[u8], entry: &Element) -> Result<Option<TrackInfo>> {
    let fields = parse_elements(data, entry.payload_start, entry.end)?;
    let track_type = find_child(&fields, ID_TRACK_TYPE)
        .map(|element| uint_payload(data, element))
        .transpose()?;
    if track_type != Some(2) {
        return Ok(None);
    }
    let number = find_child(&fields, ID_TRACK_NUMBER)
        .context("audio TrackEntry has no TrackNumber")
        .and_then(|element| uint_payload(data, element))?;
    let codec_id = find_child(&fields, ID_CODEC_ID)
        .context("audio TrackEntry has no CodecID")
        .map(|element| {
            String::from_utf8_lossy(&data[element.payload_start..element.end]).into_owned()
        })?;
    if codec_id != "A_VORBIS" {
        return Ok(None);
    }
    let codec_delay_ns = find_child(&fields, ID_CODEC_DELAY)
        .map(|element| uint_payload(data, element))
        .transpose()?
        .unwrap_or(0);
    let sample_rate = find_child(&fields, ID_AUDIO)
        .map(|audio| parse_elements(data, audio.payload_start, audio.end))
        .transpose()?
        .and_then(|fields| find_child(&fields, ID_SAMPLING_FREQUENCY).cloned())
        .map(|element| float_payload(data, &element))
        .transpose()?
        .unwrap_or(48_000.0);
    let codec_private = find_child(&fields, ID_CODEC_PRIVATE)
        .context("A_VORBIS track has no CodecPrivate")
        .map(|element| data[element.payload_start..element.end].to_vec())?;
    let vorbis = parse_vorbis_config(&codec_private).context("parsing Vorbis CodecPrivate")?;
    ensure!(
        sample_rate.is_finite() && sample_rate > 0.0,
        "invalid Vorbis sample rate"
    );
    let codec_delay_frames =
        ((codec_delay_ns as f64 * sample_rate) / 1_000_000_000.0).round() as u64;
    Ok(Some(TrackInfo {
        number,
        codec_id,
        codec_delay_frames,
        sample_rate,
        vorbis,
    }))
}

fn find_track(data: &[u8], segment_children: &[Element]) -> Result<TrackInfo> {
    let tracks = find_child(segment_children, ID_TRACKS).context("WebM has no Tracks element")?;
    let entries = parse_elements(data, tracks.payload_start, tracks.end)?;
    for entry in entries
        .iter()
        .filter(|element| element.id == ID_TRACK_ENTRY)
    {
        if let Some(track) = parse_track(data, entry)? {
            return Ok(track);
        }
    }
    bail!("WebM has no A_VORBIS audio track")
}

fn block_payload(data: &[u8], outer: &Element) -> Result<(Vec<u8>, Option<u64>)> {
    if outer.id == ID_SIMPLE_BLOCK {
        return Ok((data[outer.payload_start..outer.end].to_vec(), None));
    }
    ensure!(outer.id == ID_BLOCK_GROUP, "not a media block");
    let fields = parse_elements(data, outer.payload_start, outer.end)?;
    let block = fields
        .iter()
        .find(|element| element.id == ID_BLOCK)
        .context("BlockGroup has no Block")?;
    let duration = fields
        .iter()
        .find(|element| element.id == ID_BLOCK_DURATION)
        .map(|element| uint_payload(data, element))
        .transpose()?;
    Ok((data[block.payload_start..block.end].to_vec(), duration))
}

fn block_header(payload: &[u8]) -> Result<(u64, i64, u8, usize)> {
    let (track, track_width, _) = vint(payload, 0, false)?;
    ensure!(payload.len() >= track_width + 3, "truncated WebM Block");
    let timecode = i16::from_be_bytes([payload[track_width], payload[track_width + 1]]);
    let flags = payload[track_width + 2];
    ensure!(
        flags & 0x06 == 0,
        "laced audio blocks are not supported; split the packets first"
    );
    Ok((track, i64::from(timecode), flags, track_width))
}

fn parse_audio_blocks(
    data: &[u8],
    segment_children: &[Element],
    track_number: u64,
) -> Result<Vec<AudioBlock>> {
    let mut result = Vec::new();
    for cluster in segment_children
        .iter()
        .filter(|element| element.id == ID_CLUSTER)
    {
        let cluster_children = parse_elements(data, cluster.payload_start, cluster.end)?;
        let cluster_timecode = find_child(&cluster_children, ID_CLUSTERTIMECODE)
            .map(|element| uint_payload(data, element))
            .transpose()?
            .unwrap_or(0);
        for outer in cluster_children
            .iter()
            .filter(|element| element.id == ID_SIMPLE_BLOCK || element.id == ID_BLOCK_GROUP)
        {
            let (payload, duration_ticks) = block_payload(data, outer)?;
            let (track, relative_timecode, _flags, track_width) = block_header(&payload)?;
            if track != track_number {
                continue;
            }
            result.push(AudioBlock {
                index: result.len(),
                outer: outer.clone(),
                timestamp_ticks: i64::try_from(cluster_timecode)
                    .context("cluster timecode does not fit in signed 64 bits")?
                    + relative_timecode,
                duration_ticks,
                packet: payload[track_width + 3..].to_vec(),
            });
        }
    }
    ensure!(result.len() >= 3, "need at least three Vorbis audio blocks");
    Ok(result)
}

fn discard_padding(skip_frames: u64, sample_rate: f64) -> Result<(Vec<u8>, i64)> {
    let nanoseconds = ((skip_frames as f64 * 1_000_000_000.0) / sample_rate).round();
    ensure!(
        nanoseconds.is_finite() && nanoseconds > 0.0,
        "invalid discard duration"
    );
    ensure!(
        nanoseconds <= f64::from(i32::MAX),
        "discard duration does not fit four bytes"
    );
    let nanoseconds = nanoseconds as i32;
    let payload = (-nanoseconds).to_be_bytes();
    Ok((
        make_element(&[0x75, 0xA2], &payload)?,
        i64::from(nanoseconds),
    ))
}

fn void_of_size(total_size: usize) -> Result<Vec<u8>> {
    ensure!(
        total_size >= 2,
        "cannot represent a one-byte EBML element as Void"
    );
    for width in 1..=8 {
        let payload_size = total_size.checked_sub(1 + width);
        let Some(payload_size) = payload_size else {
            continue;
        };
        let marker = 1_u64 << (7 * width);
        if u64::try_from(payload_size).unwrap_or(u64::MAX) < marker - 1 {
            let mut result = Vec::with_capacity(total_size);
            result.push(ID_VOID as u8);
            result.extend_from_slice(&encode_size(payload_size, width)?);
            result.resize(total_size, 0);
            return Ok(result);
        }
    }
    bail!("cannot represent {total_size} bytes as an EBML Void")
}

fn add_padding(data: &[u8], outer: &Element, padding: &[u8]) -> Result<Vec<u8>> {
    if outer.id == ID_SIMPLE_BLOCK {
        let mut block = data[outer.payload_start..outer.end].to_vec();
        let (_, _, _, track_width) = block_header(&block)?;
        block[track_width + 2] &= 0x7F;
        let block = make_element(&[0xA1], &block)?;
        let mut group_payload = block;
        group_payload.extend_from_slice(padding);
        return make_element(&[0xA0], &group_payload);
    }

    let fields = parse_elements(data, outer.payload_start, outer.end)?;
    let mut group_payload = Vec::new();
    for field in fields {
        if field.id != ID_DISCARD_PADDING {
            group_payload.extend_from_slice(&data[field.start..field.end]);
        }
    }
    group_payload.extend_from_slice(padding);
    make_element(&[0xA0], &group_payload)
}

fn timecode_scale(data: &[u8], segment_children: &[Element]) -> Result<u64> {
    let Some(info) = find_child(segment_children, ID_INFO) else {
        return Ok(1_000_000);
    };
    let fields = parse_elements(data, info.payload_start, info.end)?;
    Ok(find_child(&fields, ID_TIMECODE_SCALE)
        .map(|element| uint_payload(data, element))
        .transpose()?
        .unwrap_or(1_000_000))
}

fn frame_counts(
    blocks: &[AudioBlock],
    vorbis: &VorbisConfig,
    scale_ns: u64,
    sample_rate: f64,
) -> Vec<Option<u64>> {
    let mut result = vec![None; blocks.len()];
    let block_sizes: Vec<Option<u64>> = blocks
        .iter()
        .map(|block| vorbis_packet_block_size(&block.packet, vorbis).ok())
        .collect();
    for index in 1..blocks.len() {
        if let (Some(previous), Some(current)) = (block_sizes[index - 1], block_sizes[index]) {
            result[index] = Some((previous + current) / 4);
        }
    }
    if result.iter().any(Option::is_some) {
        return result;
    }
    for index in 0..blocks.len() {
        let ticks: Option<u64> = if index + 1 < blocks.len() {
            blocks[index + 1]
                .timestamp_ticks
                .checked_sub(blocks[index].timestamp_ticks)
                .and_then(|value| u64::try_from(value).ok())
        } else {
            blocks[index].duration_ticks
        };
        if let Some(ticks) = ticks.filter(|ticks| *ticks > 0) {
            let nanoseconds = (ticks as f64) * (scale_ns as f64);
            let frames = (nanoseconds * sample_rate / 1_000_000_000.0).round();
            if frames.is_finite() && frames > 0.0 {
                result[index] = Some(frames as u64);
            }
        }
    }
    result
}

fn rebuild_cluster(
    data: &[u8],
    cluster: &Element,
    targets: &HashMap<usize, Vec<u8>>,
    target_cluster: bool,
) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    for child in parse_elements(data, cluster.payload_start, cluster.end)? {
        if target_cluster && child.id == ID_CRC32 {
            payload.extend_from_slice(&void_of_size(child.end - child.start)?);
        } else if let Some(padding) = targets.get(&child.start) {
            payload.extend_from_slice(&add_padding(data, &child, padding)?);
        } else {
            payload.extend_from_slice(&data[child.start..child.end]);
        }
    }
    rebuild_element(data, cluster, &payload)
}

fn mutate_segment(
    data: &[u8],
    segment: &Element,
    targets: &HashMap<usize, Vec<u8>>,
) -> Result<Vec<u8>> {
    let children = parse_elements(data, segment.payload_start, segment.end)?;
    let target_clusters: std::collections::HashSet<usize> = children
        .iter()
        .filter(|element| element.id == ID_CLUSTER)
        .filter(|cluster| {
            parse_elements(data, cluster.payload_start, cluster.end)
                .map(|children| {
                    children
                        .iter()
                        .any(|child| targets.contains_key(&child.start))
                })
                .unwrap_or(false)
        })
        .map(|cluster| cluster.start)
        .collect();

    let mut payload = Vec::new();
    for child in children {
        if child.id == ID_SEEK_HEAD || child.id == ID_CUES {
            payload.extend_from_slice(&void_of_size(child.end - child.start)?);
        } else if child.id == ID_CLUSTER {
            payload.extend_from_slice(&rebuild_cluster(
                data,
                &child,
                targets,
                target_clusters.contains(&child.start),
            )?);
        } else {
            payload.extend_from_slice(&data[child.start..child.end]);
        }
    }
    rebuild_element(data, segment, &payload)
}

pub fn make_candidate(data: &[u8], options: &WebmOptions) -> Result<(Vec<u8>, serde_json::Value)> {
    let top = parse_elements(data, 0, data.len())?;
    let segment = top
        .iter()
        .find(|element| element.id == ID_SEGMENT)
        .context("WebM has no Segment element")?;
    let segment_children = parse_elements(data, segment.payload_start, segment.end)?;
    let track = find_track(data, &segment_children)?;
    let sample_rate = options.sample_rate.unwrap_or(track.sample_rate);
    ensure!(
        sample_rate.is_finite() && sample_rate > 0.0,
        "invalid sample rate"
    );
    let codec_delay = options
        .codec_delay_frames
        .unwrap_or(track.codec_delay_frames);
    ensure!(
        codec_delay > 0,
        "Vorbis track has no positive CodecDelay; use --codec-delay"
    );
    let blocks = parse_audio_blocks(data, &segment_children, track.number)?;
    let scale_ns = timecode_scale(data, &segment_children)?;
    let frame_counts = frame_counts(&blocks, &track.vorbis, scale_ns, sample_rate);

    let plan = dual_trigger_plan(&frame_counts, codec_delay, options.trigger_skip)?;
    let (shared_padding, shared_ns) = discard_padding(plan.shared_skip_frames, sample_rate)?;
    let (trigger_padding, trigger_ns) = discard_padding(options.trigger_skip, sample_rate)?;

    let mut targets = HashMap::new();
    targets.insert(blocks[0].outer.start, shared_padding.clone());
    targets.insert(blocks[1].outer.start, shared_padding);
    targets.insert(blocks[2].outer.start, trigger_padding);
    let rebuilt_segment = mutate_segment(data, segment, &targets)?;
    let mut output =
        Vec::with_capacity(data.len() + rebuilt_segment.len() - (segment.end - segment.start));
    output.extend_from_slice(&data[..segment.start]);
    output.extend_from_slice(&rebuilt_segment);
    output.extend_from_slice(&data[segment.end..]);

    let details = json!({
        "audio_track": track.number,
        "codec_id": track.codec_id,
        "sample_rate": sample_rate,
        "codec_delay_frames": codec_delay,
        "audio_packet_count": blocks.len(),
        "layout": "dual-immediate-and-delayed",
        "priming_packet": blocks[0].index,
        "carrier_packet": blocks[1].index,
        "trigger_packet": blocks[2].index,
        "carrier_output_frames": plan.carrier_frames,
        "trigger_output_frames": plan.trigger_frames,
        "shared_skip_frames": plan.shared_skip_frames,
        "trigger_skip_frames": options.trigger_skip,
        "expected_carry_frames": plan.carried_frames,
        "expected_check": format!(
            "discarded {} > codec_delay {} before a {}-frame trigger packet",
            plan.carried_frames, codec_delay, plan.trigger_frames
        ),
        "expected_check_fails": true,
        "current_no_delay_path": {
            "packet_0_padding_dropped_without_pcm": true,
            "large_skip_packet": blocks[1].index,
            "positive_trigger_packet": blocks[2].index,
            "check_packet": blocks[2].index,
            "carried_frames": plan.carried_frames,
        },
        "legacy_delayed_path": {
            "large_skip_source_packet": blocks[0].index,
            "large_skip_applied_packet": blocks[1].index,
            "positive_trigger_source_packet": blocks[1].index,
            "check_packet": blocks[2].index,
            "carried_frames": plan.carried_frames,
        },
        "shared_discard_padding_ns": -shared_ns,
        "trigger_discard_padding_ns": -trigger_ns,
        "seek_head_and_cues_replaced_with_void": true,
    });
    Ok((output, details))
}

#[cfg(test)]
mod tests {
    use super::{
        ID_SEGMENT, WebmOptions, dual_trigger_plan, find_track, make_candidate, parse_audio_blocks,
        parse_elements,
    };

    #[test]
    fn standard_vorbis_window_triggers_both_discard_modes() {
        let plan = dual_trigger_plan(&[None, Some(576), Some(1024)], 128, 1).unwrap();
        assert_eq!(plan.carrier_frames, 576);
        assert_eq!(plan.trigger_frames, 1024);
        assert_eq!(plan.shared_skip_frames, 577);
        assert_eq!(plan.carried_frames, 129);
    }

    #[test]
    fn rejects_a_trigger_packet_consumed_by_the_carry() {
        let error = dual_trigger_plan(&[None, Some(576), Some(129)], 128, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("must exceed the 129-frame carry"));
    }

    #[test]
    fn rejects_a_zero_trigger_skip() {
        let error = dual_trigger_plan(&[None, Some(576), Some(1024)], 128, 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--trigger-skip must be positive"));
    }

    #[test]
    fn checked_in_short_control_builds_the_dual_layout_without_changing_packets() {
        let control = include_bytes!("../samples/short-vorbis-dual-control.webm");
        let options = WebmOptions {
            trigger_skip: 1,
            ..WebmOptions::default()
        };
        let (candidate, details) = make_candidate(control, &options).unwrap();

        assert_eq!(candidate.len(), 4206);
        assert_eq!(details["layout"], "dual-immediate-and-delayed");
        assert_eq!(details["shared_skip_frames"], 577);
        assert_eq!(details["expected_carry_frames"], 129);
        assert_eq!(details["current_no_delay_path"]["check_packet"], 2);
        assert_eq!(details["legacy_delayed_path"]["check_packet"], 2);

        fn packets(data: &[u8]) -> Vec<Vec<u8>> {
            let top = parse_elements(data, 0, data.len()).unwrap();
            let segment = top.iter().find(|element| element.id == ID_SEGMENT).unwrap();
            let children = parse_elements(data, segment.payload_start, segment.end).unwrap();
            let track = find_track(data, &children).unwrap();
            parse_audio_blocks(data, &children, track.number)
                .unwrap()
                .into_iter()
                .map(|block| block.packet)
                .collect()
        }

        assert_eq!(packets(control), packets(&candidate));
    }
}
