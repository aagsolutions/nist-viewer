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

//! Parsed representations of individual NIST logical records.

/// A single text field inside a record, identified by its `field` number.
#[derive(Debug, Clone)]
pub struct TextField {
    /// The numeric field identifier (the `NNN` in `crt.NNN:value`).
    pub field: u16,
    /// The standard 3-letter code for the field (e.g. `"VER"`, `"DAT"`).
    pub code: String,
    /// The textual value associated with the field.
    pub value: String,
}

impl TextField {
    pub fn new(field: u16, code: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            field,
            code: code.into(),
            value: value.into(),
        }
    }
}

/// A parsed logical record from an ANSI/NIST-ITL file.
///
/// Text-style records (Type-1, Type-2, Type-9, Type-10, Type-13-17) keep their
/// fields as `TextField`s plus an optional binary image blob in `image_data`.
///
/// Binary-style records (Types 3-8) keep only their binary payload and a few
/// well-known header bytes.
#[derive(Debug, Clone)]
pub struct NistRecord {
    /// The record type identifier (e.g. `1` for Type-1).
    pub record_type: u8,
    /// Image Designation Character — `0` if this record is unique in the file.
    pub idc: u8,
    /// The textual fields found in the record. Always includes `LEN` (field 1).
    pub fields: Vec<TextField>,
    /// The raw binary image payload for image records (`None` for pure-text records).
    pub image_data: Option<Vec<u8>>,
}

impl NistRecord {
    /// Returns the value of the first field whose code matches `code`, if any.
    pub fn field_value(&self, code: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.code == code)
            .map(|f| f.value.as_str())
    }

    /// Returns the named compression algorithm (when present, e.g. `WSQ20`, `JPEGB`, `JPEGL`, `PNG`).
    pub fn compression_algorithm(&self) -> Option<&str> {
        self.field_value("CGA")
            .or_else(|| self.field_value("GCA"))
    }
}
