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

//! WSQ marker constants and helpers.

/// WSQ marker definitions (from the WSQ 3.1 specification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsqMarker {
    /// Start Of Image.
    SOI = 0xFFA0,
    /// End Of Image.
    EOI = 0xFFA1,
    /// Start Of Frame.
    SOF = 0xFFA2,
    /// Start Of Block (compressed data block).
    SOB = 0xFFA3,
    /// Define Transform Table.
    DTT = 0xFFA4,
    /// Define Quantization Table.
    DQT = 0xFFA5,
    /// Define Huffman Table.
    DHT = 0xFFA6,
    /// Define Restart Interval (unused in WSQ 3.1 but reserved).
    DRT = 0xFFA7,
    /// Comment.
    COMMENT = 0xFFA8,
}

impl WsqMarker {
    /// Decodes a 16-bit big-endian marker into a typed marker, if recognized.
    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            0xFFA0 => Some(Self::SOI),
            0xFFA1 => Some(Self::EOI),
            0xFFA2 => Some(Self::SOF),
            0xFFA3 => Some(Self::SOB),
            0xFFA4 => Some(Self::DTT),
            0xFFA5 => Some(Self::DQT),
            0xFFA6 => Some(Self::DHT),
            0xFFA7 => Some(Self::DRT),
            0xFFA8 => Some(Self::COMMENT),
            _ => None,
        }
    }
}

/// Parsed WSQ header (Start Of Frame).
#[derive(Debug, Clone)]
pub struct WsqHeader {
    /// Black pixel value in the source image.
    pub black: u8,
    /// White pixel value in the source image.
    pub white: u8,
    /// Image width in pixels.
    pub width: u16,
    /// Image height in pixels.
    pub height: u16,
    /// Pixel-shift parameter used during reconstruction.
    pub m_shift: f32,
    /// Reconstruction scale parameter.
    pub r_scale: f32,
}
