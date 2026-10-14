//! CSV columns are bound to source identities, not potentially duplicate display labels.
use crate::model::{Snapshot, Unit};
use anyhow::{Context, Result};
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ColumnKey {
    System(String),
    Sensor {
        chip: String,
        attribute: String,
        unit: Unit,
    },
}
struct Field {
    key: ColumnKey,
    name: String,
    value: String,
}
struct Column {
    key: ColumnKey,
    name: String,
}
struct Schema {
    columns: Vec<Column>,
}

impl Schema {
    fn new(snapshot: &Snapshot) -> Self {
        let mut names = HashSet::new();
        let columns = fields(snapshot)
            .into_iter()
            .map(|field| {
                let mut name = field.name.clone();
                let mut suffix = 2;
                while !names.insert(name.clone()) {
                    name = format!("{}#{suffix}", field.name);
                    suffix += 1;
                }
                Column {
                    key: field.key,
                    name,
                }
            })
            .collect();
        Self { columns }
    }
    fn row(&self, snapshot: &Snapshot) -> Vec<String> {
        let values = fields(snapshot)
            .into_iter()
            .map(|field| (field.key, field.value))
            .collect::<HashMap<_, _>>();
        self.columns
            .iter()
            .map(|column| values.get(&column.key).cloned().unwrap_or_default())
            .collect()
    }
}

pub struct Logger {
    file: crate::csv_writer::Output,
    schema: Schema,
    path: String,
}
impl Logger {
    pub fn create(snapshot: &Snapshot, path: Option<&str>) -> Result<Self> {
        let generated = path.is_none();
        let path = path
            .map(str::to_owned)
            .unwrap_or_else(|| format!("rustinfo_{}.csv", Local::now().format("%Y%m%d_%H%M%S_%f")));
        let schema = Schema::new(snapshot);
        let mut file = crate::csv_writer::Output::open(&path, generated)?;
        write_record(
            &mut file,
            schema.columns.iter().map(|column| column.name.as_str()),
        )?;
        file.flush()?;
        Ok(Self { file, schema, path })
    }
    pub fn write(&mut self, snapshot: &Snapshot) -> Result<()> {
        let row = self.schema.row(snapshot);
        write_record(&mut self.file, row.iter().map(String::as_str))
            .with_context(|| format!("写入 {} 失败", self.path))?;
        self.file.flush()?;
        Ok(())
    }
    pub fn finish(&mut self) -> Result<()> {
        self.file.finish().context("刷新 CSV 失败")
    }
    pub fn path(&self) -> &str {
        &self.path
    }
}

fn write_record<'a>(
    writer: &mut impl Write,
    values: impl IntoIterator<Item = &'a str>,
) -> io::Result<()> {
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        if value.contains([',', '"', '\n', '\r']) {
            write!(writer, "\"{}\"", value.replace('"', "\"\""))?;
        } else {
            writer.write_all(value.as_bytes())?;
        }
    }
    writer.write_all(b"\n")
}
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn fields(snapshot: &Snapshot) -> Vec<Field> {
    let mut out = Vec::new();
    let mut system = |name: String, value: String| {
        out.push(Field {
            key: ColumnKey::System(name.clone()),
            name,
            value,
        })
    };
    system(
        "ts".into(),
        snapshot.time.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    );
    system(
        "cpu_total_pct".into(),
        format!("{:.2}", snapshot.cpu.total_pct),
    );
    system(
        "cpu_pkg_w".into(),
        snapshot
            .cpu
            .pkg_w
            .map(|v| format!("{v:.3}"))
            .unwrap_or_default(),
    );
    for (i, pct) in snapshot.cpu.per_pct.iter().enumerate() {
        let id = snapshot.cpu.ids.get(i).copied().unwrap_or(i);
        system(format!("t{id:02}_pct"), format!("{pct:.2}"));
    }
    for (i, mhz) in snapshot.cpu.per_mhz.iter().enumerate() {
        let id = snapshot.cpu.ids.get(i).copied().unwrap_or(i);
        system(format!("t{id:02}_mhz"), format!("{mhz:.0}"));
    }
    let used = snapshot
        .mem
        .total_kib
        .saturating_sub(snapshot.mem.avail_kib);
    let percentage = if snapshot.mem.total_kib > 0 {
        used as f64 / snapshot.mem.total_kib as f64 * 100.0
    } else {
        0.0
    };
    system("mem_used_pct".into(), format!("{percentage:.2}"));
    system(
        "mem_used_mib".into(),
        format!("{:.1}", used as f64 / 1024.0),
    );
    system(
        "swap_used_mib".into(),
        format!(
            "{:.1}",
            snapshot
                .mem
                .swap_total_kib
                .saturating_sub(snapshot.mem.swap_free_kib) as f64
                / 1024.0
        ),
    );
    system("load1".into(), format!("{:.3}", snapshot.load[0]));
    let battery = snapshot.battery.as_ref();
    for (name, value, precision) in [
        ("batt_pct", battery.and_then(|b| b.capacity_pct), 0),
        ("batt_v", battery.and_then(|b| b.volts), 3),
        ("batt_a", battery.and_then(|b| b.amps), 3),
        ("batt_w", battery.and_then(|b| b.watts), 3),
        ("batt_health_pct", battery.and_then(|b| b.health_pct), 1),
    ] {
        system(
            name.into(),
            value
                .map(|v| format!("{v:.precision$}"))
                .unwrap_or_default(),
        );
    }
    system(
        "batt_runtime_min".into(),
        battery
            .and_then(|b| b.runtime_min)
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    for (name, value) in [
        ("vram_used_mib", snapshot.vram.as_ref().map(|v| v.used_mib)),
        (
            "vram_total_mib",
            snapshot.vram.as_ref().map(|v| v.total_mib),
        ),
    ] {
        system(
            name.into(),
            value.map(|v| format!("{v:.0}")).unwrap_or_default(),
        );
    }
    for chip in &snapshot.chips {
        for reading in &chip.readings {
            out.push(Field {
                key: ColumnKey::Sensor {
                    chip: chip.id.clone(),
                    attribute: reading.key.as_ref().unwrap_or(&reading.label).clone(),
                    unit: reading.unit,
                },
                name: format!(
                    "{}.{}_{}",
                    sanitize(&chip.name),
                    sanitize(&reading.label),
                    reading.unit.csv_suffix()
                ),
                value: if reading.value.is_finite() {
                    format!("{:.3}", reading.value)
                } else {
                    String::new()
                },
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chip, Reading};
    fn chip(id: &str, value: f64) -> Chip {
        Chip {
            id: id.into(),
            name: "nvme".into(),
            readings: vec![Reading {
                key: Some("temp1_input".into()),
                label: "Composite".into(),
                value,
                unit: Unit::TempC,
            }],
        }
    }
    #[test]
    fn identical_device_names_never_overwrite_or_shift_csv_values() {
        let mut snapshot = Snapshot {
            chips: vec![chip("drive-a", 30.0), chip("drive-b", 40.0)],
            ..Snapshot::default()
        };
        let schema = Schema::new(&snapshot);
        let a = schema
            .columns
            .iter()
            .position(|column| column.name == "nvme.Composite_C")
            .unwrap();
        let b = schema
            .columns
            .iter()
            .position(|column| column.name == "nvme.Composite_C#2")
            .unwrap();
        snapshot.chips = vec![chip("drive-b", 45.0)];
        let row = schema.row(&snapshot);
        assert_eq!(row[a], "");
        assert_eq!(row[b], "45.000");
    }
    #[test]
    fn optional_system_fields_have_columns_even_when_the_first_read_is_missing() {
        let mut snapshot = Snapshot::default();
        let schema = Schema::new(&snapshot);
        let pkg = schema
            .columns
            .iter()
            .position(|column| column.name == "cpu_pkg_w")
            .unwrap();
        snapshot.cpu.pkg_w = Some(12.5);
        assert_eq!(schema.row(&snapshot)[pkg], "12.500");
    }
    #[test]
    fn sparse_cpu_ids_survive_csv_export() {
        let mut snapshot = Snapshot::default();
        snapshot.cpu.ids = vec![2, 10];
        snapshot.cpu.per_pct = vec![5.0, 8.0];
        let schema = Schema::new(&snapshot);
        assert!(schema.columns.iter().any(|column| column.name == "t10_pct"));
        assert!(!schema.columns.iter().any(|column| column.name == "t00_pct"));
    }
    #[test]
    fn a_real_csv_file_keeps_a_rectangular_schema_when_a_device_disappears() {
        let fixture = crate::test_support::Fixture::new();
        let path = fixture.path().join("log.csv");
        let mut snapshot = Snapshot {
            chips: vec![chip("drive-a", 30.0)],
            ..Snapshot::default()
        };
        let mut logger = Logger::create(&snapshot, Some(path.to_str().unwrap())).unwrap();
        logger.write(&snapshot).unwrap();
        snapshot.chips.clear();
        logger.write(&snapshot).unwrap();
        logger.finish().unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let widths = text
            .lines()
            .map(|line| line.split(',').count())
            .collect::<Vec<_>>();
        assert_eq!(widths.len(), 3);
        assert!(widths.iter().all(|width| *width == widths[0]));
    }
}
