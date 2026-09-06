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

//! Format constants and helper utilities for the NIST biometric file format.

/// ASCII Record Separator — separates logical records.
pub const RECORD_SEPARATOR: u8 = 0x1E;

/// ASCII Unit Separator — separates fields within a record.
pub const UNIT_SEPARATOR: u8 = 0x1F;

/// ASCII Group Separator — used inside records to separate groups of fields.
pub const GROUP_SEPARATOR: u8 = 0x1D;

/// ASCII File/Field Separator — terminates a logical record.
pub const FIELD_SEPARATOR: u8 = 0x1C;

/// Returns `true` if the byte is one of the four NIST separator characters.
#[inline]
pub fn is_separator(b: u8) -> bool {
    matches!(b, RECORD_SEPARATOR | UNIT_SEPARATOR | GROUP_SEPARATOR | FIELD_SEPARATOR)
}

/// A logical record type identifier (the `crt` in `crt.field:value`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordType(pub u8);

impl RecordType {
    /// Returns the human-readable description of the record type, when known.
    pub fn name(self) -> &'static str {
        match self.0 {
            1 => "Type-1: Transaction Information",
            2 => "Type-2: User-defined Descriptive Text",
            3 => "Type-3: Low-resolution Grayscale Fingerprint",
            4 => "Type-4: High-resolution Grayscale Fingerprint",
            5 => "Type-5: Low-resolution Binary Fingerprint",
            6 => "Type-6: High-resolution Binary Fingerprint",
            7 => "Type-7: User-defined Image",
            8 => "Type-8: Signature Image",
            9 => "Type-9: Minutiae Data",
            10 => "Type-10: Facial & SMT Image",
            11 => "Type-11: Forensic & Investigatory Voice",
            12 => "Type-12: Forensic Dental & Oral Care Data",
            13 => "Type-13: Variable-resolution Latent Image",
            14 => "Type-14: Variable-resolution Fingerprint",
            15 => "Type-15: Palm Print Image",
            16 => "Type-16: User-defined Test Image",
            17 => "Type-17: Iris Image",
            99 => "Type-99: CBEFF (Biometric Data Block)",
            _ => "Unknown",
        }
    }
}

impl std::fmt::Display for RecordType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Type-{}", self.0)
    }
}
