//! read — universal file-read skill core.
//!
//! Pure extraction logic, deliberately kept free of any `host_fs_*` / wasm
//! plumbing so it can be unit-tested natively. The wasm entry points in
//! `wasm_shim.rs` are a thin wrapper around `extract()`.

use serde::Serialize;
use std::io::Read;

#[cfg(target_arch = "wasm32")]
#[cfg(target_arch = "wasm32")]
pub mod wasm_shim;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    PlainText,
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Unsupported,
}

pub fn detect_format(path: &str) -> Format {
    let ext = path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Format::Pdf,
        "docx" => Format::Docx,
        "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" => Format::Xlsx,
        "pptx" => Format::Pptx,
        // Binary formats we deliberately do not attempt (no text-layer to extract
        // without OCR/decoding support this skill doesn't have).
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "zip" | "exe" | "bin" => Format::Unsupported,
        _ => Format::PlainText,
    }
}

#[derive(Serialize, Debug)]
pub struct ReadResult {
    pub path: String,
    pub format: String,
    pub content: String,
    pub total_lines: usize,
    pub truncated: bool,
}

pub struct ReadOptions {
    pub line_range: Option<(usize, usize)>,
    pub max_bytes: usize,
    /// For Xlsx: cap rows per sheet in the rendered text output.
    pub max_rows_per_sheet: usize,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions {
            line_range: None,
            max_bytes: 1024 * 1024,
            max_rows_per_sheet: 500,
        }
    }
}

/// Top-level entry: raw file bytes in, extracted text out, with line-range
/// slicing applied uniformly across all formats (slicing operates on the
/// *extracted* text, not the raw file).
pub fn extract(path: &str, raw: &[u8], opts: &ReadOptions) -> Result<ReadResult, String> {
    let format = detect_format(path);

    let (text, byte_truncated) = match format {
        Format::PlainText => {
            let truncated = raw.len() > opts.max_bytes;
            let capped = if truncated { &raw[..opts.max_bytes] } else { raw };
            (String::from_utf8_lossy(capped).to_string(), truncated)
        }
        Format::Pdf => (extract_pdf(raw)?, false),
        Format::Docx => (extract_docx(raw)?, false),
        Format::Xlsx => (extract_xlsx(raw, opts.max_rows_per_sheet)?, false),
        Format::Pptx => (extract_pptx(raw)?, false),
        Format::Unsupported => {
            return Err(format!(
                "Unsupported format for text extraction: \"{}\". Binary/image formats need OCR or a dedicated skill.",
                path
            ));
        }
    };

    let all_lines: Vec<&str> = text.lines().collect();
    let total_lines = all_lines.len();

    let (content, line_truncated) = match opts.line_range {
        Some((start, end)) => {
            if start == 0 {
                return Err("line_range is 1-indexed; start must be >= 1".to_string());
            }
            let start_idx = start.saturating_sub(1);
            if start_idx >= all_lines.len() {
                return Err(format!(
                    "line_range start {} exceeds extracted length ({} lines)",
                    start, total_lines
                ));
            }
            let end_idx = end.min(all_lines.len());
            (all_lines[start_idx..end_idx].join("\n"), false)
        }
        None => (text, false),
    };

    if content.trim().is_empty() {
        return Err(format!(
            "\"{}\" was read successfully but no extractable text was found (format={:?}). The file may be image-based, empty, or scanned without OCR text.",
            path, format
        ));
    }

    Ok(ReadResult {
        path: path.to_string(),
        format: format!("{:?}", format).to_lowercase(),
        content,
        total_lines,
        truncated: byte_truncated || line_truncated,
    })
}

// ── PDF ───────────────────────────────────────────────────────────────────────

fn extract_pdf(raw: &[u8]) -> Result<String, String> {
    pdf_extract::extract_text_from_mem(raw)
        .map_err(|e| format!("PDF extraction failed: {}", e))
}

// ── DOCX ──────────────────────────────────────────────────────────────────────
// DOCX is a zip archive; the text body lives in word/document.xml as a flat
// run of <w:p>...<w:r><w:t>text</w:t></w:r>...</w:p> elements.

fn extract_docx(raw: &[u8]) -> Result<String, String> {
    let xml = read_zip_entry(raw, "word/document.xml")?;
    extract_text_from_wordprocessing_xml(&xml)
}

fn extract_text_from_wordprocessing_xml(xml: &str) -> Result<String, String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.trim_text(false);

    let mut out = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local = name.as_ref();
                if local.ends_with(b"t") && (local == b"w:t" || local.ends_with(b":t")) {
                    in_text = true;
                } else if local == b"w:p" || local.ends_with(b":p") {
                    // paragraph boundary handled on End(p) to avoid a leading blank line
                } else if local == b"w:br" || local == b"w:tab" || local.ends_with(b":br") {
                    out.push(' ');
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local = name.as_ref();
                if local == b"w:t" || local.ends_with(b":t") {
                    in_text = false;
                } else if local == b"w:p" || local.ends_with(b":p") {
                    out.push('\n');
                }
            }
            Ok(Event::Text(t)) if in_text => {
                let decoded = t
                    .unescape()
                    .map_err(|e| format!("XML text decode error: {}", e))?;
                out.push_str(&decoded);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("DOCX XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

// ── PPTX ──────────────────────────────────────────────────────────────────────
// PPTX is a zip archive with one XML per slide at ppt/slides/slideN.xml,
// text runs live in <a:t> elements. We walk slides in numeric order.

fn extract_pptx(raw: &[u8]) -> Result<String, String> {
    let cursor = std::io::Cursor::new(raw);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("Not a valid PPTX/zip file: {}", e))?;

    let mut slide_indices: Vec<(usize, String)> = Vec::new();
    for i in 0..archive.len() {
        let name = archive
            .by_index(i)
            .map_err(|e| format!("Zip read error: {}", e))?
            .name()
            .to_string();
        if let Some(rest) = name
            .strip_prefix("ppt/slides/slide")
            .and_then(|s| s.strip_suffix(".xml"))
        {
            if let Ok(n) = rest.parse::<usize>() {
                slide_indices.push((n, name));
            }
        }
    }
    slide_indices.sort_by_key(|(n, _)| *n);

    if slide_indices.is_empty() {
        return Err("No slides found in PPTX (unexpected archive layout)".to_string());
    }

    let mut out = String::new();
    for (n, name) in slide_indices {
        let mut file = archive
            .by_name(&name)
            .map_err(|e| format!("Missing slide entry {}: {}", name, e))?;
        let mut xml = String::new();
        file.read_to_string(&mut xml)
            .map_err(|e| format!("Failed reading {}: {}", name, e))?;
        drop(file);

        let text = extract_text_from_drawingml_xml(&xml)?;
        out.push_str(&format!("--- Slide {} ---\n{}\n\n", n, text.trim()));
    }

    Ok(out)
}

fn extract_text_from_drawingml_xml(xml: &str) -> Result<String, String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.trim_text(false);

    let mut out = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = e.name();
                let local = local.as_ref();
                if local == b"a:t" {
                    in_text = true;
                } else if local == b"a:p" {
                    // paragraph handled on close
                }
            }
            Ok(Event::End(e)) => {
                let local = e.name();
                let local = local.as_ref();
                if local == b"a:t" {
                    in_text = false;
                } else if local == b"a:p" {
                    out.push('\n');
                }
            }
            Ok(Event::Text(t)) if in_text => {
                let decoded = t
                    .unescape()
                    .map_err(|e| format!("XML text decode error: {}", e))?;
                out.push_str(&decoded);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("PPTX slide XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

// ── XLSX / XLS / ODS ─────────────────────────────────────────────────────────
// calamine reads workbook + all sheets; we render each sheet as a simple
// tab-separated preview capped at max_rows_per_sheet rows.

fn extract_xlsx(raw: &[u8], max_rows_per_sheet: usize) -> Result<String, String> {
    use calamine::{open_workbook_from_rs, Reader, Xlsx};

    let cursor = std::io::Cursor::new(raw);
    let mut workbook: Xlsx<_> = open_workbook_from_rs(cursor)
        .map_err(|e| format!("Failed to open spreadsheet: {}", e))?;

    let sheet_names = workbook.sheet_names().to_owned();
    if sheet_names.is_empty() {
        return Err("Spreadsheet has no sheets".to_string());
    }

    let mut out = String::new();
    for name in sheet_names {
        let range = match workbook.worksheet_range(&name) {
            Ok(r) => r,
            Err(e) => {
                out.push_str(&format!("--- Sheet: {} (unreadable: {}) ---\n\n", name, e));
                continue;
            }
        };

        out.push_str(&format!("--- Sheet: {} ---\n", name));
        let mut row_count = 0usize;
        let total_rows = range.rows().count();
        for row in range.rows() {
            if row_count >= max_rows_per_sheet {
                out.push_str(&format!(
                    "... ({} more rows truncated)\n",
                    total_rows - row_count
                ));
                break;
            }
            let cells: Vec<String> = row.iter().map(|c| c.to_string()).collect();
            out.push_str(&cells.join("\t"));
            out.push('\n');
            row_count += 1;
        }
        out.push('\n');
    }

    Ok(out)
}

// ── Zip helper ────────────────────────────────────────────────────────────────

fn read_zip_entry(raw: &[u8], entry_name: &str) -> Result<String, String> {
    let cursor = std::io::Cursor::new(raw);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("Not a valid zip-based file: {}", e))?;
    let mut file = archive
        .by_name(entry_name)
        .map_err(|e| format!("Missing expected entry \"{}\": {}", entry_name, e))?;
    let mut s = String::new();
    file.read_to_string(&mut s)
        .map_err(|e| format!("Failed reading \"{}\": {}", entry_name, e))?;
    Ok(s)
}

// ── Tests ─────────────────────────────────────────────────────────────────────
// Run with `cargo test` on a native target (these never touch host_fs_read /
// wasm_shim, which is wasm32-gated). Fixtures are the same files this was
// validated against during development.
