//! Parquet writers (and readers, for tests / recovery checks) for the three
//! per-segment tables. Column names and Arrow types are the on-disk contract
//! with the Python pipeline (`pipeline/SCHEMA.md`):
//!
//! | file            | column     | Arrow type | nullable |
//! |-----------------|------------|------------|----------|
//! | frames.parquet  | frame_idx  | UInt32     | no       |
//! |                 | tick_ns    | Int64      | no       |
//! |                 | capture_ns | Int64      | no       |
//! |                 | repeated   | Boolean    | no       |
//! | inputs.parquet  | t_ns       | Int64      | no       |
//! |                 | device     | Utf8       | no       |
//! |                 | kind       | Utf8       | no       |
//! |                 | code       | UInt32     | no       |
//! |                 | value      | Float32    | no       |
//! | focus.parquet   | t_ns       | Int64      | no       |
//! |                 | focused    | Boolean    | no       |
//! |                 | game_id    | Utf8       | no       |
//!
//! Files are written as a single row group, snappy-compressed, rows sorted by
//! time (frames by `frame_idx`).

use arrow::array::{Array, ArrayRef, BooleanArray, Float32Array, Int64Array, RecordBatch, StringArray, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use cap_types::{Device, EventKind, FocusRecord, FrameRecord, InputEvent};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Arc;

pub type TableResult<T> = std::result::Result<T, String>;

pub fn frames_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("frame_idx", DataType::UInt32, false),
        Field::new("tick_ns", DataType::Int64, false),
        Field::new("capture_ns", DataType::Int64, false),
        Field::new("repeated", DataType::Boolean, false),
    ]))
}

pub fn inputs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("t_ns", DataType::Int64, false),
        Field::new("device", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("code", DataType::UInt32, false),
        Field::new("value", DataType::Float32, false),
    ]))
}

pub fn focus_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("t_ns", DataType::Int64, false),
        Field::new("focused", DataType::Boolean, false),
        Field::new("game_id", DataType::Utf8, false),
    ]))
}

fn write_batch(path: &Path, batch: RecordBatch) -> TableResult<()> {
    let e = |x: &dyn std::fmt::Display| format!("{}: {x}", path.display());
    let file = File::create(path).map_err(|x| e(&x))?;
    let props = WriterProperties::builder().set_compression(Compression::SNAPPY).build();
    let mut w = ArrowWriter::try_new(file, batch.schema(), Some(props)).map_err(|x| e(&x))?;
    w.write(&batch).map_err(|x| e(&x))?;
    w.close().map_err(|x| e(&x))?;
    OpenOptions::new().write(true).open(path).and_then(|f| f.sync_all()).map_err(|x| e(&x))?;
    Ok(())
}

pub fn write_frames(path: &Path, rows: &[FrameRecord]) -> TableResult<()> {
    let cols: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from_iter_values(rows.iter().map(|r| r.frame_idx))),
        Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.tick_ns))),
        Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.capture_ns))),
        Arc::new(BooleanArray::from(rows.iter().map(|r| r.repeated).collect::<Vec<_>>())),
    ];
    write_batch(path, RecordBatch::try_new(frames_schema(), cols).map_err(|e| e.to_string())?)
}

pub fn write_inputs(path: &Path, rows: &[InputEvent]) -> TableResult<()> {
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.t_ns))),
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.device.as_str()))),
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.kind.as_str()))),
        Arc::new(UInt32Array::from_iter_values(rows.iter().map(|r| r.code))),
        Arc::new(Float32Array::from_iter_values(rows.iter().map(|r| r.value))),
    ];
    write_batch(path, RecordBatch::try_new(inputs_schema(), cols).map_err(|e| e.to_string())?)
}

pub fn write_focus(path: &Path, rows: &[FocusRecord]) -> TableResult<()> {
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.t_ns))),
        Arc::new(BooleanArray::from(rows.iter().map(|r| r.focused).collect::<Vec<_>>())),
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.game_id.as_str()))),
    ];
    write_batch(path, RecordBatch::try_new(focus_schema(), cols).map_err(|e| e.to_string())?)
}

/// Reads a whole table; returns its Arrow schema and batches.
pub fn read_table(path: &Path) -> TableResult<(SchemaRef, Vec<RecordBatch>)> {
    let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let b = ParquetRecordBatchReaderBuilder::try_new(f).map_err(|e| e.to_string())?;
    let schema = b.schema().clone();
    let batches = b.build().map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    Ok((schema, batches))
}

fn col<'a, T: 'static>(b: &'a RecordBatch, name: &str) -> &'a T {
    b.column_by_name(name).and_then(|c| c.as_any().downcast_ref::<T>()).unwrap_or_else(|| panic!("column {name}"))
}

pub fn parse_device(s: &str) -> Option<Device> {
    Some(match s {
        "keyboard" => Device::Keyboard,
        "mouse" => Device::Mouse,
        "gamepad" => Device::Gamepad,
        _ => return None,
    })
}

pub fn parse_kind(s: &str) -> Option<EventKind> {
    Some(match s {
        "key_down" => EventKind::KeyDown,
        "key_up" => EventKind::KeyUp,
        "mouse_move" => EventKind::MouseMove,
        "mouse_button" => EventKind::MouseButton,
        "wheel" => EventKind::Wheel,
        "axis" => EventKind::Axis,
        "button" => EventKind::Button,
        _ => return None,
    })
}

pub fn read_frames(path: &Path) -> TableResult<Vec<FrameRecord>> {
    let (_, batches) = read_table(path)?;
    let mut out = Vec::new();
    for b in &batches {
        let (a, t, c, r) = (
            col::<UInt32Array>(b, "frame_idx"),
            col::<Int64Array>(b, "tick_ns"),
            col::<Int64Array>(b, "capture_ns"),
            col::<BooleanArray>(b, "repeated"),
        );
        for i in 0..b.num_rows() {
            out.push(FrameRecord { frame_idx: a.value(i), tick_ns: t.value(i), capture_ns: c.value(i), repeated: r.value(i) });
        }
    }
    Ok(out)
}

pub fn read_inputs(path: &Path) -> TableResult<Vec<InputEvent>> {
    let (_, batches) = read_table(path)?;
    let mut out = Vec::new();
    for b in &batches {
        let (t, d, k, c, v) = (
            col::<Int64Array>(b, "t_ns"),
            col::<StringArray>(b, "device"),
            col::<StringArray>(b, "kind"),
            col::<UInt32Array>(b, "code"),
            col::<Float32Array>(b, "value"),
        );
        for i in 0..b.num_rows() {
            out.push(InputEvent {
                t_ns: t.value(i),
                device: parse_device(d.value(i)).ok_or("bad device")?,
                kind: parse_kind(k.value(i)).ok_or("bad kind")?,
                code: c.value(i),
                value: v.value(i),
            });
        }
    }
    Ok(out)
}

pub fn read_focus(path: &Path) -> TableResult<Vec<FocusRecord>> {
    let (_, batches) = read_table(path)?;
    let mut out = Vec::new();
    for b in &batches {
        let (t, f, g) = (col::<Int64Array>(b, "t_ns"), col::<BooleanArray>(b, "focused"), col::<StringArray>(b, "game_id"));
        for i in 0..b.num_rows() {
            out.push(FocusRecord { t_ns: t.value(i), focused: f.value(i), game_id: g.value(i).to_string() });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_schema() {
        let dir = std::env::temp_dir().join(format!("caprec-tables-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let frames = vec![
            FrameRecord { frame_idx: 0, tick_ns: 10, capture_ns: 5, repeated: false },
            FrameRecord { frame_idx: 1, tick_ns: 20, capture_ns: 5, repeated: true },
        ];
        let inputs = vec![InputEvent { t_ns: 11, device: Device::Mouse, kind: EventKind::MouseMove, code: 0, value: -2.5 }];
        let focus = vec![FocusRecord { t_ns: 10, focused: true, game_id: "game.exe".into() }];
        write_frames(&dir.join("f.parquet"), &frames).unwrap();
        write_inputs(&dir.join("i.parquet"), &inputs).unwrap();
        write_focus(&dir.join("c.parquet"), &focus).unwrap();
        write_inputs(&dir.join("empty.parquet"), &[]).unwrap();
        assert_eq!(read_frames(&dir.join("f.parquet")).unwrap(), frames);
        assert_eq!(read_inputs(&dir.join("i.parquet")).unwrap(), inputs);
        assert_eq!(read_focus(&dir.join("c.parquet")).unwrap(), focus);
        assert!(read_inputs(&dir.join("empty.parquet")).unwrap().is_empty());
        let (s, _) = read_table(&dir.join("f.parquet")).unwrap();
        assert_eq!(s.fields().iter().map(|f| f.name().as_str()).collect::<Vec<_>>(), ["frame_idx", "tick_ns", "capture_ns", "repeated"]);
        assert_eq!(s.field(0).data_type(), &DataType::UInt32);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
