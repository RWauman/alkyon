//! Reading `.xlsx` in-process with calamine, rather than through DuckDB's `excel`
//! extension — a spreadsheet should never require a network download.

use calamine::{Data, Reader};

use crate::error::{Error, Result};
use crate::model::{ColumnMeta, LogicalType};

/// A sheet, flattened the way the rest of federation expects: column metadata
/// plus every cell already rendered as text.
pub struct Sheet {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// Every sheet in a workbook, in the order the file lists them.
///
/// A folder source needs this to turn one workbook into one table per sheet, which
/// it cannot do without knowing what the sheets are called.
pub fn sheet_names(path: &std::path::Path) -> Result<Vec<String>> {
    let workbook = calamine::open_workbook_auto(path)
        .map_err(|e| Error::BadRequest(format!("cannot read {}: {e}", path.display())))?;
    Ok(workbook.sheet_names().to_vec())
}

/// The first row is the header. `sheet` picks one by name; without it, the first.
pub fn read(path: &std::path::Path, sheet: Option<&str>) -> Result<Sheet> {
    let mut workbook = calamine::open_workbook_auto(path)
        .map_err(|e| Error::BadRequest(format!("cannot read {}: {e}", path.display())))?;

    let name = match sheet {
        Some(wanted) => workbook
            .sheet_names()
            .iter()
            .find(|name| name.eq_ignore_ascii_case(wanted))
            .cloned()
            .ok_or_else(|| {
                Error::BadRequest(format!(
                    "no sheet `{wanted}` in {} — found {:?}",
                    path.display(),
                    workbook.sheet_names()
                ))
            })?,
        None => workbook
            .sheet_names()
            .first()
            .cloned()
            .ok_or_else(|| Error::BadRequest(format!("{} has no sheets", path.display())))?,
    };

    let range = workbook
        .worksheet_range(&name)
        .map_err(|e| Error::BadRequest(format!("cannot read sheet `{name}`: {e}")))?;

    let mut rows = range.rows();
    let Some(header) = rows.next() else {
        return Err(Error::BadRequest(format!("sheet `{name}` is empty")));
    };

    let names: Vec<String> = header
        .iter()
        .enumerate()
        .map(|(index, cell)| match text(cell) {
            Some(label) if !label.trim().is_empty() => label.trim().to_owned(),
            // An unnamed column still needs a handle.
            _ => format!("column{}", index + 1),
        })
        .collect();

    let body: Vec<Vec<Data>> = rows.map(<[Data]>::to_vec).collect();

    // A spreadsheet has no declared types, so infer one per column from what is
    // actually in it. Without this every column would arrive as VARCHAR and a
    // join on a numeric key would not work.
    let columns = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let logical = infer(body.iter().filter_map(|row| row.get(index)));
            ColumnMeta {
                name: name.clone(),
                type_name: format!("xlsx {logical:?}").to_lowercase(),
                logical,
            }
        })
        .collect();

    let rows = body
        .iter()
        .map(|row| {
            (0..names.len())
                .map(|index| row.get(index).and_then(text))
                .collect()
        })
        .collect();

    Ok(Sheet { columns, rows })
}

/// The narrowest type every non-empty cell in the column fits.
fn infer<'a>(cells: impl Iterator<Item = &'a Data>) -> LogicalType {
    let mut seen = None;
    for cell in cells {
        let kind = match cell {
            Data::Empty => continue,
            Data::Bool(_) => LogicalType::Bool,
            Data::Int(_) => LogicalType::Int,
            Data::Float(f) if f.fract() == 0.0 => LogicalType::Int,
            Data::Float(_) => LogicalType::Float,
            Data::DateTime(_) => LogicalType::Timestamp,
            Data::DateTimeIso(_) => LogicalType::Timestamp,
            Data::DurationIso(_) => LogicalType::Text,
            Data::String(_) | Data::Error(_) => LogicalType::Text,
        };
        seen = Some(match seen {
            None => kind,
            Some(previous) => widen(previous, kind),
        });
    }
    seen.unwrap_or(LogicalType::Text)
}

/// The type that holds both. Anything that disagrees ends up as text.
fn widen(a: LogicalType, b: LogicalType) -> LogicalType {
    use LogicalType::{Float, Int, Text};
    match (a, b) {
        (x, y) if x == y => x,
        (Int, Float) | (Float, Int) => Float,
        _ => Text,
    }
}

fn text(cell: &Data) -> Option<String> {
    match cell {
        Data::Empty => None,
        Data::String(s) => Some(s.clone()),
        Data::Float(f) => Some(if f.fract() == 0.0 {
            format!("{}", *f as i64)
        } else {
            f.to_string()
        }),
        Data::Int(i) => Some(i.to_string()),
        Data::Bool(b) => Some(b.to_string()),
        // Excel serial dates: calamine converts these for us.
        Data::DateTime(d) => d
            .as_datetime()
            .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string()),
        Data::DateTimeIso(s) | Data::DurationIso(s) => Some(s.clone()),
        Data::Error(e) => Some(format!("#{e:?}")),
    }
}
