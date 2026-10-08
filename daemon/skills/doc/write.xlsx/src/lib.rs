//! write.xlsx — spreadsheet generation skill.
//! Builds an XLSX workbook guest-side with rust_xlsxwriter (pure Rust, no C
//! dependencies) and persists the raw bytes via the binary-safe
//! host_fs_write_bytes host function.

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

const MAX_ROWS_PER_SHEET: usize = 500;
const MAX_COLS_PER_ROW: usize = 50;

#[derive(Deserialize)]
struct SheetInput {
    /// Worksheet name (sanitized to Excel rules: 31 chars, no []:*?/\\).
    #[serde(default)]
    name: String,
    /// Grid of cell values; numbers and booleans keep their types.
    #[serde(default)]
    rows: Vec<Vec<Value>>,
}

#[derive(Deserialize)]
struct Input {
    /// Destination path for the workbook (relative to fs_root or absolute).
    path: String,
    /// Worksheets to create; overrides headers/rows when non-empty.
    #[serde(default)]
    sheets: Vec<SheetInput>,
    /// Header row for single-sheet mode (first row when sheets is empty).
    #[serde(default)]
    headers: Vec<Value>,
    /// Data rows for single-sheet mode.
    #[serde(default)]
    rows: Vec<Vec<Value>>,
}

// ── Logic ─────────────────────────────────────────────────────────────────────

fn execute(input: &str) -> Result<Value, String> {
    let args: Input = serde_json::from_str(input).map_err(|e| format!("Invalid input: {}", e))?;

    if args.path.trim().is_empty() {
        return Err("Missing required argument: path must be a non-empty string".to_string());
    }

    let mut grids: Vec<(String, Vec<Vec<Value>>)> = Vec::new();
    if args.sheets.is_empty() {
        let mut rows: Vec<Vec<Value>> = Vec::new();
        if !args.headers.is_empty() {
            rows.push(args.headers.clone());
        }
        rows.extend(args.rows.clone());
        grids.push(("Sheet1".to_string(), rows));
    } else {
        for (i, sheet) in args.sheets.iter().enumerate() {
            let name =
                if sheet.name.trim().is_empty() { format!("Sheet{}", i + 1) } else { sheet.name.clone() };
            grids.push((name, sheet.rows.clone()));
        }
    }

    if grids.iter().all(|(_, rows)| rows.is_empty()) {
        return Err("No data: provide sheets with rows, or headers/rows for a single sheet"
            .to_string());
    }

    let mut workbook = rust_xlsxwriter::Workbook::new();
    let mut truncated = false;
    let mut total_rows: usize = 0;

    for (i, (name, rows)) in grids.iter().enumerate() {
        let sheet_name = unique_sheet_name(name, i);
        let worksheet = workbook.add_worksheet();
        worksheet
            .set_name(&sheet_name)
            .map_err(|e| format!("Invalid sheet name '{}': {}", name, e))?;

        let mut row_idx: u32 = 0;
        for row in rows.iter().take(MAX_ROWS_PER_SHEET) {
            if row_idx as usize >= MAX_ROWS_PER_SHEET {
                break;
            }
            for (col_idx, cell) in row.iter().take(MAX_COLS_PER_ROW).enumerate() {
                write_cell(worksheet, row_idx, col_idx as u16, cell)
                    .map_err(|e| format!("Failed to write cell (row {}, col {}) in '{}': {}", row_idx + 1, col_idx + 1, sheet_name, e))?;
            }
            if row.len() > MAX_COLS_PER_ROW {
                truncated = true;
            }
            row_idx += 1;
        }
        if rows.len() > MAX_ROWS_PER_SHEET {
            truncated = true;
        }
        total_rows += row_idx as usize;
    }

    let bytes =
        workbook.save_to_buffer().map_err(|e| format!("Failed to build XLSX: {}", e))?;
    let bytes_written = bytes.len();
    let sheet_count = grids.len();

    fs_write_bytes(&args.path, &bytes)?;

    Ok(json!({
        "path": args.path,
        "bytes_written": bytes_written,
        "sheets": sheet_count,
        "rows": total_rows,
        "truncated": truncated,
    }))
}

fn write_cell(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    cell: &Value,
) -> Result<(), rust_xlsxwriter::XlsxError> {
    let result = match cell {
        Value::String(s) => worksheet.write_string(row, col, s.as_str()),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                worksheet.write_number(row, col, f)
            } else {
                worksheet.write_string(row, col, n.to_string())
            }
        }
        Value::Bool(b) => worksheet.write_boolean(row, col, *b),
        Value::Null => worksheet.write_string(row, col, ""),
        other => worksheet.write_string(row, col, other.to_string()),
    };
    result.map(|_| ())
}

fn unique_sheet_name(raw: &str, index: usize) -> String {
    let mut name: String =
        raw.chars().map(|c| if "[]:*?/\\".contains(c) { '_' } else { c }).collect();
    name = name.trim().to_string();
    if name.is_empty() {
        name = format!("Sheet{}", index + 1);
    }
    let mut chars: Vec<char> = name.chars().collect();
    if chars.len() > 31 {
        chars.truncate(31);
        name = chars.into_iter().collect();
    }
    if name.eq_ignore_ascii_case("history") {
        name.push('_');
    }
    name
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_wasm_ptr(s: String) -> *mut u8 {
    let mut bytes = s.into_bytes();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr
}
