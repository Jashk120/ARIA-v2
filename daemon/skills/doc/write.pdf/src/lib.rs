//! write.pdf — PDF document generation skill.
//! Builds a valid %PDF-1.4 file guest-side with a hand-rolled writer
//! (Base14 Helvetica, no embedded fonts, zero extra dependencies so it
//! always cross-compiles to wasm32-wasip1) and persists the raw bytes via
//! the binary-safe host_fs_write_bytes host function.

use serde::Deserialize;
use serde_json::{
    Value,
    json,
};

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
    /// Destination path for the PDF (relative to fs_root or absolute).
    path: String,
    /// Title rendered large at the top of the first page.
    #[serde(default)]
    title: String,
    /// Body text lines rendered as wrapped paragraphs.
    #[serde(default)]
    lines: Vec<Value>,
    /// Table rows rendered as "cell | cell" lines.
    #[serde(default)]
    table: Vec<Vec<Value>>,
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    if args.path.trim().is_empty() {
        return Err("Missing required argument: path must be a non-empty string".to_string());
    }

    let mut logical: Vec<String> = Vec::new();
    for row in &args.table {
        let cells: Vec<String> = row.iter().map(cell_to_string).collect();
        logical.push(cells.join(" | "));
    }
    for line in &args.lines {
        logical.push(cell_to_string(line));
    }

    let pdf = build_pdf(&args.title, &logical);
    let pages = count_pages(&pdf);
    let bytes_written = pdf.len();

    fs_write_bytes(&args.path, &pdf)?;

    Ok(json!({
        "path": args.path,
        "bytes_written": bytes_written,
        "pages": pages,
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

// ── Minimal PDF writer ────────────────────────────────────────────────────────
//
// A4 (595x842pt), 72pt margins, Helvetica body 11pt / Helvetica-Bold title
// 18pt. Text is WinAnsi-ish: codepoints above U+00FF become '?' so the byte
// stream stays valid for Base14 fonts without embedding anything.

const PAGE_W: f32 = 595.0;
const PAGE_H: f32 = 842.0;
const MARGIN: f32 = 72.0;
const BODY_SIZE: f32 = 11.0;
const TITLE_SIZE: f32 = 18.0;
const LEADING: f32 = 14.0;

fn count_pages(pdf: &[u8]) -> usize {
    let needle = b"/Type /Page ";
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= pdf.len() {
        if &pdf[i..i + needle.len()] == needle {
            count += 1;
        }
        i += 1;
    }
    count.max(1)
}

fn push_long_word(out: &mut Vec<String>, word: &str, max_chars: usize) {
    let chars: Vec<char> = word.chars().collect();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + max_chars).min(chars.len());
        out.push(chars[start..end].iter().collect());
        start = end;
    }
}

fn wrap_line(line: &str, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len: usize = 0;
    for word in line.split_whitespace() {
        let word_len = word.chars().count();
        if word_len > max_chars {
            if current_len > 0 {
                out.push(std::mem::take(&mut current));
                current_len = 0;
            }
            push_long_word(&mut out, word, max_chars);
        } else if current_len == 0 {
            current = word.to_string();
            current_len = word_len;
        } else if current_len + 1 + word_len <= max_chars {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            out.push(std::mem::take(&mut current));
            current = word.to_string();
            current_len = word_len;
        }
    }
    if current_len > 0 {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn sanitize_for_pdf(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let code = c as u32;
        if code == 0x09 || code == 0x0A || code == 0x0D || (0x20..=0xFF).contains(&code) {
            out.push(code as u8);
        } else {
            out.push(b'?');
        }
    }
    out
}

fn escape_pdf_bytes(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for &b in raw {
        match b {
            b'\\' | b'(' | b')' => {
                out.push(b'\\');
                out.push(b);
            }
            _ => out.push(b),
        }
    }
    out
}

fn text_line_op(font: &str, size: f32, x: f32, y: f32, text: &[u8]) -> Vec<u8> {
    let mut op = Vec::new();
    op.extend_from_slice(b"BT ");
    op.extend_from_slice(font.as_bytes());
    op.extend_from_slice(format!(" {} Tf {} {} Td (", size, x, y).as_bytes());
    op.extend_from_slice(&escape_pdf_bytes(text));
    op.extend_from_slice(b") Tj ET\n");
    op
}

fn build_pdf(title: &str, logical_lines: &[String]) -> Vec<u8> {
    let usable_w = PAGE_W - 2.0 * MARGIN;
    let body_chars = (usable_w / (BODY_SIZE * 0.55)) as usize;
    let title_chars = (usable_w / (TITLE_SIZE * 0.60)) as usize;

    let mut wrapped: Vec<(bool, String)> = Vec::new();
    if !title.trim().is_empty() {
        for w in wrap_line(title.trim(), title_chars.max(10)) {
            wrapped.push((true, w));
        }
    }
    for line in logical_lines {
        for w in wrap_line(line, body_chars.max(10)) {
            wrapped.push((false, w));
        }
    }
    if wrapped.is_empty() {
        wrapped.push((false, String::new()));
    }

    let first_y = PAGE_H - MARGIN;
    let last_y = MARGIN;
    let mut pages_text: Vec<Vec<(bool, String)>> = Vec::new();
    let mut current: Vec<(bool, String)> = Vec::new();
    let mut y = first_y;
    for (is_title, line) in wrapped {
        let size = if is_title { TITLE_SIZE + 6.0 } else { LEADING };
        if y - size < last_y && !current.is_empty() {
            pages_text.push(std::mem::take(&mut current));
            y = first_y;
        }
        current.push((is_title, line));
        y -= size;
    }
    if !current.is_empty() {
        pages_text.push(current);
    }

    let n = pages_text.len();
    // Object layout: 1 catalog, 2 pages, then per page (page, content),
    // then Helvetica + Helvetica-Bold font objects.
    let content_start = 3 + 2 * n;
    let font_regular = content_start as i32;
    let font_bold = (content_start + 1) as i32;

    let mut objects: Vec<Vec<u8>> = Vec::new();

    let mut kids = String::new();
    for i in 0..n {
        let page_obj = (3 + 2 * i) as i32;
        kids.push_str(&format!("{} 0 R ", page_obj));
    }
    objects.push(format!("1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n").into_bytes());
    objects.push(
        format!(
            "2 0 obj\n<< /Type /Pages /Kids [{}] /Count {} >>\nendobj\n",
            kids.trim_end(),
            n
        )
        .into_bytes(),
    );

    for (i, lines) in pages_text.iter().enumerate() {
        let page_obj = (3 + 2 * i) as i32;
        let content_obj = (3 + 2 * i + 1) as i32;
        objects.push(
            format!(
                "{} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 {} 0 R /F2 {} 0 R >> >> /Contents {} 0 R >>\nendobj\n",
                page_obj, PAGE_W as i32, PAGE_H as i32, font_regular, font_bold, content_obj
            )
            .into_bytes(),
        );

        let mut stream = Vec::new();
        let mut y = first_y;
        for (is_title, line) in lines {
            let (font, size, step) = if *is_title {
                ("/F2", TITLE_SIZE, TITLE_SIZE + 6.0)
            } else {
                ("/F1", BODY_SIZE, LEADING)
            };
            y -= step;
            let bytes = sanitize_for_pdf(line);
            stream.extend_from_slice(&text_line_op(font, size, MARGIN, y, &bytes));
        }
        let mut obj = format!("{} 0 obj\n<< /Length {} >>\nstream\n", content_obj, stream.len())
            .into_bytes();
        obj.extend_from_slice(&stream);
        obj.extend_from_slice(b"endstream\nendobj\n");
        objects.push(obj);
    }

    objects.push(
        format!(
            "{} 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
            font_regular
        )
        .into_bytes(),
    );
    objects.push(
        format!(
            "{} 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>\nendobj\n",
            font_bold
        )
        .into_bytes(),
    );

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets: Vec<usize> = Vec::new();
    for obj in &objects {
        offsets.push(pdf.len());
        pdf.extend_from_slice(obj);
    }
    let xref_pos = pdf.len();
    let total = (objects.len() + 1) as i32;
    pdf.extend_from_slice(format!("xref\n0 {}\n", total).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for off in offsets {
        pdf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF", total, xref_pos)
            .as_bytes(),
    );
    pdf
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
