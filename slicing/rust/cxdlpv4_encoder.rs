//! Creality CXDLPv4 (`.cxdlpv4`) container, read by the HALOT-MAGE family.
//!
//! The container is a sibling of CTB: layer images use the same 7-bit gray RLE
//! and the previews the same RGB15 RLE, so both are taken from the CTB modules.
//! What differs is the framing, written here:
//!
//! - header: `u32 BE 9`, `"CXSW3DV2\0"`, `u16 BE` version 4, then the printer
//!   model as a `u32 BE` length (terminator included) and the NUL-terminated
//!   name; every field after that is little-endian;
//! - small (120x120) and large (300x300) preview records, each followed by its
//!   image;
//! - print parameters (68 bytes) and slicer info (76 bytes);
//! - the layer table (40 bytes per layer), then per layer a 44-byte extended
//!   record directly followed by its RLE;
//! - a CRC-32 (reflected 0xEDB88320, initial value 0, no final xor) of every
//!   byte before it, appended big-endian.
//!
//! The field layout follows the format notes published with UVtools
//! (`CrealityCXDLPv4File`); this is an independent implementation.
//! Layer encryption is never used (key 0), and the pixel-pair codec of the
//! HALOT-X1 series is not implemented, so X1 jobs are refused.

use crate::engine::SlicerV3Error;
use crate::types::{LayerAreaStatsV3, SliceJobV3};
use serde_json::Value;
use std::path::Path;

use super::ctb_layout::push_ctb_run;
use super::ctb_metadata::{
    ctb_multi_parameter_exposure_override, parse_ctb_build_model_from_job,
    parse_timing_model_from_metadata,
};
use super::ctb_preview::build_previews_sized;
use super::ctb_types::{CtbPreparedLayer, CtbTimingModel};

pub(super) const CXDLPV4_OUTPUT_FORMAT: &str = ".cxdlpv4";

const MAGIC: &[u8; 9] = b"CXSW3DV2\0";
const VERSION: u16 = 4;
const PREVIEW_RECORD_SIZE: u32 = 32;
const PRINT_PARAMETERS_SIZE: u32 = 68;
const SLICER_INFO_SIZE: u32 = 76;
pub(super) const LAYER_DEF_SIZE: u32 = 40;
pub(super) const LAYER_DEF_EX_SIZE: u32 = 44;
const PREVIEW_SIZES: [(u32, u32); 2] = [(120, 120), (300, 300)];

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn non_negative(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 {
        v
    } else {
        0.0
    }
}

/// The model name the HALOT firmware expects. Creality's own slicer writes the
/// machine code (for example `CL-103L` for the HALOT-MAGE), so known profile
/// names are mapped to it. `cxdlpv4.machineName` in the job metadata overrides
/// the mapping; any other name is written as given.
pub(super) fn resolve_cxdlpv4_machine_name(
    job: &SliceJobV3,
    profile_name: &str,
) -> Result<String, SlicerV3Error> {
    let override_name = serde_json::from_str::<Value>(&job.metadata_json)
        .ok()
        .and_then(|meta| {
            meta.get("cxdlpv4")
                .and_then(|v| v.get("machineName"))
                .or_else(|| {
                    meta.get("export")
                        .and_then(|v| v.get("cxdlpv4"))
                        .and_then(|v| v.get("machineName"))
                })
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty());

    let name = override_name.unwrap_or_else(|| profile_name.trim().to_string());
    let normalized: String = name
        .to_ascii_uppercase()
        .chars()
        .map(|c| if c == '_' || c == ' ' { '-' } else { c })
        .collect();

    if normalized.contains("X1") || normalized == "R009" {
        return Err(SlicerV3Error::UnsupportedOutput(format!(
            "CXDLPv4 for \"{name}\" needs the HALOT-X1 pixel-pair layer codec, which is not implemented yet."
        )));
    }
    if normalized.starts_with("CL-") || normalized.starts_with("CT-") {
        return Ok(normalized);
    }
    if normalized.contains("MAGE-PRO") {
        return Ok("CL-103".to_string());
    }
    if normalized.ends_with("HALOT-MAGE") || normalized == "MAGE" {
        return Ok("CL-103L".to_string());
    }
    Ok(name)
}

fn transition_exposure(timing: &CtbTimingModel, index: u32) -> f32 {
    let steps = timing.transition_layer_count.max(1) as f32;
    let step = (index + 1).saturating_sub(timing.bottom_layer_count) as f32;
    timing.bottom_exposure_sec - (timing.bottom_exposure_sec - timing.normal_exposure_sec) / steps * step
}

struct LayerParams {
    exposure_sec: f32,
    light_off_sec: f32,
    lift_total_mm: f32,
    lift_speed: f32,
    lift2_mm: f32,
    lift_speed2: f32,
    retract_speed: f32,
    retract2_mm: f32,
    retract_speed2: f32,
    wait_after_cure: f32,
    wait_after_lift: f32,
    wait_before_cure: f32,
    pwm: f32,
}

fn pwm_or_full(value: u16) -> u16 {
    if value == 0 {
        255
    } else {
        value.min(255)
    }
}

fn layer_params(
    job: &SliceJobV3,
    timing: &CtbTimingModel,
    index: u32,
    area_mm2: Option<f64>,
) -> (LayerParams, bool) {
    let is_bottom = index < timing.bottom_layer_count;
    let bottom_wait = index < timing.wait_time_bottom_layer_count;
    let pick = |bottom: f32, normal: f32| if is_bottom { bottom } else { normal };
    let pick_wait = |bottom: f32, normal: f32| if bottom_wait { bottom } else { normal };

    let override_exposure = ctb_multi_parameter_exposure_override(
        &job.metadata_json,
        index as usize,
        job.layer_height_mm,
        timing.bottom_layer_count,
        timing.transition_layer_count,
        area_mm2,
    );
    let exposure_sec = if let Some(v) = override_exposure {
        v
    } else if is_bottom {
        timing.bottom_exposure_sec
    } else if index < timing.bottom_layer_count + timing.transition_layer_count {
        transition_exposure(timing, index)
    } else {
        timing.normal_exposure_sec
    };

    let lift1 = non_negative(pick(timing.bottom_lift_distance_mm, timing.lift_distance_mm));
    let lift2 = non_negative(pick(timing.bottom_lift_distance2_mm, timing.lift_distance2_mm));
    let lift_total = lift1 + lift2;
    let retract2 = non_negative(pick(timing.bottom_retract_height2_mm, timing.retract_distance2_mm))
        .min(lift_total);

    (
        LayerParams {
            exposure_sec: non_negative(exposure_sec),
            light_off_sec: non_negative(pick(
                timing.bottom_light_off_delay_sec,
                timing.light_off_delay_sec,
            )),
            lift_total_mm: lift_total,
            lift_speed: non_negative(pick(timing.bottom_lift_speed_mm_min, timing.lift_speed_mm_min)),
            lift2_mm: lift2,
            lift_speed2: non_negative(pick(
                timing.bottom_lift_speed2_mm_min,
                timing.lift_speed2_mm_min,
            )),
            retract_speed: non_negative(pick(
                timing.bottom_retract_speed_mm_min,
                timing.retract_speed_mm_min,
            )),
            retract2_mm: retract2,
            retract_speed2: non_negative(pick(
                timing.bottom_retract_speed2_mm_min,
                timing.retract_speed2_mm_min,
            )),
            wait_after_cure: non_negative(pick_wait(
                timing.bottom_wait_time_after_cure_sec,
                timing.wait_time_after_cure_sec,
            )),
            wait_after_lift: non_negative(pick_wait(
                timing.bottom_wait_time_after_lift_sec,
                timing.wait_time_after_lift_sec,
            )),
            wait_before_cure: non_negative(pick_wait(
                timing.bottom_wait_time_before_cure_sec,
                timing.wait_time_before_cure_sec,
            )),
            pwm: pwm_or_full(if is_bottom {
                timing.bottom_layer_projector_duty_cycle_pwm
            } else {
                timing.projector_duty_cycle_pwm
            }) as f32,
        },
        override_exposure.is_some(),
    )
}

/// Seconds for one layer: waits, exposure, and the lift/retract travel.
fn layer_seconds(p: &LayerParams) -> f32 {
    let travel = |mm: f32, speed_mm_min: f32| {
        if mm > 0.0 && speed_mm_min > 0.0 {
            mm / speed_mm_min * 60.0
        } else {
            0.0
        }
    };
    let lift1 = p.lift_total_mm - p.lift2_mm;
    let retract1 = p.lift_total_mm - p.retract2_mm;
    p.exposure_sec
        + p.light_off_sec
        + p.wait_after_cure
        + p.wait_after_lift
        + p.wait_before_cure
        + travel(lift1, p.lift_speed)
        + travel(p.lift2_mm, p.lift_speed2)
        + travel(p.retract2_mm, p.retract_speed2)
        + travel(retract1, p.retract_speed)
}

fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut v = i as u32;
        for _ in 0..8 {
            v = if v & 1 == 0 { v >> 1 } else { (v >> 1) ^ 0xEDB8_8320 };
        }
        *slot = v;
    }
    table
}

/// The container checksum: CRC-32 with initial value 0 and no final xor.
pub(super) fn cxdlpv4_checksum(bytes: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0u32;
    for &b in bytes {
        crc = table[((crc as u8) ^ b) as usize] ^ (crc >> 8);
    }
    crc
}

/// Encodes one layer's RLE runs (in display order) as CTB 7-bit gray RLE.
pub(super) fn encode_cxdlpv4_rle_layer(
    runs: &[crate::rle::RleRun],
    total_pixels: usize,
    is_anti_aliased: bool,
    threshold: u8,
) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(32 * 1024);
    if runs.is_empty() {
        push_ctb_run(&mut encoded, total_pixels.min(u32::MAX as usize) as u32, 0);
        return encoded;
    }
    for run in runs {
        let value = if is_anti_aliased {
            run.value
        } else if run.value > threshold {
            255
        } else {
            0
        };
        push_ctb_run(&mut encoded, run.length, value);
    }
    encoded
}

/// Assembles a complete `.cxdlpv4` file from layers already RLE-encoded and
/// sorted by index.
pub(super) fn build_cxdlpv4_container_bytes(
    job: &SliceJobV3,
    prepared: &[CtbPreparedLayer],
    layer_area_stats: &[LayerAreaStatsV3],
) -> Result<Vec<u8>, SlicerV3Error> {
    if prepared.is_empty() {
        return Err(SlicerV3Error::MissingRenderedLayerPayload(
            "no rendered layers were provided for CXDLPv4 encoding".to_string(),
        ));
    }
    let resolution_x = u16::try_from(job.source_width_px).map_err(|_| {
        SlicerV3Error::UnsupportedOutput("CXDLPv4 resolution X exceeds 65535 pixels".to_string())
    })?;
    let resolution_y = u16::try_from(job.source_height_px).map_err(|_| {
        SlicerV3Error::UnsupportedOutput("CXDLPv4 resolution Y exceeds 65535 pixels".to_string())
    })?;

    let timing = parse_timing_model_from_metadata(&job.metadata_json);
    let build = parse_ctb_build_model_from_job(job);
    let machine_name = resolve_cxdlpv4_machine_name(job, &build.machine_name)?;
    let previews = build_previews_sized(job, PREVIEW_SIZES)?;

    let layer_count = prepared.len() as u32;
    let mut model_bytes = machine_name.into_bytes();
    model_bytes.push(0);

    let header_size = 4 + MAGIC.len() as u32 + 2 + 4 + model_bytes.len() as u32 + 2 + 2 + 5 * 4 + 10 * 4 + 2 + 2 + 3 * 4;

    // Offsets, in file order.
    let small_preview_offset = header_size;
    let large_preview_offset = small_preview_offset + PREVIEW_RECORD_SIZE + previews[0].encoded.len() as u32;
    let print_parameters_offset = large_preview_offset + PREVIEW_RECORD_SIZE + previews[1].encoded.len() as u32;
    let slicer_offset = print_parameters_offset + PRINT_PARAMETERS_SIZE;
    let layer_defs_offset = slicer_offset + SLICER_INFO_SIZE;
    let layer_data_start = layer_defs_offset as u64 + (layer_count as u64) * (LAYER_DEF_SIZE as u64);

    let mut layer_defs = Vec::with_capacity(prepared.len() * LAYER_DEF_SIZE as usize);
    let mut layer_data: Vec<u8> = Vec::new();
    let mut print_time = 0.0_f32;
    let mut uses_overrides = false;
    let mut volume_mm3 = 0.0_f64;
    let pixel_area_mm2 = (job.build_width_mm as f64 / job.source_width_px.max(1) as f64)
        * (job.build_depth_mm as f64 / job.source_height_px.max(1) as f64);

    for layer in prepared {
        let index = layer.index as u32;
        let stats = layer_area_stats.get(layer.index);
        let (params, overridden) =
            layer_params(job, &timing, index, stats.map(|s| s.total_solid_area_mm2));
        uses_overrides |= overridden;
        print_time += layer_seconds(&params);
        if let Some(s) = stats {
            volume_mm3 += s.total_solid_area_mm2 * job.layer_height_mm as f64;
        }

        let record_offset = layer_data_start + layer_data.len() as u64;
        let record_offset = u32::try_from(record_offset).map_err(|_| {
            SlicerV3Error::UnsupportedOutput("CXDLPv4 files are limited to 4 GiB".to_string())
        })?;
        let largest_area = stats
            .map(|s| {
                let mm2 = if s.largest_area_mm2 > 0.0 {
                    s.largest_area_mm2
                } else {
                    s.total_solid_pixels as f64 * pixel_area_mm2
                };
                (mm2 * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32
            })
            .unwrap_or(0);

        push_f32(&mut layer_defs, (index as f32 + 1.0) * job.layer_height_mm);
        push_f32(&mut layer_defs, params.exposure_sec);
        push_f32(&mut layer_defs, params.light_off_sec);
        push_u32(&mut layer_defs, record_offset);
        push_u32(&mut layer_defs, LAYER_DEF_EX_SIZE + layer.encoded.len() as u32);
        push_u32(&mut layer_defs, 0); // data type: 7-bit RLE
        push_u32(&mut layer_defs, 0); // centroid distance
        push_u32(&mut layer_defs, largest_area);
        push_u32(&mut layer_defs, 0);
        push_u32(&mut layer_defs, 0);

        push_f32(&mut layer_data, params.lift_total_mm);
        push_f32(&mut layer_data, params.lift_speed);
        push_f32(&mut layer_data, params.lift2_mm);
        push_f32(&mut layer_data, params.lift_speed2);
        push_f32(&mut layer_data, params.retract_speed);
        push_f32(&mut layer_data, params.retract2_mm);
        push_f32(&mut layer_data, params.retract_speed2);
        push_f32(&mut layer_data, params.wait_after_cure);
        push_f32(&mut layer_data, params.wait_after_lift);
        push_f32(&mut layer_data, params.wait_before_cure);
        push_f32(&mut layer_data, params.pwm);
        layer_data.extend_from_slice(&layer.encoded);
    }

    let print_height = layer_count as f32 * job.layer_height_mm;
    let bottom_lift_total = non_negative(timing.bottom_lift_distance_mm) + non_negative(timing.bottom_lift_distance2_mm);
    let lift_total = non_negative(timing.lift_distance_mm) + non_negative(timing.lift_distance2_mm);
    let volume_ml = (volume_mm3 / 1000.0) as f32;
    let anti_alias = build.anti_alias_level.clamp(1, 16);

    let mut out = Vec::with_capacity(layer_data_start as usize + layer_data.len() + 4);

    // Header.
    out.extend_from_slice(&(MAGIC.len() as u32).to_be_bytes());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&(model_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&model_bytes);
    push_u16(&mut out, resolution_x);
    push_u16(&mut out, resolution_y);
    push_f32(&mut out, job.build_width_mm);
    push_f32(&mut out, job.build_depth_mm);
    push_f32(&mut out, build.bed_size_z_mm);
    push_f32(&mut out, print_height);
    push_f32(&mut out, job.layer_height_mm);
    push_u32(&mut out, timing.bottom_layer_count);
    push_u32(&mut out, small_preview_offset);
    push_u32(&mut out, layer_defs_offset);
    push_u32(&mut out, layer_count);
    push_u32(&mut out, large_preview_offset);
    push_u32(&mut out, print_time.round().max(0.0) as u32);
    push_u32(&mut out, build.projector_type);
    push_u32(&mut out, print_parameters_offset);
    push_u32(&mut out, PRINT_PARAMETERS_SIZE);
    push_u32(&mut out, anti_alias);
    push_u16(&mut out, pwm_or_full(timing.projector_duty_cycle_pwm));
    push_u16(&mut out, pwm_or_full(timing.bottom_layer_projector_duty_cycle_pwm));
    push_u32(&mut out, 0); // encryption key: none
    push_u32(&mut out, slicer_offset);
    push_u32(&mut out, SLICER_INFO_SIZE);
    debug_assert_eq!(out.len(), header_size as usize);

    // Previews: small, then large.
    for (preview, offset) in previews.iter().zip([small_preview_offset, large_preview_offset]) {
        debug_assert_eq!(out.len(), offset as usize);
        push_u32(&mut out, preview.width);
        push_u32(&mut out, preview.height);
        push_u32(&mut out, offset + PREVIEW_RECORD_SIZE);
        push_u32(&mut out, preview.encoded.len() as u32);
        for _ in 0..4 {
            push_u32(&mut out, 0);
        }
        out.extend_from_slice(&preview.encoded);
    }

    // Print parameters.
    debug_assert_eq!(out.len(), print_parameters_offset as usize);
    push_f32(&mut out, bottom_lift_total);
    push_f32(&mut out, non_negative(timing.bottom_lift_speed_mm_min));
    push_f32(&mut out, lift_total);
    push_f32(&mut out, non_negative(timing.lift_speed_mm_min));
    push_f32(&mut out, non_negative(timing.retract_speed_mm_min));
    push_f32(&mut out, volume_ml);
    push_f32(&mut out, 0.0); // weight (g)
    push_f32(&mut out, 0.0); // cost
    push_f32(&mut out, non_negative(timing.bottom_light_off_delay_sec));
    push_f32(&mut out, non_negative(timing.light_off_delay_sec));
    push_u32(&mut out, timing.bottom_layer_count);
    push_f32(&mut out, non_negative(timing.normal_exposure_sec));
    push_f32(&mut out, non_negative(timing.bottom_exposure_sec));
    for _ in 0..4 {
        push_u32(&mut out, 0);
    }

    // Slicer info.
    debug_assert_eq!(out.len(), slicer_offset as usize);
    push_f32(&mut out, non_negative(timing.bottom_lift_distance2_mm));
    push_f32(&mut out, non_negative(timing.bottom_lift_speed2_mm_min));
    push_f32(&mut out, non_negative(timing.lift_distance2_mm));
    push_f32(&mut out, non_negative(timing.lift_speed2_mm_min));
    push_f32(&mut out, non_negative(timing.retract_distance2_mm).min(lift_total));
    push_f32(&mut out, non_negative(timing.retract_speed2_mm_min));
    push_f32(&mut out, non_negative(timing.wait_time_after_lift_sec));
    push_u32(&mut out, u32::from(build.per_layer_settings || uses_overrides));
    push_u32(&mut out, build.modified_date_unix / 60);
    push_u32(&mut out, anti_alias);
    push_u32(&mut out, 0); // software version
    push_f32(&mut out, non_negative(timing.wait_time_before_cure_sec));
    push_f32(&mut out, non_negative(timing.wait_time_after_cure_sec));
    push_f32(&mut out, non_negative(timing.normal_exposure_sec));
    push_f32(&mut out, non_negative(timing.bottom_exposure_sec));
    push_f32(&mut out, non_negative(timing.wait_time_after_lift_sec));
    push_u32(&mut out, timing.transition_layer_count);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);

    debug_assert_eq!(out.len(), layer_defs_offset as usize);
    out.extend_from_slice(&layer_defs);
    out.extend_from_slice(&layer_data);

    let checksum = cxdlpv4_checksum(&out);
    out.extend_from_slice(&checksum.to_be_bytes());
    Ok(out)
}

fn read_u32_le(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| "CXDLPv4 file is truncated".to_string())
}

/// The parts of a `.cxdlpv4` file a reader needs to find a layer.
pub(super) struct Cxdlpv4Index {
    pub resolution_x: u32,
    pub resolution_y: u32,
    pub layer_count: u32,
    pub layer_defs_offset: u32,
}

pub(super) fn parse_cxdlpv4_index(bytes: &[u8]) -> Result<Cxdlpv4Index, String> {
    if bytes.len() < 4 + MAGIC.len() + 2 + 4 || &bytes[4..4 + MAGIC.len()] != MAGIC {
        return Err("Not a CXDLPv4 file".to_string());
    }
    let name_len_at = 4 + MAGIC.len() + 2;
    let name_len = u32::from_be_bytes(
        bytes[name_len_at..name_len_at + 4].try_into().map_err(|_| "CXDLPv4 file is truncated")?,
    ) as usize;
    let at = name_len_at + 4 + name_len;
    let res = bytes.get(at..at + 4).ok_or("CXDLPv4 file is truncated")?;
    let resolution_x = u16::from_le_bytes([res[0], res[1]]) as u32;
    let resolution_y = u16::from_le_bytes([res[2], res[3]]) as u32;
    // After the resolution: 5 floats, bottom count, small preview offset, then
    // the layer table offset and the layer count.
    let fields = at + 4 + 5 * 4;
    Ok(Cxdlpv4Index {
        resolution_x,
        resolution_y,
        layer_defs_offset: read_u32_le(bytes, fields + 8)?,
        layer_count: read_u32_le(bytes, fields + 12)?,
    })
}

/// Returns the raw RLE of a 1-based layer.
pub(super) fn cxdlpv4_layer_rle(bytes: &[u8], index: &Cxdlpv4Index, layer_number: u32) -> Result<Vec<u8>, String> {
    if layer_number == 0 || layer_number > index.layer_count {
        return Err(format!(
            "Layer {layer_number} is out of range (file has {} layers)",
            index.layer_count
        ));
    }
    let def = index.layer_defs_offset as usize + (layer_number as usize - 1) * LAYER_DEF_SIZE as usize;
    let address = read_u32_le(bytes, def + 12)? as usize;
    let size = read_u32_le(bytes, def + 16)? as usize;
    if size < LAYER_DEF_EX_SIZE as usize {
        return Err("CXDLPv4 layer record is smaller than its header".to_string());
    }
    bytes
        .get(address + LAYER_DEF_EX_SIZE as usize..address + size)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| "CXDLPv4 layer data runs past the end of the file".to_string())
}

pub(super) fn read_cxdlpv4_layer_preview_png(path: &Path, layer_number: u32) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Failed opening CXDLPv4 file: {e}"))?;
    let index = parse_cxdlpv4_index(&bytes)?;
    let rle = cxdlpv4_layer_rle(&bytes, &index, layer_number)?;
    let pixels = super::decode_ctb_rle(&rle, (index.resolution_x * index.resolution_y) as usize);
    super::encode_pixels_as_grayscale_png(index.resolution_x, index.resolution_y, &pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rle::RleRun;

    fn job(printer_name: &str) -> SliceJobV3 {
        SliceJobV3 {
            output_format: CXDLPV4_OUTPUT_FORMAT.to_string(),
            source_width_px: 4,
            source_height_px: 2,
            width_px: 4,
            height_px: 2,
            build_width_mm: 228.096,
            build_depth_mm: 128.304,
            layer_height_mm: 0.05,
            total_layers: 2,
            anti_aliasing_level: "Off".to_string(),
            metadata_json: format!(
                r#"{{"printer":{{"name":"{printer_name}","buildVolumeMm":{{"height":230}}}},
                    "material":{{"normalExposureSec":2.5,"bottomExposureSec":30,"bottomLayerCount":1}}}}"#
            ),
            ..Default::default()
        }
    }

    fn layers() -> Vec<CtbPreparedLayer> {
        let runs = [
            vec![RleRun { length: 2, value: 255 }, RleRun { length: 6, value: 0 }],
            vec![RleRun { length: 8, value: 255 }],
        ];
        runs.iter()
            .enumerate()
            .map(|(index, r)| CtbPreparedLayer {
                index,
                source_len: 8,
                encoded: encode_cxdlpv4_rle_layer(r, 8, false, 127),
            })
            .collect()
    }

    #[test]
    fn maps_profile_names_to_machine_codes() {
        let j = job("HALOT-MAGE");
        assert_eq!(resolve_cxdlpv4_machine_name(&j, "HALOT-MAGE").unwrap(), "CL-103L");
        assert_eq!(resolve_cxdlpv4_machine_name(&j, "HALOT-MAGE PRO").unwrap(), "CL-103");
        assert_eq!(resolve_cxdlpv4_machine_name(&j, "cl-103l").unwrap(), "CL-103L");
        assert_eq!(resolve_cxdlpv4_machine_name(&j, "HALOT-R6").unwrap(), "HALOT-R6");
        assert!(resolve_cxdlpv4_machine_name(&j, "HALOT-X1 Max").is_err());
    }

    #[test]
    fn metadata_overrides_machine_name() {
        let mut j = job("HALOT-MAGE");
        j.metadata_json = r#"{"cxdlpv4":{"machineName":"CL-133"}}"#.to_string();
        assert_eq!(resolve_cxdlpv4_machine_name(&j, "HALOT-MAGE").unwrap(), "CL-133");
    }

    #[test]
    fn checksum_matches_reference_crc_without_final_xor() {
        // CRC-32 of "123456789" is 0xCBF43926 with init/xorout 0xFFFFFFFF;
        // with both at 0 the same table gives 0x2DFD2D88.
        assert_eq!(cxdlpv4_checksum(b"123456789"), 0x2DFD_2D88);
    }

    #[test]
    fn container_has_header_layers_and_checksum() {
        let j = job("HALOT-MAGE");
        let bytes = build_cxdlpv4_container_bytes(&j, &layers(), &[]).unwrap();

        assert_eq!(&bytes[0..4], &9u32.to_be_bytes());
        assert_eq!(&bytes[4..13], b"CXSW3DV2\0");
        assert_eq!(&bytes[13..15], &4u16.to_be_bytes());
        assert_eq!(&bytes[15..19], &8u32.to_be_bytes());
        assert_eq!(&bytes[19..27], b"CL-103L\0");

        let body = &bytes[..bytes.len() - 4];
        let stored = u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().unwrap());
        assert_eq!(stored, cxdlpv4_checksum(body));

        let index = parse_cxdlpv4_index(&bytes).unwrap();
        assert_eq!((index.resolution_x, index.resolution_y, index.layer_count), (4, 2, 2));

        let first = cxdlpv4_layer_rle(&bytes, &index, 1).unwrap();
        let pixels = super::super::decode_ctb_rle(&first, 8);
        assert_eq!(pixels, vec![254, 254, 0, 0, 0, 0, 0, 0]);
        let second = cxdlpv4_layer_rle(&bytes, &index, 2).unwrap();
        let pixels = super::super::decode_ctb_rle(&second, 8);
        assert_eq!(pixels, vec![254; 8]);
        assert!(cxdlpv4_layer_rle(&bytes, &index, 3).is_err());

        // Layer table: Z and exposure for the bottom and the normal layer.
        let def = index.layer_defs_offset as usize;
        let f = |at: usize| f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert!((f(def) - 0.05).abs() < 1e-6);
        assert_eq!(f(def + 4), 30.0);
        assert_eq!(f(def + LAYER_DEF_SIZE as usize + 4), 2.5);
        // The extended record starts where the table says, and its PWM is full on.
        let address = read_u32_le(&bytes, def + 12).unwrap() as usize;
        assert_eq!(f(address + 40), 255.0);
    }

    #[test]
    fn x1_jobs_are_refused() {
        let j = job("HALOT-X1");
        assert!(build_cxdlpv4_container_bytes(&j, &layers(), &[]).is_err());
    }
    #[test]
    fn app_layer_preview_reads_cxdlpv4_through_the_registry() {
        let j = job("HALOT-MAGE");
        let bytes = build_cxdlpv4_container_bytes(&j, &layers(), &[]).unwrap();
        let path = std::env::temp_dir().join(format!("rambutan-cxdlpv4-{}.cxdlpv4", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();

        assert!(crate::encoders::registry::find_encoder(CXDLPV4_OUTPUT_FORMAT).is_some());
        let png = crate::engine::read_layer_preview_png_by_format_hint(&path, 1, ".cxdlpv4");
        std::fs::remove_file(&path).ok();
        let png = png.unwrap();

        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0u8; reader.output_buffer_size()];
        reader.next_frame(&mut pixels).unwrap();
        assert_eq!((reader.info().width, reader.info().height), (4, 2));
        assert_eq!(&pixels[..8], &[254, 254, 0, 0, 0, 0, 0, 0]);
    }
}
