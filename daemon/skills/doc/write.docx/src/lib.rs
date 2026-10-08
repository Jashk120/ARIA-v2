//! write.docx — Word document generation skill.
//!
//! Builds a valid .docx (Office Open XML) guest-side with a hand-rolled
//! writer: raw XML parts packed into a ZIP archive (pure Rust via the `zip`
//! crate's deflate backend — same zip approach read.fs uses for parsing, in
//! reverse, so it always cross-compiles to wasm32-wasip1 with no C-linked
//! dependencies). The finished bytes are persisted via the binary-safe
//! host_fs_write_bytes host function.
//!
//! Generation is kept structured on purpose: [`build_paragraph_xml`] and
//! [`build_table_xml`] emit independent `word/document.xml` fragments, so a
//! future classification-marking step (headers/footers) can hook in by adding
//! new part builders + relationship entries without touching the body logic.

use serde::Deserialize;
use serde_json::{
    Value,
    json,
};
use std::io::Write;

// ── Host functions ────────────────────────────────────────────────────────────

#[link(wasm_import_module = "aria")]
unsafe extern "C" {
    /// host_fs_write_bytes(path_ptr, path_len, data_ptr, data_len) -> 1 ok / 0 err
    fn host_fs_write_bytes(
        path_ptr: *const u8,
        path_len: usize,
        data_ptr: *const u8,
        data_len: usize,
    ) -> i32;
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn alloc(size: usize) -> *mut u8 {
    let mut buf = Vec::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn run(input_ptr: *const u8, input_len: usize) -> u64 {
    let input = unsafe {
        let slice = std::slice::from_raw_parts(input_ptr, input_len);
        std::str::from_utf8(slice).unwrap_or("")
    };

    let output = match execute(input) {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    };

    let len = output.len();
    let ptr = to_wasm_ptr(output);
    ((ptr as u64) << 32) | (len as u64)
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Input {
    /// Destination path for the .docx (relative to fs_root or absolute).
    path: String,
    /// Title rendered as a Heading1 paragraph at the top of the document.
    #[serde(default)]
    title: String,
    /// Body paragraphs (plain strings; numbers/bools are stringified).
    #[serde(default)]
    paragraphs: Vec<Value>,
    /// One table: rows of cell values (numbers/bools are stringified).
    #[serde(default)]
    table: Vec<Vec<Value>>,
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    if args.path.trim().is_empty() {
        return Err("Missing required argument: path must be a non-empty string".to_string());
    }

    let paragraphs: Vec<String> = args.paragraphs.iter().map(cell_to_string).collect();
    let table: Vec<Vec<String>> =
        args.table.iter().map(|row| row.iter().map(cell_to_string).collect()).collect();
    let table_rows = table.len();

    let docx = build_docx(&args.title, &paragraphs, &table)?;
    let bytes_written = docx.len();

    fs_write_bytes(&args.path, &docx)?;

    Ok(json!({
        "path": args.path,
        "bytes_written": bytes_written,
        "paragraphs": paragraphs.len(),
        "table_rows": table_rows,
    }))
}

fn cell_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ── Host call wrapper ─────────────────────────────────────────────────────────

fn fs_write_bytes(path: &str, data: &[u8]) -> Result<(), String> {
    let status = unsafe {
        let p = path.as_bytes();
        host_fs_write_bytes(p.as_ptr(), p.len(), data.as_ptr(), data.len())
    };

    if status == 0 {
        return Err(format!(
            "host_fs_write_bytes returned error — write to \"{}\" failed (access denied or path invalid)",
            path
        ));
    }

    Ok(())
}

// ── Minimal .docx writer ──────────────────────────────────────────────────────
//
// A .docx is a ZIP of XML parts. The minimal set Word/LibreOffice accept:
//   [Content_Types].xml — part content-type map
//   _rels/.rels         — package relationships (document + properties)
//   word/document.xml   — the body (title + paragraphs + table)
//   word/styles.xml     — Normal + Heading1 + TableGrid style definitions
//   docProps/core.xml   — core properties (title)
//   docProps/app.xml    — extended properties
//
// FUTURE HOOK (classification marking): headers/footers become extra parts
// (word/header1.xml, word/footer1.xml) + <w:headerReference>/<w:footerReference>
// entries inside the <w:sectPr> in build_document_xml, plus matching
// Relationship entries and [Content_Types] overrides. The body builders below
// stay untouched.

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\n' && c != '\r' && c != '\t' => {
                out.push('\u{FFFD}');
            }
            c => out.push(c),
        }
    }
    out
}

/// One body paragraph. `style` is None for Normal, Some("Heading1") for titles.
fn build_paragraph_xml(style: Option<&str>, text: &str) -> String {
    let ppr = match style {
        Some(name) => {
            format!("<w:pPr><w:pStyle w:val=\"{}\"/></w:pPr>", name)
        }
        None => String::new(),
    };
    format!(
        "<w:p>{}<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
        ppr,
        escape_xml(text)
    )
}

/// One `w:tbl` element. Column count is the widest row; short rows are padded
/// with empty cells so the grid stays rectangular.
fn build_table_xml(table: &[Vec<String>]) -> String {
    let cols = table.iter().map(|r| r.len()).max().unwrap_or(0).max(1);
    let mut out = String::from(
        "<w:tbl><w:tblPr><w:tblStyle w:val=\"TableGrid\"/><w:tblW w:w=\"0\" w:type=\"auto\"/></w:tblPr><w:tblGrid>",
    );
    for _ in 0..cols {
        out.push_str("<w:gridCol w:w=\"2880\"/>");
    }
    out.push_str("</w:tblGrid>");
    for row in table {
        out.push_str("<w:tr>");
        for i in 0..cols {
            let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
            out.push_str("<w:tc><w:tcPr><w:tcW w:w=\"2880\" w:type=\"dxa\"/></w:tcPr>");
            out.push_str(&build_paragraph_xml(None, cell));
            out.push_str("</w:tc>");
        }
        out.push_str("</w:tr>");
    }
    out.push_str("</w:tbl>");
    out
}

fn build_document_xml(title: &str, paragraphs: &[String], table: &[Vec<String>]) -> String {
    let mut body = String::new();
    if !title.trim().is_empty() {
        body.push_str(&build_paragraph_xml(Some("Heading1"), title.trim()));
    }
    for p in paragraphs {
        body.push_str(&build_paragraph_xml(None, p));
    }
    if !table.is_empty() {
        body.push_str(&build_table_xml(table));
    }
    if body.is_empty() {
        // Keep an empty body element so the document still opens.
        body.push_str("<w:p/>");
    }
    // sectPr carries page geometry; header/footer references hook in here later.
    body.push_str("<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\"/></w:sectPr>");

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:body>{}</w:body></w:document>",
        body
    )
}

fn content_types_xml() -> &'static str {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>\
</Types>"
}

fn rels_xml() -> &'static str {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/>\
</Relationships>"
}

fn styles_xml() -> &'static str {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\">\
<w:name w:val=\"Normal\"/><w:rPr><w:sz w:val=\"22\"/><w:szCs w:val=\"22\"/></w:rPr>\
</w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\">\
<w:name w:val=\"heading 1\"/><w:basedOn w:val=\"Normal\"/>\
<w:rPr><w:b/><w:sz w:val=\"36\"/><w:szCs w:val=\"36\"/></w:rPr>\
</w:style>\
<w:style w:type=\"table\" w:styleId=\"TableGrid\">\
<w:name w:val=\"Table Grid\"/><w:tblPr>\
<w:tblBorders>\
<w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
<w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/>\
</w:tblBorders></w:tblPr>\
</w:style>\
</w:styles>"
}

fn core_xml(title: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
<dc:title>{}</dc:title></cp:coreProperties>",
        escape_xml(title.trim())
    )
}

fn app_xml() -> &'static str {
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\">\
<Application>aria write.docx</Application></Properties>"
}

fn build_docx(title: &str, paragraphs: &[String], table: &[Vec<String>]) -> Result<Vec<u8>, String> {
    let document = build_document_xml(title, paragraphs, table);
    let core = core_xml(title);

    let parts: [(&str, Vec<u8>); 6] = [
        ("[Content_Types].xml", content_types_xml().as_bytes().to_vec()),
        ("_rels/.rels", rels_xml().as_bytes().to_vec()),
        ("word/document.xml", document.into_bytes()),
        ("word/styles.xml", styles_xml().as_bytes().to_vec()),
        ("docProps/core.xml", core.into_bytes()),
        ("docProps/app.xml", app_xml().as_bytes().to_vec()),
    ];

    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buf);
        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in &parts {
            writer.start_file(*name, options).map_err(|e| format!("Failed to add zip entry {}: {}", name, e))?;
            writer.write_all(data).map_err(|e| format!("Failed to write zip entry {}: {}", name, e))?;
        }
        writer.finish().map_err(|e| format!("Failed to finish .docx archive: {}", e))?;
    }
    Ok(buf.into_inner())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_part(docx: &[u8], name: &str) -> String {
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(docx)).expect("valid zip archive");
        let mut file = archive.by_name(name).expect("part exists");
        let mut s = String::new();
        file.read_to_string(&mut s).expect("part is utf-8 xml");
        s
    }

    #[test]
    fn builds_docx_with_title_paragraphs_and_table() {
        let docx = build_docx(
            "Approval Note",
            &["Finding 1 resolved.".to_string(), "Approved.".to_string()],
            &[vec!["ID".to_string(), "Status".to_string()], vec!["F-1".to_string(), "Pass".to_string()]],
        )
        .expect("build succeeds");

        let names: Vec<String> = {
            let mut archive =
                zip::ZipArchive::new(std::io::Cursor::new(&docx[..])).expect("valid zip");
            (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_string()).collect()
        };
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/document.xml",
            "word/styles.xml",
            "docProps/core.xml",
            "docProps/app.xml",
        ] {
            assert!(names.iter().any(|n| n == required), "missing part {}", required);
        }

        let document = read_part(&docx, "word/document.xml");
        assert!(document.contains("Approval Note"));
        assert!(document.contains("w:val=\"Heading1\""));
        assert!(document.contains("Finding 1 resolved."));
        assert!(document.contains("<w:tbl>"));
        assert!(document.contains("F-1"));

        std::fs::write("/tmp/write_docx_sample.docx", &docx).expect("write sample");
    }

    #[test]
    fn empty_input_still_produces_openable_document() {
        let docx = build_docx("", &[], &[]).expect("build succeeds");
        let document = read_part(&docx, "word/document.xml");
        assert!(document.contains("<w:body>"));
    }

    #[test]
    fn xml_escaping_keeps_document_well_formed() {
        let docx = build_docx("A&B <tag>", &["x > y & \"q\"".to_string()], &[])
            .expect("build succeeds");
        let document = read_part(&docx, "word/document.xml");
        assert!(document.contains("A&amp;B &lt;tag&gt;"));
    }
}
