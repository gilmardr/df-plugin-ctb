use super::ctb_layout::{ctb_layer_rle_xor, page_number_and_offset, rle_encode_mask_row_major};
use super::ctb_types::*;
use super::{decode_ctb_rle_checked, decode_png_as_grayscale};
use crate::encoders::PrintIslandReport;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_PIXELS: usize = 150_000_000;
static EDIT_LOCK: Mutex<()> = Mutex::new(());

struct PlainCtb {
    source: File,
    file_len: u64,
    modified: Option<SystemTime>,
    width: u32,
    height: u32,
    pixel_count: usize,
    table_start: u64,
    table_end: u64,
    layer_count: u32,
    xor_key: u32,
    legacy: bool,
}

struct Layer {
    definition: [u8; CTB_LAYER_DEF_SIZE as usize],
    definition_offset: u64,
    data_offset: u64,
    extended_size: usize,
    index: u32,
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

impl PlainCtb {
    fn open(path: &Path) -> Result<Self, String> {
        let mut source = File::open(path).map_err(|err| format!("Opening CTB: {err}"))?;
        let metadata = source
            .metadata()
            .map_err(|err| format!("Reading CTB metadata: {err}"))?;
        let mut header = [0; CTB_HEADER_SIZE as usize];
        source
            .read_exact(&mut header)
            .map_err(|err| format!("Reading CTB header: {err}"))?;

        let legacy = match read_u32(&header, 0) {
            CTB_MAGIC_V2_V3 => true,
            CTB_MAGIC_V4_V5 => false,
            CTB_MAGIC_V5_ENCRYPTED => {
                return Err("Editing encrypted CTB files is not supported.".into())
            }
            _ => return Err("The selected file is not a supported CTB artifact.".into()),
        };
        let width = read_u32(&header, 52);
        let height = read_u32(&header, 56);
        let pixel_count = (width as usize)
            .checked_mul(height as usize)
            .filter(|count| (1..=MAX_PIXELS).contains(count))
            .ok_or("CTB raster dimensions are invalid or exceed the editing limit.")?;
        let table_start = u64::from(read_u32(&header, 64));
        let layer_count = read_u32(&header, 68);
        let table_end = table_start + u64::from(layer_count) * u64::from(CTB_LAYER_DEF_SIZE);
        if layer_count == 0
            || table_start < u64::from(CTB_HEADER_SIZE)
            || table_end > metadata.len()
        {
            return Err("CTB layer table is outside the file or empty.".into());
        }

        Ok(Self {
            source,
            file_len: metadata.len(),
            modified: metadata.modified().ok(),
            width,
            height,
            pixel_count,
            table_start,
            table_end,
            layer_count,
            xor_key: read_u32(&header, 100),
            legacy,
        })
    }

    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), String> {
        self.source
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.source.read_exact(buffer))
            .map_err(|err| format!("Reading CTB data at byte {offset}: {err}"))
    }

    fn read_layer(&mut self, index: u32) -> Result<(Layer, Vec<u8>), String> {
        if index >= self.layer_count {
            return Err(format!(
                "CTB layer {} is outside the file.",
                u64::from(index) + 1
            ));
        }

        let definition_offset = self.table_start + u64::from(index) * u64::from(CTB_LAYER_DEF_SIZE);
        let mut definition = [0; CTB_LAYER_DEF_SIZE as usize];
        self.read_at(definition_offset, &mut definition)?;
        let data_offset = u64::from(read_u32(&definition, 20)) * CTB_PAGE_SIZE
            + u64::from(read_u32(&definition, 12));
        let data_len = read_u32(&definition, 16) as usize;
        let extended_size = match read_u32(&definition, 24) {
            0..=CTB_LAYER_DEF_SIZE => 0,
            CTB_LAYER_DEF_EX_SIZE => CTB_LAYER_DEF_EX_SIZE as usize,
            _ => {
                return Err(format!(
                    "CTB layer {} uses an unsupported metadata layout.",
                    index + 1
                ))
            }
        };
        if data_offset.saturating_sub(extended_size as u64) < self.table_end
            || data_offset.saturating_add(data_len as u64) > self.file_len
            || !(1..=self.pixel_count * 2).contains(&data_len)
        {
            return Err(format!(
                "CTB layer {} has an invalid raster range.",
                index + 1
            ));
        }

        let mut encoded = vec![0; data_len];
        self.read_at(data_offset, &mut encoded)?;
        ctb_layer_rle_xor(self.xor_key, index, &mut encoded);
        let (pixels, complete) = decode_ctb_rle_checked(&encoded, self.pixel_count);
        if !complete {
            return Err(format!(
                "CTB layer {} contains invalid run-length data.",
                index + 1
            ));
        }
        Ok((
            Layer {
                definition,
                definition_offset,
                data_offset,
                extended_size,
                index,
            },
            pixels,
        ))
    }

    fn stage(&mut self, path: &Path) -> Result<StagedEdit, String> {
        let mut stage = StagedEdit::create(path)?;
        self.source
            .rewind()
            .map_err(|err| format!("Seeking original CTB: {err}"))?;
        let copied = io::copy(&mut self.source, stage.file.as_mut().unwrap())
            .map_err(|err| format!("Copying CTB into the staging file: {err}"))?;
        if copied != self.file_len {
            return Err("The CTB file changed during editing.".into());
        }
        Ok(stage)
    }

    fn write_layer(
        &mut self,
        file: &mut File,
        mut layer: Layer,
        pixels: &[u8],
    ) -> Result<(), String> {
        let mut encoded = rle_encode_mask_row_major(pixels);
        ctb_layer_rle_xor(self.xor_key, layer.index, &mut encoded);
        let len = u32::try_from(encoded.len()).map_err(|_| "Edited CTB raster is too large.")?;
        let mut extended = vec![0; layer.extended_size];
        if !extended.is_empty() {
            self.read_at(
                layer.data_offset - layer.extended_size as u64,
                &mut extended,
            )?;
        }

        // Append the record because multiple layers can reference the same raster.
        let data_offset = file
            .seek(SeekFrom::End(0))
            .map_err(|err| format!("Seeking staged CTB: {err}"))?
            + layer.extended_size as u64;
        if self.legacy && data_offset.saturating_add(u64::from(len)) >= CTB_PAGE_SIZE {
            return Err("Edited CTB exceeds the size supported by this CTB version.".into());
        }
        let (page, offset) = page_number_and_offset(data_offset);
        for record in [&mut layer.definition[..], &mut extended[..]] {
            if record.is_empty() {
                continue;
            }
            record[12..16].copy_from_slice(&offset.to_le_bytes());
            record[16..20].copy_from_slice(&len.to_le_bytes());
            record[20..24].copy_from_slice(&page.to_le_bytes());
        }
        if !extended.is_empty() {
            let total = (CTB_LAYER_DEF_EX_SIZE + len).to_le_bytes();
            extended[36..40].copy_from_slice(&total);
        }

        file.seek(SeekFrom::Start(data_offset - layer.extended_size as u64))
            .and_then(|_| file.write_all(&extended))
            .and_then(|_| file.write_all(&encoded))
            .and_then(|_| file.seek(SeekFrom::Start(layer.definition_offset)))
            .and_then(|_| file.write_all(&layer.definition))
            .map_err(|err| format!("Writing edited CTB layer {}: {err}", layer.index + 1))
    }

    fn commit(self, stage: StagedEdit, path: &Path) -> Result<u64, String> {
        let metadata = fs::metadata(path).map_err(|err| format!("Checking original CTB: {err}"))?;
        if metadata.len() != self.file_len || metadata.modified().ok() != self.modified {
            return Err("The CTB file changed during editing.".into());
        }
        drop(self.source);
        stage.commit(path)
    }
}

struct StagedEdit {
    path: PathBuf,
    file: Option<File>,
}

impl StagedEdit {
    fn create(path: &Path) -> Result<Self, String> {
        let name = path.file_name().ok_or("Missing CTB filename.")?;
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(
            ".edit-{}-{}.tmp",
            std::process::id(),
            timestamp().as_nanos()
        ));
        let temp = path.with_file_name(temp_name);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|err| format!("Creating staged CTB file: {err}"))?;
        Ok(Self {
            path: temp,
            file: Some(file),
        })
    }

    fn commit(mut self, path: &Path) -> Result<u64, String> {
        let mut file = self.file.take().unwrap();
        file.seek(SeekFrom::Start(24))
            .and_then(|_| file.write_all(&(timestamp().as_secs() as u32).to_le_bytes()))
            .and_then(|_| file.sync_all())
            .map_err(|err| format!("Flushing edited CTB file: {err}"))?;
        let size = file
            .metadata()
            .map_err(|err| format!("Reading edited CTB metadata: {err}"))?
            .len();
        drop(file);
        fs::rename(&self.path, path).map_err(|err| format!("Replacing the original CTB: {err}"))?;
        Ok(size)
    }
}

impl Drop for StagedEdit {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn timestamp() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

pub(super) fn replace_layer_pixels(path: &Path, number: u32, png: &[u8]) -> Result<u64, String> {
    let _lock = EDIT_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let mut ctb = PlainCtb::open(path)?;
    let index = number
        .checked_sub(1)
        .ok_or("Layer number must be at least 1.")?;
    let (layer, old_pixels) = ctb.read_layer(index)?;
    let (_, _, new_pixels) = decode_png_as_grayscale(png, ctb.width, ctb.height)?;
    if new_pixels == old_pixels {
        return Ok(ctb.file_len);
    }
    let mut stage = ctb.stage(path)?;
    ctb.write_layer(stage.file.as_mut().unwrap(), layer, &new_pixels)?;
    ctb.commit(stage, path)
}

pub(super) fn process_islands(
    path: &Path,
    minimum_pixels: u32,
    remove: bool,
    progress: &dyn Fn(u32, u32),
) -> Result<PrintIslandReport, String> {
    let _lock = EDIT_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let mut ctb = PlainCtb::open(path)?;
    let (_, mut previous) = ctb.read_layer(0)?;
    let mut stage = None;
    let mut finder = IslandFinder::default();
    let mut report = PrintIslandReport {
        byte_size: ctb.file_len,
        ..PrintIslandReport::default()
    };
    progress(1, ctb.layer_count);

    for index in 1..ctb.layer_count {
        let (layer, mut current) = ctb.read_layer(index)?;
        let (island_count, pixel_count) = finder.erase(
            &mut current,
            &previous,
            ctb.width as usize,
            minimum_pixels.max(1) as usize,
        );
        if island_count > 0 {
            report.affected_layers += 1;
            report.first_affected_layer.get_or_insert(index + 1);
            report.island_count += island_count;
            report.pixel_count += pixel_count;
            if remove {
                if stage.is_none() {
                    stage = Some(ctb.stage(path)?);
                }
                ctb.write_layer(
                    stage.as_mut().unwrap().file.as_mut().unwrap(),
                    layer,
                    &current,
                )?;
            }
        }
        // Scanning simulates the same removals, so the report includes regions that
        // lose their support when an island on an earlier layer is removed.
        previous = current;
        progress(index + 1, ctb.layer_count);
    }
    if let Some(stage) = stage {
        report.byte_size = ctb.commit(stage, path)?;
    }
    Ok(report)
}

#[derive(Default)]
struct IslandFinder {
    visited: Vec<u64>,
    spans: Vec<Range<usize>>,
}

impl IslandFinder {
    fn seen(&self, pixel: usize) -> bool {
        self.visited[pixel / 64] & (1 << (pixel % 64)) != 0
    }

    fn visit_span(&mut self, mask: &[u8], seed: usize, width: usize) -> usize {
        let row_start = seed / width * width;
        let mut from = seed;
        let mut to = seed + 1;
        while from > row_start && mask[from - 1] != 0 && !self.seen(from - 1) {
            from -= 1;
        }
        while to < row_start + width && mask[to] != 0 && !self.seen(to) {
            to += 1;
        }
        for pixel in from..to {
            self.visited[pixel / 64] |= 1 << (pixel % 64);
        }
        self.spans.push(from..to);
        to
    }

    fn erase(&mut self, mask: &mut [u8], previous: &[u8], width: usize, min: usize) -> (u64, u64) {
        if mask
            .iter()
            .zip(previous)
            .all(|(&pixel, &below)| pixel == 0 || below != 0)
        {
            return (0, 0);
        }
        self.visited.resize(mask.len().div_ceil(64), 0);
        self.visited.fill(0);

        let mut islands = 0;
        let mut pixels = 0;
        for seed in 0..mask.len() {
            if mask[seed] == 0 || self.seen(seed) {
                continue;
            }
            self.spans.clear();
            self.visit_span(mask, seed, width);
            let mut head = 0;
            let mut area = 0;
            let mut supported = false;
            while head < self.spans.len() {
                let span = self.spans[head].clone();
                head += 1;
                area += span.len();
                supported |= previous[span.clone()].iter().any(|&pixel| pixel != 0);
                let row = span.start / width * width;
                let start_x = (span.start - row).saturating_sub(1);
                let end_x = (span.end - row + 1).min(width);
                let above = row.checked_sub(width);
                let below = (row + width < mask.len()).then_some(row + width);
                for adjacent in [above, below].into_iter().flatten() {
                    let mut pixel = adjacent + start_x;
                    while pixel < adjacent + end_x {
                        if mask[pixel] != 0 && !self.seen(pixel) {
                            pixel = self.visit_span(mask, pixel, width);
                        } else {
                            pixel += 1;
                        }
                    }
                }
            }
            if !supported && area >= min {
                islands += 1;
                pixels += area as u64;
                for span in &self.spans {
                    mask[span.clone()].fill(0);
                }
            }
        }
        (islands, pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoders::FormatEncoder;
    use crate::types::SliceJobV3;

    fn write_fixture(version: &str) -> (StagedEdit, Vec<Vec<u8>>) {
        let mut masks = vec![vec![0; 40]; 4];
        masks[0][0] = 200;
        masks[1][0] = 200;
        masks[1][13] = 150;
        masks[1][22] = 160; // A diagonal neighbour of pixel 13.
        masks[2][13] = 180; // Only supported by the island on layer two.
        masks[3][13] = 210;

        let job = SliceJobV3 {
            output_format: ".ctb".into(),
            format_version: Some(version.into()),
            source_width_px: 8,
            source_height_px: 5,
            width_px: 8,
            height_px: 5,
            build_width_mm: 10.0,
            build_depth_mm: 10.0,
            layer_height_mm: 0.05,
            total_layers: masks.len() as u32,
            metadata_json: "{}".into(),
            ..Default::default()
        };
        let model = super::super::ctb_metadata::parse_ctb_build_model_from_job(&job);
        let layers = super::super::ctb_layout::prepare_layers_for_ctb(
            &masks,
            true,
            127,
            model.layer_xor_key,
        );
        let bytes = super::super::ctb_layout::build_ctb_container_bytes(&job, &layers).unwrap();
        let mut fixture =
            StagedEdit::create(&std::env::temp_dir().join("rambutan-islands.ctb")).unwrap();
        let mut file = fixture.file.take().unwrap();
        file.write_all(&bytes).unwrap();
        drop(file);
        (fixture, masks)
    }

    #[test]
    fn scans_and_saves_cascading_islands_in_all_plain_versions() {
        for version in ["v2", "v3", "v4", "v5"] {
            let (fixture, masks) = write_fixture(version);
            let original = fs::read(&fixture.path).unwrap();
            let initial = process_islands(&fixture.path, 2, false, &|_, _| {}).unwrap();
            assert_eq!(
                (
                    initial.island_count,
                    initial.pixel_count,
                    initial.affected_layers
                ),
                (1, 2, 1)
            );
            let scan = process_islands(&fixture.path, 1, false, &|_, _| {}).unwrap();
            assert_eq!(scan.first_affected_layer, Some(2));
            assert_eq!(
                (scan.island_count, scan.pixel_count, scan.affected_layers),
                (3, 4, 3)
            );
            assert_eq!(fs::read(&fixture.path).unwrap(), original);

            let result = process_islands(&fixture.path, 1, true, &|_, _| {}).unwrap();
            assert_eq!(
                (
                    result.island_count,
                    result.pixel_count,
                    result.affected_layers
                ),
                (3, 4, 3)
            );
            let cleaned = fs::read(&fixture.path).unwrap();
            assert_eq!(cleaned.len() as u64, result.byte_size);
            let mut file = PlainCtb::open(&fixture.path).unwrap();
            let table = file.table_start as usize;
            for index in 0..4 {
                let offset = table + index * CTB_LAYER_DEF_SIZE as usize;
                assert_eq!(
                    cleaned[offset..offset + 12],
                    original[offset..offset + 12],
                    "timing changed on {version} layer {index}"
                );
            }
            assert_eq!(file.read_layer(0).unwrap().1, masks[0]);
            assert_eq!(file.read_layer(1).unwrap().1[0], 200);
            assert_eq!(file.read_layer(1).unwrap().1[13], 0);
            assert!(file
                .read_layer(2)
                .unwrap()
                .1
                .iter()
                .all(|&pixel| pixel == 0));
            drop(file);
            assert_eq!(
                process_islands(&fixture.path, 1, true, &|_, _| {})
                    .unwrap()
                    .island_count,
                0
            );
            assert_eq!(fs::read(&fixture.path).unwrap(), cleaned);
        }
    }

    #[test]
    fn malformed_late_layer_does_not_commit_earlier_removals() {
        let (fixture, _) = write_fixture("v4");
        let mut bytes = fs::read(&fixture.path).unwrap();
        let last_definition = read_u32(&bytes, 64) as usize + 3 * CTB_LAYER_DEF_SIZE as usize;
        bytes[last_definition + 16..last_definition + 20].copy_from_slice(&0u32.to_le_bytes());
        fs::write(&fixture.path, &bytes).unwrap();
        assert!(process_islands(&fixture.path, 1, true, &|_, _| {}).is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), bytes);
    }

    #[test]
    fn pixel_edit_keeps_other_layers_and_round_trips_through_png_preview() {
        let (fixture, masks) = write_fixture("v5");
        let replacement = super::super::encode_pixels_as_grayscale_png(8, 5, &[100; 40]).unwrap();
        replace_layer_pixels(&fixture.path, 2, &replacement).unwrap();
        let mut file = PlainCtb::open(&fixture.path).unwrap();
        assert_eq!(file.read_layer(0).unwrap().1, masks[0]);
        assert_eq!(file.read_layer(1).unwrap().1, [100; 40]);
        let preview = super::super::CtbPluginEncoder
            .read_layer_preview_png(&fixture.path, 2)
            .unwrap();
        assert_eq!(
            decode_png_as_grayscale(&preview, 8, 5).unwrap().2,
            [100; 40]
        );
    }
}
