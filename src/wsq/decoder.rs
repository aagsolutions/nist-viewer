/*
 * Copyright (c) 2026 Aurel Avramescu.
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights to
 * use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
 * the Software, and to permit persons to whom the Software is furnished to do
 * so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *  
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND,
 * EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
 * MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
 * NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
 * HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
 * WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
 * OTHER DEALINGS IN THE SOFTWARE.
 */

//! Pure-Rust WSQ decoder.
//!
//! Implements the WSQ 3.1 specification used by the FBI for grayscale fingerprint
//! images. The algorithm is ported from the public-domain NBIS 5.0 reference
//! implementation (also re-implemented in the [`jnbis`](https://github.com/mhshams/jnbis)
//! Java library).
//!
//! Public entry point: [`decode`].

use anyhow::{anyhow, bail, Result};

use super::header::{WsqHeader, WsqMarker};

/// Decoded 8-bit grayscale WSQ image.
#[derive(Debug, Clone)]
pub struct DecodedWsq {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Pixel data in row-major order, one byte per pixel.
    pub pixels: Vec<u8>,
    /// PPI (pixels per inch) extracted from the WSQ comment, if present.
    pub ppi: Option<u32>,
}

const MAX_HUFFBITS: usize = 16;
const MAX_HUFFCOUNTS_WSQ: usize = 256;
const W_TREELEN: usize = 20;
const Q_TREELEN: usize = 64;
const NUM_SUBBANDS: usize = 60;

#[derive(Debug, Clone, Default)]
struct WaveletTree {
    x: i32,
    y: i32,
    lenx: i32,
    leny: i32,
    invrw: i32,
    invcl: i32,
}

#[derive(Debug, Clone, Default)]
struct QuantTree {
    x: i32,
    y: i32,
    lenx: i32,
    leny: i32,
}

#[derive(Debug, Clone, Default)]
struct TableDTT {
    lofilt: Vec<f32>,
    hifilt: Vec<f32>,
    losz: i32,
    hisz: i32,
    lodef: i32,
    hidef: i32,
}

#[derive(Debug, Clone)]
struct TableDQT {
    bin_center: f32,
    q_bin: Vec<f32>,
    z_bin: Vec<f32>,
    dqt_def: i32,
}

impl Default for TableDQT {
    fn default() -> Self {
        Self {
            bin_center: 0.0,
            q_bin: vec![0.0; 64],
            z_bin: vec![0.0; 64],
            dqt_def: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct TableDHT {
    tabdef: i32,
    huffbits: Vec<u8>,
    huffvalues: Vec<i32>,
}

impl Default for TableDHT {
    fn default() -> Self {
        Self {
            tabdef: 0,
            huffbits: vec![0; MAX_HUFFBITS],
            huffvalues: vec![0; MAX_HUFFCOUNTS_WSQ + 1],
        }
    }
}

struct Token {
    buffer: Vec<u8>,
    pointer: usize,
    table_dtt: TableDTT,
    table_dqt: TableDQT,
    table_dht: Vec<TableDHT>,
    wtree: Vec<WaveletTree>,
    qtree: Vec<QuantTree>,
}

impl Token {
    fn new(buffer: Vec<u8>) -> Self {
        Self {
            buffer,
            pointer: 0,
            table_dtt: TableDTT::default(),
            table_dqt: TableDQT::default(),
            table_dht: (0..8).map(|_| TableDHT::default()).collect(),
            wtree: Vec::new(),
            qtree: Vec::new(),
        }
    }

    fn read_short(&mut self) -> Result<u16> {
        if self.pointer + 2 > self.buffer.len() {
            bail!("WSQ: unexpected end of buffer reading u16");
        }
        let v = u16::from_be_bytes([self.buffer[self.pointer], self.buffer[self.pointer + 1]]);
        self.pointer += 2;
        Ok(v)
    }

    fn read_byte(&mut self) -> Result<u8> {
        if self.pointer >= self.buffer.len() {
            bail!("WSQ: unexpected end of buffer reading u8");
        }
        let v = self.buffer[self.pointer];
        self.pointer += 1;
        Ok(v)
    }

    /// Reads a 4-byte field as an unsigned 32-bit value widened into `i64`.
    /// DTT filter coefficients are stored as large positive integers (up to
    /// ~4.1e9) with a separate sign byte, so a signed `i32` would overflow
    /// and corrupt the filter taps.
    fn read_int(&mut self) -> Result<i64> {
        if self.pointer + 4 > self.buffer.len() {
            bail!("WSQ: unexpected end of buffer reading i32");
        }
        let v = u32::from_be_bytes([
            self.buffer[self.pointer],
            self.buffer[self.pointer + 1],
            self.buffer[self.pointer + 2],
            self.buffer[self.pointer + 3],
        ]) as i64;
        self.pointer += 4;
        Ok(v)
    }
}

fn read_marker(token: &mut Token, marker_type: u8) -> Result<u16> {
    let raw = token.read_short()?;
    let recognized = WsqMarker::from_u16(raw);
    let allowed = match marker_type {
        1 => matches!(recognized, Some(WsqMarker::SOI)),
        2 => matches!(
            recognized,
            Some(
                WsqMarker::DTT
                    | WsqMarker::DQT
                    | WsqMarker::DHT
                    | WsqMarker::SOF
                    | WsqMarker::COMMENT
                    | WsqMarker::EOI
            )
        ),
        3 => matches!(
            recognized,
            Some(
                WsqMarker::DTT
                    | WsqMarker::DQT
                    | WsqMarker::DHT
                    | WsqMarker::SOB
                    | WsqMarker::COMMENT
                    | WsqMarker::EOI
            )
        ),
        _ => recognized.is_some(),
    };
    if !allowed {
        bail!(
            "WSQ: marker 0x{:04X} not allowed in this context (type {})",
            raw,
            marker_type
        );
    }
    Ok(raw)
}

fn int_sign(power: i32) -> f32 {
    if power == 0 {
        return 1.0;
    }
    let mut num = -1.0_f32;
    for _ in 1..power {
        num *= -1.0;
    }
    num
}

fn read_transform_table(token: &mut Token) -> Result<()> {
    let _header_size = token.read_short()?;
    token.table_dtt.hisz = token.read_byte()? as i32;
    token.table_dtt.losz = token.read_byte()? as i32;
    token.table_dtt.hifilt = vec![0.0; token.table_dtt.hisz as usize];
    token.table_dtt.lofilt = vec![0.0; token.table_dtt.losz as usize];

    let a_size = if token.table_dtt.hisz % 2 != 0 {
        (token.table_dtt.hisz + 1) / 2
    } else {
        token.table_dtt.hisz / 2
    };
    let mut a_lofilt = vec![0.0_f32; a_size as usize];
    let last = a_size - 1;
    for cnt in 0..a_size {
        let sign = token.read_byte()?;
        let scale = token.read_byte()?;
        let mut v = token.read_int()? as f32;
        for _ in 0..scale {
            v /= 10.0;
        }
        if sign != 0 {
            v = -v;
        }
        a_lofilt[cnt as usize] = v;
        let idx = (cnt + last) as usize;
        if token.table_dtt.hisz % 2 != 0 {
            token.table_dtt.hifilt[idx] = int_sign(cnt) * v;
            if cnt > 0 {
                token.table_dtt.hifilt[(last - cnt) as usize] =
                    token.table_dtt.hifilt[idx];
            }
        } else {
            token.table_dtt.hifilt[(cnt + last + 1) as usize] = int_sign(cnt) * v;
            token.table_dtt.hifilt[(last - cnt) as usize] =
                -token.table_dtt.hifilt[(cnt + last + 1) as usize];
        }
    }

    let b_size = if token.table_dtt.losz % 2 != 0 {
        (token.table_dtt.losz + 1) / 2
    } else {
        token.table_dtt.losz / 2
    };
    let mut a_hifilt = vec![0.0_f32; b_size as usize];
    let last2 = b_size - 1;
    for cnt in 0..b_size {
        let sign = token.read_byte()?;
        let scale = token.read_byte()?;
        let mut v = token.read_int()? as f32;
        for _ in 0..scale {
            v /= 10.0;
        }
        if sign != 0 {
            v = -v;
        }
        a_hifilt[cnt as usize] = v;
        let idx = (cnt + last2) as usize;
        if token.table_dtt.losz % 2 != 0 {
            token.table_dtt.lofilt[idx] = int_sign(cnt) * v;
            if cnt > 0 {
                token.table_dtt.lofilt[(last2 - cnt) as usize] =
                    token.table_dtt.lofilt[idx];
            }
        } else {
            token.table_dtt.lofilt[(cnt + last2 + 1) as usize] = int_sign(cnt + 1) * v;
            token.table_dtt.lofilt[(last2 - cnt) as usize] =
                token.table_dtt.lofilt[(cnt + last2 + 1) as usize];
        }
    }

    token.table_dtt.lodef = 1;
    token.table_dtt.hidef = 1;
    Ok(())
}

fn read_quantization_table(token: &mut Token) -> Result<()> {
    let _header_size = token.read_short()?;
    let mut scale = token.read_byte()?;
    let shrt_dat = token.read_short()? as f32;
    let mut bin_center = shrt_dat;
    while scale > 0 {
        bin_center /= 10.0;
        scale -= 1;
    }
    token.table_dqt.bin_center = bin_center;

    for cnt in 0..64 {
        let mut s = token.read_byte()?;
        let mut sd = token.read_short()? as f32;
        let mut q = sd;
        while s > 0 {
            q /= 10.0;
            s -= 1;
        }
        token.table_dqt.q_bin[cnt] = q;

        s = token.read_byte()?;
        sd = token.read_short()? as f32;
        let mut z = sd;
        while s > 0 {
            z /= 10.0;
            s -= 1;
        }
        token.table_dqt.z_bin[cnt] = z;
    }
    token.table_dqt.dqt_def = 1;
    Ok(())
}

struct DecodedHuffmanTable {
    table_id: usize,
    huffbits: Vec<u8>,
    huffvalues: Vec<i32>,
}

fn read_huffman_tables(token: &mut Token) -> Result<()> {
    let (first, mut bytes_left) = read_one_huffman_table(token, None)?;
    token.table_dht[first.table_id].huffbits = first.huffbits;
    token.table_dht[first.table_id].huffvalues = first.huffvalues;
    token.table_dht[first.table_id].tabdef = 1;
    while bytes_left > 0 {
        let (next, bl) = read_one_huffman_table(token, Some(bytes_left))?;
        if token.table_dht[next.table_id].tabdef != 0 {
            bail!("WSQ: huffman table {} already defined", next.table_id);
        }
        token.table_dht[next.table_id].huffbits = next.huffbits;
        token.table_dht[next.table_id].huffvalues = next.huffvalues;
        token.table_dht[next.table_id].tabdef = 1;
        bytes_left = bl;
    }
    Ok(())
}

fn read_one_huffman_table(
    token: &mut Token,
    initial_bytes_left: Option<i32>,
) -> Result<(DecodedHuffmanTable, i32)> {
    let mut bytes_left = if let Some(bl) = initial_bytes_left {
        bl
    } else {
        token.read_short()? as i32 - 2
    };
    if bytes_left <= 0 {
        bail!("WSQ: empty huffman table");
    }
    let table_id = token.read_byte()? as usize;
    bytes_left -= 1;
    let mut huffbits = vec![0_u8; MAX_HUFFBITS];
    let mut num_hufvals = 0;
    for slot in huffbits.iter_mut() {
        *slot = token.read_byte()?;
        num_hufvals += *slot as usize;
    }
    bytes_left -= MAX_HUFFBITS as i32;
    if num_hufvals > MAX_HUFFCOUNTS_WSQ + 1 {
        bail!("WSQ: too many huffman values ({})", num_hufvals);
    }
    let mut huffvalues = vec![0_i32; MAX_HUFFCOUNTS_WSQ + 1];
    for slot in huffvalues.iter_mut().take(num_hufvals) {
        *slot = token.read_byte()? as i32;
    }
    bytes_left -= num_hufvals as i32;
    Ok((
        DecodedHuffmanTable {
            table_id,
            huffbits,
            huffvalues,
        },
        bytes_left,
    ))
}

fn read_comment(token: &mut Token, ppi: &mut Option<u32>) -> Result<()> {
    let size = (token.read_short()? as usize).saturating_sub(2);
    if token.pointer + size > token.buffer.len() {
        bail!("WSQ: truncated comment");
    }
    let bytes = &token.buffer[self_end(token.pointer, size, token.buffer.len()) - size..self_end(token.pointer, size, token.buffer.len())];
    token.pointer += size;
    if let Ok(text) = std::str::from_utf8(bytes) {
        for line in text.split(|c| c == '\n' || c == '\r') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Look for both "PPI 500" and "PIX_WIDTH ..." formats.
            for prefix in ["PPI ", "PPI\t"] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    if let Some(num) = rest
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse::<u32>().ok())
                    {
                        *ppi = Some(num);
                    }
                }
            }
        }
    }
    Ok(())
}

fn self_end(start: usize, len: usize, total: usize) -> usize {
    start + len.min(total.saturating_sub(start))
}

fn read_frame_header(token: &mut Token) -> Result<WsqHeader> {
    let _header_size = token.read_short()?;
    let black = token.read_byte()?;
    let white = token.read_byte()?;
    let height = token.read_short()?;
    let width = token.read_short()?;
    let mut scale = token.read_byte()?;
    let mut shrt_dat = token.read_short()? as f32;
    let mut m_shift = shrt_dat;
    while scale > 0 {
        m_shift /= 10.0;
        scale -= 1;
    }
    scale = token.read_byte()?;
    shrt_dat = token.read_short()? as f32;
    let mut r_scale = shrt_dat;
    while scale > 0 {
        r_scale /= 10.0;
        scale -= 1;
    }
    let _wsq_encoder = token.read_byte()?;
    let _software = token.read_short()?;
    Ok(WsqHeader {
        black,
        white,
        width,
        height,
        m_shift,
        r_scale,
    })
}

// ----- Tree construction -----------------------------------------------------

fn build_w_trees(token: &mut Token, width: u32, height: u32) {
    let mut wtree: Vec<WaveletTree> = (0..W_TREELEN)
        .map(|_| WaveletTree::default())
        .collect();
    wtree[2].invrw = 1;
    wtree[4].invrw = 1;
    wtree[7].invrw = 1;
    wtree[9].invrw = 1;
    wtree[11].invrw = 1;
    wtree[13].invrw = 1;
    wtree[16].invrw = 1;
    wtree[18].invrw = 1;
    wtree[3].invcl = 1;
    wtree[5].invcl = 1;
    wtree[8].invcl = 1;
    wtree[9].invcl = 1;
    wtree[12].invcl = 1;
    wtree[13].invcl = 1;
    wtree[17].invcl = 1;
    wtree[18].invcl = 1;

    wtree4(&mut wtree, 0, 1, width as i32, height as i32, 0, 0, 1);

    let (lenx, lenx2) = if wtree[1].lenx % 2 == 0 {
        (wtree[1].lenx / 2, wtree[1].lenx / 2)
    } else {
        ((wtree[1].lenx + 1) / 2, (wtree[1].lenx + 1) / 2 - 1)
    };
    let (leny, leny2) = if wtree[1].leny % 2 == 0 {
        (wtree[1].leny / 2, wtree[1].leny / 2)
    } else {
        ((wtree[1].leny + 1) / 2, (wtree[1].leny + 1) / 2 - 1)
    };

    wtree4(&mut wtree, 4, 6, lenx2, leny, lenx, 0, 0);
    wtree4(&mut wtree, 5, 10, lenx, leny2, 0, leny, 0);
    wtree4(&mut wtree, 14, 15, lenx, leny, 0, 0, 0);

    wtree[19].x = 0;
    wtree[19].y = 0;
    wtree[19].lenx = if wtree[15].lenx % 2 == 0 {
        wtree[15].lenx / 2
    } else {
        (wtree[15].lenx + 1) / 2
    };
    wtree[19].leny = if wtree[15].leny % 2 == 0 {
        wtree[15].leny / 2
    } else {
        (wtree[15].leny + 1) / 2
    };

    token.wtree = wtree;
}

fn wtree4(
    wtree: &mut [WaveletTree],
    start1: usize,
    start2: usize,
    lenx: i32,
    leny: i32,
    x: i32,
    y: i32,
    stop1: i32,
) {
    let evenx = lenx % 2;
    let eveny = leny % 2;
    let p1 = start1;
    let p2 = start2;

    wtree[p1].x = x;
    wtree[p1].y = y;
    wtree[p1].lenx = lenx;
    wtree[p1].leny = leny;

    wtree[p2].x = x;
    wtree[p2 + 2].x = x;
    wtree[p2].y = y;
    wtree[p2 + 1].y = y;

    if evenx == 0 {
        wtree[p2].lenx = lenx / 2;
        wtree[p2 + 1].lenx = wtree[p2].lenx;
    } else if p1 == 4 {
        wtree[p2].lenx = (lenx - 1) / 2;
        wtree[p2 + 1].lenx = wtree[p2].lenx + 1;
    } else {
        wtree[p2].lenx = (lenx + 1) / 2;
        wtree[p2 + 1].lenx = wtree[p2].lenx - 1;
    }
    wtree[p2 + 1].x = wtree[p2].lenx + x;
    if stop1 == 0 {
        wtree[p2 + 3].lenx = wtree[p2 + 1].lenx;
        wtree[p2 + 3].x = wtree[p2 + 1].x;
    }
    wtree[p2 + 2].lenx = wtree[p2].lenx;

    if eveny == 0 {
        wtree[p2].leny = leny / 2;
        wtree[p2 + 2].leny = wtree[p2].leny;
    } else if p1 == 5 {
        wtree[p2].leny = (leny - 1) / 2;
        wtree[p2 + 2].leny = wtree[p2].leny + 1;
    } else {
        wtree[p2].leny = (leny + 1) / 2;
        wtree[p2 + 2].leny = wtree[p2].leny - 1;
    }
    wtree[p2 + 2].y = wtree[p2].leny + y;
    if stop1 == 0 {
        wtree[p2 + 3].leny = wtree[p2 + 2].leny;
        wtree[p2 + 3].y = wtree[p2 + 2].y;
    }
    wtree[p2 + 1].leny = wtree[p2].leny;
}

fn build_q_trees(token: &mut Token) {
    let mut qtree: Vec<QuantTree> = (0..Q_TREELEN).map(|_| QuantTree::default()).collect();
    let w = token.wtree.clone();
    qtree16(&mut qtree, 3, w[14].lenx, w[14].leny, w[14].x, w[14].y, 0, 0);
    qtree16(&mut qtree, 19, w[4].lenx, w[4].leny, w[4].x, w[4].y, 0, 1);
    qtree16(&mut qtree, 48, w[0].lenx, w[0].leny, w[0].x, w[0].y, 0, 0);
    qtree16(&mut qtree, 35, w[5].lenx, w[5].leny, w[5].x, w[5].y, 1, 0);
    qtree4(&mut qtree, 0, w[19].lenx, w[19].leny, w[19].x, w[19].y);
    token.qtree = qtree;
}

fn qtree16(
    qtree: &mut [QuantTree],
    start: usize,
    lenx: i32,
    leny: i32,
    x: i32,
    y: i32,
    rw: i32,
    cl: i32,
) {
    let evenx = lenx % 2;
    let eveny = leny % 2;
    let p = start;

    let (tempx, temp2x) = if evenx == 0 {
        (lenx / 2, lenx / 2)
    } else if cl != 0 {
        ((lenx + 1) / 2 - 1, (lenx + 1) / 2)
    } else {
        ((lenx + 1) / 2, (lenx + 1) / 2 - 1)
    };
    let (tempy, temp2y) = if eveny == 0 {
        (leny / 2, leny / 2)
    } else if rw != 0 {
        ((leny + 1) / 2 - 1, (leny + 1) / 2)
    } else {
        ((leny + 1) / 2, (leny + 1) / 2 - 1)
    };

    let evenx = tempx % 2;
    let eveny = tempy % 2;
    qtree[p].x = x;
    qtree[p + 2].x = x;
    qtree[p].y = y;
    qtree[p + 1].y = y;
    if evenx == 0 {
        qtree[p].lenx = tempx / 2;
        qtree[p + 1].lenx = qtree[p].lenx;
        qtree[p + 2].lenx = qtree[p].lenx;
        qtree[p + 3].lenx = qtree[p].lenx;
    } else {
        qtree[p].lenx = (tempx + 1) / 2;
        qtree[p + 1].lenx = qtree[p].lenx - 1;
        qtree[p + 2].lenx = qtree[p].lenx;
        qtree[p + 3].lenx = qtree[p + 1].lenx;
    }
    qtree[p + 1].x = x + qtree[p].lenx;
    qtree[p + 3].x = qtree[p + 1].x;
    if eveny == 0 {
        qtree[p].leny = tempy / 2;
        qtree[p + 1].leny = qtree[p].leny;
        qtree[p + 2].leny = qtree[p].leny;
        qtree[p + 3].leny = qtree[p].leny;
    } else {
        qtree[p].leny = (tempy + 1) / 2;
        qtree[p + 1].leny = qtree[p].leny;
        qtree[p + 2].leny = qtree[p].leny - 1;
        qtree[p + 3].leny = qtree[p + 2].leny;
    }
    qtree[p + 2].y = y + qtree[p].leny;
    qtree[p + 3].y = qtree[p + 2].y;

    let evenx = temp2x % 2;
    qtree[p + 4].x = x + tempx;
    qtree[p + 6].x = qtree[p + 4].x;
    qtree[p + 4].y = y;
    qtree[p + 5].y = y;
    qtree[p + 6].y = qtree[p + 2].y;
    qtree[p + 7].y = qtree[p + 2].y;
    qtree[p + 4].leny = qtree[p].leny;
    qtree[p + 5].leny = qtree[p].leny;
    qtree[p + 6].leny = qtree[p + 2].leny;
    qtree[p + 7].leny = qtree[p + 2].leny;
    if evenx == 0 {
        qtree[p + 4].lenx = temp2x / 2;
        qtree[p + 5].lenx = qtree[p + 4].lenx;
        qtree[p + 6].lenx = qtree[p + 4].lenx;
        qtree[p + 7].lenx = qtree[p + 4].lenx;
    } else {
        qtree[p + 5].lenx = (temp2x + 1) / 2;
        qtree[p + 4].lenx = qtree[p + 5].lenx - 1;
        qtree[p + 6].lenx = qtree[p + 4].lenx;
        qtree[p + 7].lenx = qtree[p + 5].lenx;
    }
    qtree[p + 5].x = qtree[p + 4].x + qtree[p + 4].lenx;
    qtree[p + 7].x = qtree[p + 5].x;

    let eveny = temp2y % 2;
    qtree[p + 8].x = x;
    qtree[p + 9].x = qtree[p + 1].x;
    qtree[p + 10].x = x;
    qtree[p + 11].x = qtree[p + 1].x;
    qtree[p + 8].y = y + tempy;
    qtree[p + 9].y = qtree[p + 8].y;
    qtree[p + 8].lenx = qtree[p].lenx;
    qtree[p + 9].lenx = qtree[p + 1].lenx;
    qtree[p + 10].lenx = qtree[p].lenx;
    qtree[p + 11].lenx = qtree[p + 1].lenx;
    if eveny == 0 {
        qtree[p + 8].leny = temp2y / 2;
        qtree[p + 9].leny = qtree[p + 8].leny;
        qtree[p + 10].leny = qtree[p + 8].leny;
        qtree[p + 11].leny = qtree[p + 8].leny;
    } else {
        qtree[p + 10].leny = (temp2y + 1) / 2;
        qtree[p + 11].leny = qtree[p + 10].leny;
        qtree[p + 8].leny = qtree[p + 10].leny - 1;
        qtree[p + 9].leny = qtree[p + 8].leny;
    }
    qtree[p + 10].y = qtree[p + 8].y + qtree[p + 8].leny;
    qtree[p + 11].y = qtree[p + 10].y;

    qtree[p + 12].x = qtree[p + 4].x;
    qtree[p + 13].x = qtree[p + 5].x;
    qtree[p + 14].x = qtree[p + 4].x;
    qtree[p + 15].x = qtree[p + 5].x;
    qtree[p + 12].y = qtree[p + 8].y;
    qtree[p + 13].y = qtree[p + 8].y;
    qtree[p + 14].y = qtree[p + 10].y;
    qtree[p + 15].y = qtree[p + 10].y;
    qtree[p + 12].lenx = qtree[p + 4].lenx;
    qtree[p + 13].lenx = qtree[p + 5].lenx;
    qtree[p + 14].lenx = qtree[p + 4].lenx;
    qtree[p + 15].lenx = qtree[p + 5].lenx;
    qtree[p + 12].leny = qtree[p + 8].leny;
    qtree[p + 13].leny = qtree[p + 8].leny;
    qtree[p + 14].leny = qtree[p + 10].leny;
    qtree[p + 15].leny = qtree[p + 10].leny;
}

fn qtree4(qtree: &mut [QuantTree], start: usize, lenx: i32, leny: i32, x: i32, y: i32) {
    let evenx = lenx % 2;
    let eveny = leny % 2;
    let p = start;
    qtree[p].x = x;
    qtree[p + 2].x = x;
    qtree[p].y = y;
    qtree[p + 1].y = y;
    if evenx == 0 {
        qtree[p].lenx = lenx / 2;
        qtree[p + 1].lenx = qtree[p].lenx;
        qtree[p + 2].lenx = qtree[p].lenx;
        qtree[p + 3].lenx = qtree[p].lenx;
    } else {
        qtree[p].lenx = (lenx + 1) / 2;
        qtree[p + 1].lenx = qtree[p].lenx - 1;
        qtree[p + 2].lenx = qtree[p].lenx;
        qtree[p + 3].lenx = qtree[p + 1].lenx;
    }
    qtree[p + 1].x = x + qtree[p].lenx;
    qtree[p + 3].x = qtree[p + 1].x;
    if eveny == 0 {
        qtree[p].leny = leny / 2;
        qtree[p + 1].leny = qtree[p].leny;
        qtree[p + 2].leny = qtree[p].leny;
        qtree[p + 3].leny = qtree[p].leny;
    } else {
        qtree[p].leny = (leny + 1) / 2;
        qtree[p + 1].leny = qtree[p].leny;
        qtree[p + 2].leny = qtree[p].leny - 1;
        qtree[p + 3].leny = qtree[p + 2].leny;
    }
    qtree[p + 2].y = y + qtree[p].leny;
    qtree[p + 3].y = qtree[p + 2].y;
}

// ----- Huffman decoding -----------------------------------------------------

fn read_block_header(token: &mut Token) -> Result<usize> {
    let _header_size = token.read_short()?;
    let table_id = token.read_byte()? as usize;
    Ok(table_id)
}

struct HuffCode {
    size: u8,
    code: u16,
}

fn build_huff_sizes(huffbits: &[u8]) -> Vec<HuffCode> {
    let mut table: Vec<HuffCode> = Vec::new();
    let mut number_of_codes: i32 = 1;
    for code_size in 1..=MAX_HUFFBITS {
        while number_of_codes <= huffbits[code_size - 1] as i32 {
            table.push(HuffCode {
                size: code_size as u8,
                code: 0,
            });
            number_of_codes += 1;
        }
        number_of_codes = 1;
    }
    table.push(HuffCode { size: 0, code: 0 });
    table
}

fn build_huff_codes(table: &mut [HuffCode]) {
    if table.is_empty() || table[0].size == 0 {
        return;
    }
    let mut temp_code: u16 = 0;
    let mut pointer = 0;
    let mut temp_size = table[0].size;
    loop {
        while table[pointer].size == temp_size {
            table[pointer].code = temp_code;
            temp_code += 1;
            pointer += 1;
            if pointer >= table.len() {
                return;
            }
        }
        if table[pointer].size == 0 {
            return;
        }
        while table[pointer].size != temp_size {
            temp_code <<= 1;
            temp_size += 1;
        }
    }
}

struct DecodeTable {
    maxcode: [i32; MAX_HUFFBITS + 2],
    mincode: [i32; MAX_HUFFBITS + 2],
    valptr: [i32; MAX_HUFFBITS + 2],
}

fn build_decode_table(huffbits: &[u8], hufftable: &[HuffCode]) -> DecodeTable {
    let mut dt = DecodeTable {
        maxcode: [-1; MAX_HUFFBITS + 2],
        mincode: [-1; MAX_HUFFBITS + 2],
        valptr: [-1; MAX_HUFFBITS + 2],
    };
    let mut i2 = 0;
    for i in 1..=MAX_HUFFBITS {
        if huffbits[i - 1] == 0 {
            dt.maxcode[i] = -1;
            continue;
        }
        dt.valptr[i] = i2 as i32;
        dt.mincode[i] = hufftable[i2].code as i32;
        i2 += huffbits[i - 1] as usize - 1;
        dt.maxcode[i] = hufftable[i2].code as i32;
        i2 += 1;
    }
    dt
}

fn mask_for_bits(bits: u8) -> u8 {
    match bits {
        0 => 0x00,
        1 => 0x01,
        2 => 0x03,
        3 => 0x07,
        4 => 0x0F,
        5 => 0x1F,
        6 => 0x3F,
        7 => 0x7F,
        _ => 0xFF,
    }
}

fn next_bits(
    token: &mut Token,
    marker: &mut u16,
    bit_count: &mut u8,
    next_byte: &mut u8,
    bits_req: u8,
) -> Result<i32> {
    if *bit_count == 0 {
        let b = token.read_byte()?;
        *next_byte = b;
        *bit_count = 8;
        if b == 0xFF {
            let code2 = token.read_byte()?;
            if code2 != 0x00 && bits_req == 1 {
                *marker = ((b as u16) << 8) | code2 as u16;
                return Ok(1);
            }
            if code2 != 0x00 {
                bail!("WSQ: missing stuffed zero after 0xFF");
            }
        }
    }
    if bits_req <= *bit_count {
        let mask = mask_for_bits(bits_req);
        let bits = ((*next_byte >> (*bit_count - bits_req)) & mask) as i32;
        *bit_count -= bits_req;
        *next_byte &= mask_for_bits(*bit_count);
        return Ok(bits);
    }
    let bits_needed = bits_req - *bit_count;
    let bits = (*next_byte as i32) << bits_needed;
    *bit_count = 0;
    let tbits = next_bits(token, marker, bit_count, next_byte, bits_needed)?;
    Ok(bits | tbits)
}

fn decode_data_mem(
    token: &mut Token,
    marker: &mut u16,
    bit_count: &mut u8,
    next_byte: &mut u8,
    dt: &DecodeTable,
    huffvalues: &[i32],
) -> Result<i32> {
    let mut code = next_bits(token, marker, bit_count, next_byte, 1)? as u16;
    if *marker != 0 {
        return Ok(-1);
    }
    let mut inx = 1;
    while (code as i32) > dt.maxcode[inx] {
        let tbits = next_bits(token, marker, bit_count, next_byte, 1)? as u16;
        code = (code << 1) + tbits;
        if *marker != 0 {
            return Ok(-1);
        }
        inx += 1;
        if inx > MAX_HUFFBITS + 1 {
            bail!("WSQ: huffman code too long");
        }
    }
    let inx2 = (dt.valptr[inx] + code as i32 - dt.mincode[inx]) as usize;
    Ok(huffvalues[inx2])
}

fn huffman_decode(token: &mut Token, total_pixels: usize) -> Result<Vec<i32>> {
    let mut qdata = vec![0_i32; total_pixels];
    let mut marker = read_marker(token, 3)?;
    let mut bit_count: u8 = 0;
    let mut next_byte: u8 = 0;
    let mut ip = 0;

    // We rebuild the per-block decode tables lazily as soon as we hit SOB.
    let mut hufftable: Vec<HuffCode>;
    let mut decode_table = DecodeTable {
        maxcode: [-1; MAX_HUFFBITS + 2],
        mincode: [-1; MAX_HUFFBITS + 2],
        valptr: [-1; MAX_HUFFBITS + 2],
    };
    let mut huffvalues: Vec<i32> = Vec::new();

    while marker != WsqMarker::EOI as u16 {
        if marker != 0 {
            while marker != WsqMarker::SOB as u16 {
                match WsqMarker::from_u16(marker) {
                    Some(WsqMarker::DTT) => read_transform_table(token)?,
                    Some(WsqMarker::DQT) => read_quantization_table(token)?,
                    Some(WsqMarker::DHT) => read_huffman_tables(token)?,
                    Some(WsqMarker::COMMENT) => {
                        let mut _ppi: Option<u32> = None;
                        read_comment(token, &mut _ppi)?;
                    }
                    _ => {}
                }
                marker = read_marker(token, 3)?;
                if marker == WsqMarker::EOI as u16 {
                    break;
                }
            }
            if marker == WsqMarker::EOI as u16 {
                break;
            }
            let hufftable_id = read_block_header(token)?;
            if token.table_dht[hufftable_id].tabdef != 1 {
                bail!("WSQ: huffman table {} not defined", hufftable_id);
            }
            hufftable = build_huff_sizes(&token.table_dht[hufftable_id].huffbits);
            build_huff_codes(&mut hufftable);
            decode_table = build_decode_table(
                &token.table_dht[hufftable_id].huffbits,
                &hufftable,
            );
            huffvalues = token.table_dht[hufftable_id].huffvalues.clone();
            bit_count = 0;
            marker = 0;
        }

        let nodeptr = decode_data_mem(
            token,
            &mut marker,
            &mut bit_count,
            &mut next_byte,
            &decode_table,
            &huffvalues,
        )?;
        if nodeptr == -1 {
            continue;
        }
        if nodeptr > 0 && nodeptr <= 100 {
            for _ in 0..nodeptr {
                if ip < qdata.len() {
                    qdata[ip] = 0;
                    ip += 1;
                }
            }
        } else if nodeptr > 106 && nodeptr < 0xff {
            if ip < qdata.len() {
                qdata[ip] = nodeptr - 180;
                ip += 1;
            }
        } else if nodeptr == 101 {
            let v = next_bits(token, &mut marker, &mut bit_count, &mut next_byte, 8)?;
            if ip < qdata.len() {
                qdata[ip] = v;
                ip += 1;
            }
        } else if nodeptr == 102 {
            let v = next_bits(token, &mut marker, &mut bit_count, &mut next_byte, 8)?;
            if ip < qdata.len() {
                qdata[ip] = -v;
                ip += 1;
            }
        } else if nodeptr == 103 {
            let v = next_bits(token, &mut marker, &mut bit_count, &mut next_byte, 16)?;
            if ip < qdata.len() {
                qdata[ip] = v;
                ip += 1;
            }
        } else if nodeptr == 104 {
            let v = next_bits(token, &mut marker, &mut bit_count, &mut next_byte, 16)?;
            if ip < qdata.len() {
                qdata[ip] = -v;
                ip += 1;
            }
        } else if nodeptr == 105 {
            let mut n = next_bits(token, &mut marker, &mut bit_count, &mut next_byte, 8)?;
            while ip < qdata.len() && n > 0 {
                qdata[ip] = 0;
                ip += 1;
                n -= 1;
            }
        } else if nodeptr == 106 {
            let mut n = next_bits(token, &mut marker, &mut bit_count, &mut next_byte, 16)?;
            while ip < qdata.len() && n > 0 {
                qdata[ip] = 0;
                ip += 1;
                n -= 1;
            }
        } else {
            bail!("WSQ: invalid huffman code {}", nodeptr);
        }
    }
    Ok(qdata)
}

// ----- Reconstruction -------------------------------------------------------

fn unquantize(token: &Token, sip: &[i32], width: i32, height: i32) -> Vec<f32> {
    if token.table_dqt.dqt_def != 1 {
        return Err(anyhow!("unquantize: quantization table missing")).unwrap();
    }
    let bin_center = token.table_dqt.bin_center;
    let mut fip = vec![0.0_f32; (width * height) as usize];
    let mut sptr = 0;
    for cnt in 0..NUM_SUBBANDS {
        if token.table_dqt.q_bin[cnt] == 0.0 {
            continue;
        }
        let mut fptr = (token.qtree[cnt].y * width + token.qtree[cnt].x) as usize;
        for _ in 0..token.qtree[cnt].leny {
            for _ in 0..token.qtree[cnt].lenx {
                let v = sip[sptr];
                if v == 0 {
                    fip[fptr] = 0.0;
                } else if v > 0 {
                    fip[fptr] =
                        token.table_dqt.q_bin[cnt] * (v as f32 - bin_center)
                            + token.table_dqt.z_bin[cnt] / 2.0;
                } else {
                    fip[fptr] =
                        token.table_dqt.q_bin[cnt] * (v as f32 + bin_center)
                            - token.table_dqt.z_bin[cnt] / 2.0;
                }
                fptr += 1;
                sptr += 1;
            }
            // Advance to the next row of this subband: at this point fptr
            // sits at row_start + lenx, so add the remaining row width.
            fptr += width as usize - token.qtree[cnt].lenx as usize;
        }
    }
    fip
}

/// Apply the inverse 1-D wavelet transform along either rows or columns.
///
/// This is a faithful port of the `join_lets` routine from the NIST NBIS
/// reference implementation (also used by jnbis). `new_index` / `old_index`
/// are byte offsets into the two pixel buffers.
fn join_lets(
    newdata: &mut [f32],
    olddata: &[f32],
    new_index: usize,
    old_index: usize,
    len1: i32,
    len2: i32,
    pitch: usize,
    stride: usize,
    hi: &mut [f32],
    lo: &[f32],
    inv: i32,
) {
    let lsz = lo.len() as i32;
    let hsz = hi.len() as i32;
    let da_ev = len2 % 2;
    let fi_ev = lsz % 2;
    let pstr = stride as i32;
    let nstr: i32 = -pstr;
    let (llen, hlen) = if da_ev != 0 {
        ((len2 + 1) / 2, (len2 + 1) / 2 - 1)
    } else {
        (len2 / 2, len2 / 2)
    };

    let asym: i32;
    let ssfac: f32;
    let ofhre: i32;
    let mut loc: i32;
    let mut hoc: i32;
    let lotap: i32;
    let hotap: i32;
    let mut olle: i32;
    let olre: i32;
    let mut ohle: i32;
    let ohre: i32;
    if fi_ev != 0 {
        asym = 0;
        ssfac = 1.0;
        ofhre = 0;
        loc = (lsz - 1) / 4;
        hoc = (hsz + 1) / 4 - 1;
        lotap = ((lsz - 1) / 2) % 2;
        hotap = ((hsz + 1) / 2) % 2;
        if da_ev != 0 {
            olle = 0;
            olre = 0;
            ohle = 1;
            ohre = 1;
        } else {
            olle = 0;
            olre = 1;
            ohle = 1;
            ohre = 0;
        }
    } else {
        asym = 1;
        ssfac = -1.0;
        ofhre = 2;
        loc = lsz / 4 - 1;
        hoc = hsz / 4 - 1;
        lotap = (lsz / 2) % 2;
        hotap = (hsz / 2) % 2;
        if da_ev != 0 {
            olle = 1;
            olre = 0;
            ohle = 1;
            ohre = 1;
        } else {
            olle = 1;
            olre = 1;
            ohle = 1;
            ohre = 1;
        }
        if loc == -1 {
            loc = 0;
            olle = 0;
        }
        if hoc == -1 {
            hoc = 0;
            ohle = 0;
        }
        for v in hi.iter_mut() {
            *v *= -1.0;
        }
    }

    // Bounds-checked read of the source buffer (indices are absolute).
    let get = |data: &[f32], idx: i32| -> f32 {
        if idx < 0 || idx as usize >= data.len() {
            0.0
        } else {
            data[idx as usize]
        }
    };

    for cl_rw in 0..len1 {
        let mut limg = new_index + cl_rw as usize * pitch;
        let mut himg = limg;
        if limg < newdata.len() {
            newdata[limg] = 0.0;
        }
        if himg + stride < newdata.len() {
            newdata[himg + stride] = 0.0;
        }
        let hipass_base = old_index + cl_rw as usize * pitch;
        let (lopass, hipass) = if inv != 0 {
            (
                (hipass_base + stride * hlen as usize) as i32,
                hipass_base as i32,
            )
        } else {
            (
                hipass_base as i32,
                (hipass_base + stride * llen as usize) as i32,
            )
        };

        let lp0 = lopass;
        let lp1 = lp0 + (llen - 1) * stride as i32;
        let mut lspx = lp0 + loc * stride as i32;
        let mut lspxstr = nstr;
        let mut lstap = lotap;
        let mut lle2 = olle;
        let lre2 = olre;

        let hp0 = hipass;
        let hp1 = hp0 + (hlen - 1) * stride as i32;
        let mut hspx = hp0 + hoc * stride as i32;
        let mut hspxstr = nstr;
        let mut hstap = hotap;
        let mut hle2 = ohle;
        let hre2 = ohre;
        let mut osfac = ssfac;

        for _pix in 0..hlen {
            // Low-pass taps.
            let mut tap = lstap;
            while tap >= 0 {
                let mut lle = lle2;
                let mut lre = lre2;
                let mut lpx = lspx;
                let mut lpxstr = lspxstr;

                if limg < newdata.len() {
                    newdata[limg] = get(olddata, lpx) * lo[tap as usize];
                }
                let mut i = tap + 2;
                while i < lsz {
                    if lpx == lp0 {
                        if lle != 0 {
                            lpxstr = 0;
                            lle = 0;
                        } else {
                            lpxstr = pstr;
                        }
                    }
                    if lpx == lp1 {
                        if lre != 0 {
                            lpxstr = 0;
                            lre = 0;
                        } else {
                            lpxstr = nstr;
                        }
                    }
                    lpx += lpxstr;
                    if limg < newdata.len() {
                        newdata[limg] += get(olddata, lpx) * lo[i as usize];
                    }
                    i += 2;
                }
                limg += stride;
                tap -= 1;
            }
            if lspx == lp0 {
                if lle2 != 0 {
                    lspxstr = 0;
                    lle2 = 0;
                } else {
                    lspxstr = pstr;
                }
            }
            lspx += lspxstr;
            lstap = 1;

            // High-pass taps.
            let mut tap = hstap;
            while tap >= 0 {
                let mut hle = hle2;
                let mut hre = hre2;
                let mut hpx = hspx;
                let mut hpxstr = hspxstr;
                let mut fhre = ofhre;
                let mut sfac = osfac;

                let mut i = tap;
                while i < hsz {
                    if hpx == hp0 {
                        if hle != 0 {
                            hpxstr = 0;
                            hle = 0;
                        } else {
                            hpxstr = pstr;
                            sfac = 1.0;
                        }
                    }
                    if hpx == hp1 {
                        if hre != 0 {
                            hpxstr = 0;
                            hre = 0;
                            if asym != 0 && da_ev != 0 {
                                hre = 1;
                                fhre -= 1;
                                sfac = fhre as f32;
                                if sfac == 0.0 {
                                    hre = 0;
                                }
                            }
                        } else {
                            hpxstr = nstr;
                            if asym != 0 {
                                sfac = -1.0;
                            }
                        }
                    }
                    if himg < newdata.len() {
                        newdata[himg] += get(olddata, hpx) * hi[i as usize] * sfac;
                    }
                    hpx += hpxstr;
                    i += 2;
                }
                himg += stride;
                tap -= 1;
            }
            if hspx == hp0 {
                if hle2 != 0 {
                    hspxstr = 0;
                    hle2 = 0;
                } else {
                    hspxstr = pstr;
                    osfac = 1.0;
                }
            }
            hspx += hspxstr;
            hstap = 1;
        }

        // Trailing low-pass taps.
        if da_ev != 0 {
            lstap = if lotap != 0 { 1 } else { 0 };
        } else if lotap != 0 {
            lstap = 2;
        } else {
            lstap = 1;
        }
        let mut tap = 1;
        while tap >= lstap {
            let mut lle = lle2;
            let mut lre = lre2;
            let mut lpx = lspx;
            let mut lpxstr = lspxstr;

            if limg < newdata.len() {
                newdata[limg] = get(olddata, lpx) * lo[tap as usize];
            }
            let mut i = tap + 2;
            while i < lsz {
                if lpx == lp0 {
                    if lle != 0 {
                        lpxstr = 0;
                        lle = 0;
                    } else {
                        lpxstr = pstr;
                    }
                }
                if lpx == lp1 {
                    if lre != 0 {
                        lpxstr = 0;
                        lre = 0;
                    } else {
                        lpxstr = nstr;
                    }
                }
                lpx += lpxstr;
                if limg < newdata.len() {
                    newdata[limg] += get(olddata, lpx) * lo[i as usize];
                }
                i += 2;
            }
            limg += stride;
            tap -= 1;
        }

        // Trailing high-pass taps.
        let mut fhre2: i32 = 0;
        if da_ev != 0 {
            if hotap != 0 {
                hstap = 1;
            } else {
                hstap = 0;
            }
            if hsz == 2 {
                hspx -= hspxstr;
                fhre2 = 1;
            }
        } else if hotap != 0 {
            hstap = 2;
        } else {
            hstap = 1;
        }
        let mut tap = 1;
        while tap >= hstap {
            let mut hle = hle2;
            let mut hre = hre2;
            let mut hpx = hspx;
            let mut hpxstr = hspxstr;
            let mut sfac = osfac;
            let mut fhre = if hsz != 2 { ofhre } else { fhre2 };

            let mut i = tap;
            while i < hsz {
                if hpx == hp0 {
                    if hle != 0 {
                        hpxstr = 0;
                        hle = 0;
                    } else {
                        hpxstr = pstr;
                        sfac = 1.0;
                    }
                }
                if hpx == hp1 {
                    if hre != 0 {
                        hpxstr = 0;
                        hre = 0;
                        if asym != 0 && da_ev != 0 {
                            hre = 1;
                            fhre -= 1;
                            sfac = fhre as f32;
                            if sfac == 0.0 {
                                hre = 0;
                            }
                        }
                    } else {
                        hpxstr = nstr;
                        if asym != 0 {
                            sfac = -1.0;
                        }
                    }
                }
                if himg < newdata.len() {
                    newdata[himg] += get(olddata, hpx) * hi[i as usize] * sfac;
                }
                hpx += hpxstr;
                i += 2;
            }
            himg += stride;
            tap -= 1;
        }
    }

    // Restore the high-pass sign flip so the next call sees the original filters.
    if fi_ev == 0 {
        for v in hi.iter_mut() {
            *v *= -1.0;
        }
    }
}

/// Apply the 2-D inverse wavelet transform on the full image buffer.
fn wsq_reconstruct(token: &mut Token, fdata: &mut Vec<f32>, width: i32, height: i32) {
    if token.table_dtt.lodef != 1 || token.table_dtt.hidef != 1 {
        // Without filter tables we cannot reconstruct — leave the image in its
        // unquantized form so the caller at least sees the shape.
        return;
    }
    let num_pix = (width * height) as usize;
    let mut fdata_temp = vec![0.0_f32; num_pix];
    let lo = token.table_dtt.lofilt.clone();
    let mut hi = token.table_dtt.hifilt.clone();
    for node in (0..W_TREELEN).rev() {
        let fdata_bse = (token.wtree[node].y * width + token.wtree[node].x) as usize;
        // Column-wise filter.
        join_lets(
            &mut fdata_temp,
            fdata,
            0,
            fdata_bse,
            token.wtree[node].lenx,
            token.wtree[node].leny,
            1,
            width as usize,
            &mut hi,
            &lo,
            token.wtree[node].invcl,
        );
        // Row-wise filter.
        join_lets(
            fdata,
            &mut fdata_temp,
            fdata_bse,
            0,
            token.wtree[node].leny,
            token.wtree[node].lenx,
            width as usize,
            1,
            &mut hi,
            &lo,
            token.wtree[node].invrw,
        );
    }
}

fn convert_image_to_byte(
    img: &[f32],
    width: i32,
    height: i32,
    m_shift: f32,
    r_scale: f32,
) -> Vec<u8> {
    let mut out = vec![0_u8; (width * height) as usize];
    for (idx, slot) in out.iter_mut().enumerate() {
        let mut pixel = img[idx] * r_scale + m_shift + 0.5;
        if pixel < 0.0 {
            pixel = 0.0;
        } else if pixel > 255.0 {
            pixel = 255.0;
        }
        *slot = pixel as u8;
    }
    out
}

/// Decode a WSQ byte buffer into an 8-bit grayscale image.
pub fn decode(bytes: &[u8]) -> Result<DecodedWsq> {
    if bytes.len() < 4 {
        bail!("WSQ: buffer too small");
    }
    if bytes[0] != 0xFF || bytes[1] != 0xA0 {
        bail!("WSQ: not a WSQ file (missing SOI marker)");
    }
    let mut token = Token::new(bytes.to_vec());
    let mut ppi: Option<u32> = None;
    read_marker(&mut token, 1)?;
    let mut marker = read_marker(&mut token, 2)?;
    while let Some(m) = WsqMarker::from_u16(marker) {
        if m == WsqMarker::SOF {
            break;
        }
        match m {
            WsqMarker::DTT => read_transform_table(&mut token)?,
            WsqMarker::DQT => read_quantization_table(&mut token)?,
            WsqMarker::DHT => read_huffman_tables(&mut token)?,
            WsqMarker::COMMENT => read_comment(&mut token, &mut ppi)?,
            _ => {}
        }
        marker = read_marker(&mut token, 2)?;
    }
    let header = read_frame_header(&mut token)?;
    build_w_trees(&mut token, header.width as u32, header.height as u32);
    build_q_trees(&mut token);
    let total = (header.width as i32 * header.height as i32) as usize;
    let qdata = huffman_decode(&mut token, total)?;
    let mut fdata = unquantize(&token, &qdata, header.width as i32, header.height as i32);
    wsq_reconstruct(&mut token, &mut fdata, header.width as i32, header.height as i32);
    let pixels =
        convert_image_to_byte(&fdata, header.width as i32, header.height as i32, header.m_shift, header.r_scale);
    Ok(DecodedWsq {
        width: header.width as u32,
        height: header.height as u32,
        pixels,
        ppi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_rejected() {
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn non_wsq_rejected() {
        assert!(decode(&[0x00, 0x01, 0x02, 0x03]).is_err());
    }
}