//! Safe Rust WSQ encoder: 9/7 wavelets, scalar quantization and canonical Huffman coding.
//!
//! Uses the public-domain NBIS algorithm and the same decomposition trees as
//! the in-tree decoder. All codec state belongs to an individual encode call.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use anyhow::{Result, ensure};

use super::decoder::{QuantTree, WaveletTree, encoding_trees};

/// Smallest supported image side for the WSQ decomposition and reconstruction filters.
pub const MIN_ENCODE_DIMENSION: u32 = 128;
const MAX_PIXELS: usize = 128 * 1024 * 1024;
const SUBBANDS: usize = 60;
const VARIANCE_THRESHOLD: f64 = 1.01;

// Standard WSQ odd-length analysis filter bank, rounded to single precision.
const LOW: [f32; 9] = [
    0.03782845550699546,
    -0.02384946501938000,
    -0.11062440441842342,
    0.37740285561265380,
    0.85269867900940344,
    0.37740285561265380,
    -0.11062440441842342,
    -0.02384946501938000,
    0.03782845550699546,
];
const HIGH: [f32; 7] = [
    0.06453888262893845,
    -0.04068941760955844,
    -0.41809227322221221,
    0.78848561640566439,
    -0.41809227322221221,
    -0.04068941760955844,
    0.06453888262893845,
];

/// Encode row-major 8-bit grayscale samples as WSQ, retaining PPI and target bitrate.
pub fn encode(
    pixels: &[u8],
    width: u32,
    height: u32,
    ppi: Option<u32>,
    bitrate: f32,
) -> Result<Vec<u8>> {
    ensure!(
        (MIN_ENCODE_DIMENSION..=u16::MAX as u32).contains(&width)
            && (MIN_ENCODE_DIMENSION..=u16::MAX as u32).contains(&height),
        "WSQ encoding requires dimensions between {MIN_ENCODE_DIMENSION} and 65535 pixels"
    );
    let count = u64::from(width) * u64::from(height);
    ensure!(
        count == pixels.len() as u64 && count <= MAX_PIXELS as u64,
        "Invalid or oversized WSQ pixel buffer"
    );
    ensure!(
        bitrate.is_finite() && bitrate > 0.0 && bitrate <= 2.25,
        "WSQ bitrate must be between 0 and 2.25 bits per pixel"
    );
    let (mut samples, shift, scale) = normalize(pixels);
    let (wavelet, quantization) = encoding_trees(width, height);
    decompose(&mut samples, width as usize, &wavelet);
    let (q, z, blocks) = quantize(&samples, width as usize, &quantization, f64::from(bitrate));
    let first_table = HuffmanTable::new(&[&blocks[0]]);
    let second_table = HuffmanTable::new(&[&blocks[1], &blocks[2]]);
    let mut bytes = vec![0xff, 0xa0];
    let comment = format!(
        "NIST_COM 9\nPIX_WIDTH {width}\nPIX_HEIGHT {height}\nPIX_DEPTH 8\nPPI {}\nLOSSY 1\nCOLORSPACE GRAY\nCOMPRESSION WSQ\nWSQ_BITRATE {bitrate}\n",
        ppi.map_or_else(|| "-1".into(), |value| value.to_string())
    );
    segment(&mut bytes, 0xffa8, comment.as_bytes());
    write_transform(&mut bytes);
    write_quantization(&mut bytes, &q, &z);
    let mut frame = vec![0, 255];
    frame.extend((height as u16).to_be_bytes());
    frame.extend((width as u16).to_be_bytes());
    decimal16(&mut frame, shift);
    decimal16(&mut frame, scale);
    frame.push(2); // WSQ encoder version.
    frame.extend(0u16.to_be_bytes()); // Software implementer identifier, unassigned.
    segment(&mut bytes, 0xffa2, &frame);
    write_huffman(&mut bytes, 0, &first_table);
    write_block(&mut bytes, 0, &first_table, &blocks[0]);
    write_huffman(&mut bytes, 1, &second_table);
    write_block(&mut bytes, 1, &second_table, &blocks[1]);
    write_block(&mut bytes, 1, &second_table, &blocks[2]);
    bytes.extend([0xff, 0xa1]);
    Ok(bytes)
}

fn normalize(pixels: &[u8]) -> (Vec<f32>, f32, f32) {
    let sum: u64 = pixels.iter().map(|value| u64::from(*value)).sum();
    let shift = (sum as f64 / pixels.len() as f64) as f32;
    let low = f32::from(*pixels.iter().min().unwrap());
    let high = f32::from(*pixels.iter().max().unwrap());
    let range = (shift - low).max(high - shift) / 128.0;
    let scale = if range > 0.0 { range } else { 1.0 };
    (
        pixels
            .iter()
            .map(|value| (f32::from(*value) - shift) / scale)
            .collect(),
        shift,
        scale,
    )
}

/// Whole-sample symmetric extension: ... 2 1 | 0 1 2 ... n-2 n-1 | n-2 ...
fn reflect(index: isize, length: usize) -> usize {
    if length <= 1 {
        return 0;
    }
    let period = 2 * (length as isize - 1);
    let index = index.rem_euclid(period) as usize;
    if index < length {
        index
    } else {
        period as usize - index
    }
}

fn analysis_line(source: &[f32], output: &mut [f32], inverted: bool) {
    let low_length = source.len().div_ceil(2);
    let high_length = source.len() / 2;
    let (low_start, high_start) = if inverted {
        (high_length, 0)
    } else {
        (0, low_length)
    };
    for k in 0..low_length {
        let mut value = 0.0;
        for (tap, coefficient) in LOW.iter().enumerate() {
            value += source[reflect(2 * k as isize - 4 + tap as isize, source.len())] * coefficient;
        }
        output[low_start + k] = value;
    }
    for k in 0..high_length {
        let mut value = 0.0;
        for (tap, coefficient) in HIGH.iter().enumerate() {
            value += source[reflect(2 * k as isize - 2 + tap as isize, source.len())] * coefficient;
        }
        output[high_start + k] = value;
    }
}

fn decompose(samples: &mut [f32], width: usize, tree: &[WaveletTree]) {
    for node in tree {
        let (x, y, nx, ny) = (
            node.x as usize,
            node.y as usize,
            node.lenx as usize,
            node.leny as usize,
        );
        let mut horizontal = vec![0.0; nx * ny];
        for row in 0..ny {
            let start = (y + row) * width + x;
            analysis_line(
                &samples[start..start + nx],
                &mut horizontal[row * nx..(row + 1) * nx],
                node.invrw != 0,
            );
        }
        let mut column = vec![0.0; ny];
        let mut transformed = vec![0.0; ny];
        for col in 0..nx {
            for row in 0..ny {
                column[row] = horizontal[row * nx + col];
            }
            analysis_line(&column, &mut transformed, node.invcl != 0);
            for row in 0..ny {
                samples[(y + row) * width + x + col] = transformed[row];
            }
        }
    }
}

fn band_values<'a>(
    samples: &'a [f32],
    width: usize,
    band: &'a QuantTree,
) -> impl Iterator<Item = f32> + 'a {
    (0..band.leny as usize).flat_map(move |row| {
        let start = (band.y as usize + row) * width + band.x as usize;
        samples[start..start + band.lenx as usize].iter().copied()
    })
}

fn variance(samples: &[f32], width: usize, band: &QuantTree, cropped: bool) -> f64 {
    let (skip_x, skip_y, nx, ny) = if cropped {
        (
            band.lenx / 8,
            9 * band.leny / 32,
            3 * band.lenx / 4,
            7 * band.leny / 16,
        )
    } else {
        (0, 0, band.lenx, band.leny)
    };
    if nx * ny <= 1 {
        return 0.0;
    }
    let (mut sum, mut squares) = (0.0f64, 0.0f64);
    for row in 0..ny as usize {
        let start =
            (band.y as usize + skip_y as usize + row) * width + band.x as usize + skip_x as usize;
        for value in &samples[start..start + nx as usize] {
            let value = f64::from(*value);
            sum += value;
            squares += value * value;
        }
    }
    let count = f64::from(nx * ny);
    ((squares - sum * sum / count) / (count - 1.0)).max(0.0)
}

fn quantize(
    samples: &[f32],
    width: usize,
    tree: &[QuantTree],
    bitrate: f64,
) -> ([f32; 64], [f32; 64], [Vec<i16>; 3]) {
    let cropped = tree[..4]
        .iter()
        .map(|band| variance(samples, width, band, true))
        .sum::<f64>()
        >= 20000.0;
    let variances: Vec<f64> = tree[..SUBBANDS]
        .iter()
        .map(|band| variance(samples, width, band, cropped))
        .collect();
    let weights = [1.32, 1.08, 1.42, 1.08, 1.32, 1.42, 1.08, 1.08];
    let prime: Vec<f64> = (0..SUBBANDS)
        .map(|band| {
            if variances[band] < VARIANCE_THRESHOLD {
                0.0
            } else if band < 4 {
                1.0
            } else {
                10.0 / (if band >= 52 { weights[band - 52] } else { 1.0 } * variances[band].ln())
            }
        })
        .collect();
    let fractions: Vec<f64> = (0..SUBBANDS)
        .map(|band| {
            if band < 4 {
                1.0 / 1024.0
            } else if band < 51 {
                1.0 / 256.0
            } else {
                1.0 / 16.0
            }
        })
        .collect();
    let mut active: Vec<usize> = (0..SUBBANDS).filter(|band| prime[*band] > 0.0).collect();
    let mut log_q = 0.0;
    while !active.is_empty() {
        let sum: f64 = active.iter().map(|band| fractions[*band]).sum();
        let log_product: f64 = active
            .iter()
            .map(|band| fractions[*band] * (variances[*band].sqrt() / prime[*band]).ln())
            .sum();
        log_q = (bitrate / sum - 1.0) * std::f64::consts::LN_2 - 2.5f64.ln() - log_product / sum;
        let previous = active.len();
        active.retain(|band| prime[*band] * (-log_q).exp() < 5.0 * variances[*band].sqrt());
        if active.len() == previous {
            break;
        }
    }
    let (mut q, mut z) = ([0.0; 64], [0.0; 64]);
    let mut blocks: [Vec<i16>; 3] = Default::default();
    for band in 0..SUBBANDS {
        if prime[band] == 0.0 {
            continue;
        }
        let maximum = band_values(samples, width, &tree[band])
            .map(|value| value.abs())
            .fold(0.0f32, f32::max);
        // WSQ escape coefficients must fit in a signed 16-bit integer.
        let minimum = f64::from(maximum) / 32760.0;
        let bin = (prime[band] * (-log_q).exp()).clamp(minimum.max(1e-9), 50000.0) as f32;
        q[band] = decimal_value(bin);
        z[band] = decimal_value(1.2 * q[band]);
        let half_zero = z[band] / 2.0;
        let block = if band < 19 {
            0
        } else if band < 52 {
            1
        } else {
            2
        };
        for value in band_values(samples, width, &tree[band]) {
            let coefficient = if value.abs() <= half_zero {
                0
            } else {
                (((value.abs() - half_zero) / q[band]) + 1.0)
                    .trunc()
                    .min(32767.0) as i16
                    * if value < 0.0 { -1 } else { 1 }
            };
            blocks[block].push(coefficient);
        }
    }
    (q, z, blocks)
}

/// Decimal coefficients are encoded once and used as represented on the wire.
fn decimal_parts(value: f32, maximum: u64) -> (u8, u64) {
    if value == 0.0 {
        return (0, 0);
    }
    let mut scaled = f64::from(value.abs());
    let mut exponent = 0;
    while scaled * 10.0 < maximum as f64 && exponent < 255 {
        scaled *= 10.0;
        exponent += 1;
    }
    (exponent, scaled.round().min(maximum as f64) as u64)
}

fn decimal_value(value: f32) -> f32 {
    let (exponent, coefficient) = decimal_parts(value, u16::MAX as u64);
    let mut value = coefficient as f32;
    for _ in 0..exponent {
        value /= 10.0;
    }
    value
}

fn decimal16(bytes: &mut Vec<u8>, value: f32) {
    let (exponent, coefficient) = decimal_parts(value, u16::MAX as u64);
    bytes.push(exponent);
    bytes.extend((coefficient as u16).to_be_bytes());
}

fn segment(bytes: &mut Vec<u8>, marker: u16, payload: &[u8]) {
    bytes.extend(marker.to_be_bytes());
    bytes.extend(((payload.len() + 2) as u16).to_be_bytes());
    bytes.extend(payload);
}

fn write_transform(bytes: &mut Vec<u8>) {
    let mut table = vec![LOW.len() as u8, HIGH.len() as u8];
    for coefficient in LOW[LOW.len() / 2..].iter().chain(&HIGH[HIGH.len() / 2..]) {
        let (exponent, value) = decimal_parts(*coefficient, u32::MAX as u64);
        table.push(u8::from(*coefficient < 0.0));
        table.push(exponent);
        table.extend((value as u32).to_be_bytes());
    }
    segment(bytes, 0xffa4, &table);
}

fn write_quantization(bytes: &mut Vec<u8>, q: &[f32; 64], z: &[f32; 64]) {
    let mut table = vec![2, 0, 44];
    for band in 0..64 {
        decimal16(&mut table, q[band]);
        decimal16(&mut table, z[band]);
    }
    segment(bytes, 0xffa5, &table);
}

/// Emit WSQ symbols and optional unsigned escape values without allocating tokens.
fn symbols(coefficients: &[i16], mut emit: impl FnMut(u8, u16, u8)) {
    let mut position = 0;
    while position < coefficients.len() {
        let value = coefficients[position];
        if value == 0 {
            let start = position;
            while position < coefficients.len()
                && coefficients[position] == 0
                && position - start < u16::MAX as usize
            {
                position += 1;
            }
            let run = (position - start) as u16;
            if run <= 100 {
                emit(run as u8, 0, 0);
            } else if run <= 255 {
                emit(105, run, 8);
            } else {
                emit(106, run, 16);
            }
        } else {
            if (-73..=74).contains(&value) {
                emit((i32::from(value) + 180) as u8, 0, 0);
            } else {
                let magnitude = value.unsigned_abs();
                emit(
                    match (value > 0, magnitude <= 255) {
                        (true, true) => 101,
                        (false, true) => 102,
                        (true, false) => 103,
                        (false, false) => 104,
                    },
                    magnitude,
                    if magnitude <= 255 { 8 } else { 16 },
                );
            }
            position += 1;
        }
    }
}

struct HuffmanTable {
    counts: [u8; 16],
    values: Vec<u8>,
    codes: [(u16, u8); 256],
}

impl HuffmanTable {
    fn new(blocks: &[&[i16]]) -> Self {
        let mut frequencies = [0u64; 257];
        for block in blocks {
            symbols(block, |symbol, _, _| frequencies[symbol as usize] += 1);
        }
        if frequencies[..256].iter().all(|frequency| *frequency == 0) {
            frequencies[1] = 1;
        }
        // A dummy leaf reserves the forbidden all-ones padding code.
        frequencies[256] = 1;
        let mut children = vec![None; 257];
        let mut heap = BinaryHeap::new();
        for (symbol, frequency) in frequencies
            .iter()
            .enumerate()
            .filter(|(_, frequency)| **frequency > 0)
        {
            heap.push(Reverse((*frequency, symbol)));
        }
        while heap.len() > 1 {
            let Reverse((a_weight, a)) = heap.pop().unwrap();
            let Reverse((b_weight, b)) = heap.pop().unwrap();
            let node = children.len();
            children.push(Some((a, b)));
            heap.push(Reverse((a_weight + b_weight, node)));
        }
        let mut lengths = [0usize; 257];
        let mut stack = vec![(heap.pop().unwrap().0.1, 0)];
        while let Some((node, depth)) = stack.pop() {
            if let Some((a, b)) = children[node] {
                stack.push((a, depth + 1));
                stack.push((b, depth + 1));
            } else {
                lengths[node] = depth;
            }
        }
        let maximum = *lengths.iter().max().unwrap();
        let mut counts = vec![0usize; maximum.max(16) + 1];
        for length in lengths.iter().filter(|length| **length > 0) {
            counts[*length] += 1;
        }
        for length in (17..counts.len()).rev() {
            while counts[length] > 0 {
                let shorter = (1..length - 1)
                    .rev()
                    .find(|index| counts[*index] > 0)
                    .unwrap();
                counts[length] -= 2;
                counts[length - 1] += 1;
                counts[shorter + 1] += 2;
                counts[shorter] -= 1;
            }
        }
        let last = (1..=16).rev().find(|length| counts[*length] > 0).unwrap();
        counts[last] -= 1;
        let mut values: Vec<u8> = (0..256)
            .filter(|symbol| frequencies[*symbol] > 0)
            .map(|symbol| symbol as u8)
            .collect();
        values.sort_by_key(|symbol| (lengths[*symbol as usize], *symbol));
        let mut codes = [(0, 0); 256];
        let mut code = 0u32;
        let mut index = 0;
        for length in 1..=16 {
            for _ in 0..counts[length] {
                codes[values[index] as usize] = (code as u16, length as u8);
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        Self {
            counts: std::array::from_fn(|index| counts[index + 1] as u8),
            values,
            codes,
        }
    }
}

fn write_huffman(bytes: &mut Vec<u8>, id: u8, table: &HuffmanTable) {
    let mut payload = vec![id];
    payload.extend(table.counts);
    payload.extend(&table.values);
    segment(bytes, 0xffa6, &payload);
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    used: u8,
}

impl BitWriter {
    fn write(&mut self, value: u16, count: u8) {
        for shift in (0..count).rev() {
            self.current = (self.current << 1) | ((value >> shift) & 1) as u8;
            self.used += 1;
            if self.used == 8 {
                self.bytes.push(self.current);
                if self.current == 0xff {
                    self.bytes.push(0);
                }
                self.current = 0;
                self.used = 0;
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used > 0 {
            let remaining = 8 - self.used;
            self.write((1u16 << remaining) - 1, remaining);
        }
        self.bytes
    }
}

fn write_block(bytes: &mut Vec<u8>, id: u8, table: &HuffmanTable, coefficients: &[i16]) {
    segment(bytes, 0xffa3, &[id]);
    let mut writer = BitWriter::default();
    symbols(coefficients, |symbol, extra, bits| {
        let (code, length) = table.codes[symbol as usize];
        writer.write(code, length);
        writer.write(extra, bits);
    });
    bytes.extend(writer.finish());
}
/// Preserve a usable NISTCOM bitrate; older files without one use 0.75 bpp.
pub fn source_bitrate(bytes: &[u8]) -> f32 {
    let mut position = 2;
    while let Some(header) = bytes.get(position..position + 4) {
        let marker = u16::from_be_bytes([header[0], header[1]]);
        if marker == 0xffa2 || marker == 0xffa3 {
            break;
        }
        let length = u16::from_be_bytes([header[2], header[3]]) as usize;
        if length < 2 {
            break;
        }
        let Some(payload) = bytes.get(position + 4..position + 2 + length) else {
            break;
        };
        if marker == 0xffa8 {
            if let Ok(comment) = std::str::from_utf8(payload) {
                for line in comment.lines() {
                    let mut items = line.split_whitespace();
                    if items.next() == Some("WSQ_BITRATE") {
                        if let Some(rate) = items.next().and_then(|value| value.parse::<f32>().ok())
                        {
                            if rate.is_finite() && rate > 0.0 && rate <= 2.25 {
                                return rate;
                            }
                        }
                    }
                }
            }
        }
        position += length + 2;
    }
    0.75
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huffman_codes_are_length_limited_prefix_free_and_reserve_padding() {
        // Fibonacci frequencies force an unconstrained tree beyond 16 levels.
        let (mut a, mut b) = (1, 1);
        let mut coefficients = Vec::new();
        for symbol in 1..=24 {
            coefficients.extend(std::iter::repeat_n(symbol, a));
            (a, b) = (b, a + b);
        }
        let table = HuffmanTable::new(&[&coefficients]);
        assert_eq!(
            table
                .counts
                .iter()
                .map(|count| usize::from(*count))
                .sum::<usize>(),
            table.values.len()
        );
        for &symbol in &table.values {
            let (code, length) = table.codes[symbol as usize];
            assert!((1..=16).contains(&length));
            assert!(
                u32::from(code) < (1u32 << length) - 1,
                "all-ones code must be reserved for padding"
            );
            for &other in &table.values {
                if other == symbol {
                    continue;
                }
                let (other_code, other_length) = table.codes[other as usize];
                if length >= other_length {
                    assert_ne!(
                        code >> (length - other_length),
                        other_code,
                        "Huffman codes overlap"
                    );
                }
            }
        }
    }

    #[test]
    fn entropy_bytes_are_stuffed_and_partial_bytes_use_one_bit_padding() {
        let mut writer = BitWriter::default();
        writer.write(0xff, 8);
        writer.write(0b10, 2);
        assert_eq!(writer.finish(), [0xff, 0, 0xbf]);
    }
}
