//! The perf gate's register check (ADR 0062's addendum of 2026-10-15): the registers of
//! every pipeline the gate's scenes create, as the driver reports them
//! (`VK_KHR_pipeline_executable_properties`), against this GPU vendor's baseline in
//! `tests/golden/perf/<vendor>.registers.tsv`.
//!
//! Registers do not vary from run to run as times do, so the smallest growth shows. A
//! pipeline using more is a warning, not a failure (the owner, 2026-10-15): on the iGPU
//! more registers mean fewer waves in flight, and the time gate let 64 → 70 VGPRs through
//! (ADR 0093) where 64 → 72 had cost the forward pass 9–13 % (ADR 0058).

use engine_rhi::PipelineRecord;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

/// Watched statistics per pipeline: (stage, statistic) → value.
pub type Registers = BTreeMap<String, BTreeMap<(String, String), f64>>;

/// Whether the check watches a driver statistic: the vector registers a stage uses
/// (AMD's `numUsedVgprs`, NVIDIA's `Register Count`), which bound how many waves a core
/// keeps in flight, and the memory registers spill to (AMD's `scratchMemUsageInBytes`).
/// Not AMD's scalar registers (on RDNA they never bound occupancy), nor shared memory (a
/// shader declares it), nor NVIDIA's `Local Memory Size` (2^36 for every stage).
pub fn watched(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    (name.contains("vgpr") && !name.contains("avail"))
        || name == "register count"
        || name.contains("scratch")
}

/// The compiled shaders in the repository (`crates/*/shaders/**/*.spv` under `root`) by
/// the hash of their SPIR-V ([`engine_rhi::spirv_hash`]), named `module.entry` as their
/// files are: a pipeline's name stays when its shaders change.
pub fn shader_names(root: &Path) -> HashMap<u128, String> {
    let mut names = HashMap::new();
    let Ok(crates) = std::fs::read_dir(root.join("crates")) else {
        return names;
    };
    let mut dirs: Vec<_> = crates
        .flatten()
        .map(|c| c.path().join("shaders"))
        .filter(|p| p.is_dir())
        .collect();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "spv")
                && let (Ok(bytes), Some(stem)) = (std::fs::read(&path), path.file_stem())
            {
                let words: Vec<u32> = bytes
                    .chunks_exact(4)
                    .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                    .collect();
                names.insert(
                    engine_rhi::spirv_hash(&words),
                    stem.to_string_lossy().into_owned(),
                );
            }
        }
    }
    names
}

/// A pipeline's name: its stages' shader files (`module.entry`; the entry point and the
/// hash's start for SPIR-V made at run time), its specialization constants and, for
/// graphics, the attachments it writes.
pub fn pipeline_name(record: &PipelineRecord, shaders: &HashMap<u128, String>) -> String {
    let mut name = record
        .stages
        .iter()
        .map(|(entry, hash)| {
            shaders
                .get(hash)
                .cloned()
                .unwrap_or_else(|| format!("{entry}@{:08x}", hash >> 96))
        })
        .collect::<Vec<_>>()
        .join("+");
    if !record.specialization.is_empty() {
        let values: Vec<String> = record.specialization.iter().map(u32::to_string).collect();
        let _ = write!(name, " spec={}", values.join(","));
    }
    if !record.attachments.is_empty() {
        let _ = write!(name, " {}", record.attachments);
    }
    name
}

/// Adds the watched statistics of `records` to `into`; a pipeline recorded again keeps
/// its largest values.
pub fn add(into: &mut Registers, records: &[PipelineRecord], shaders: &HashMap<u128, String>) {
    for record in records {
        let stats = into.entry(pipeline_name(record, shaders)).or_default();
        for s in record.statistics.iter().filter(|s| watched(&s.name)) {
            let value = stats
                .entry((s.stage.clone(), s.name.clone()))
                .or_insert(s.value);
            *value = value.max(s.value);
        }
    }
}

/// Reads a baseline: `pipeline \t stage \t statistic \t value` lines (none if the file is
/// missing).
pub fn read(path: &Path) -> Registers {
    let mut registers = Registers::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return registers;
    };
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let mut f = line.split('\t');
        if let (Some(pipeline), Some(stage), Some(statistic), Some(value)) =
            (f.next(), f.next(), f.next(), f.next())
            && let Ok(value) = value.trim().parse()
        {
            registers
                .entry(pipeline.to_owned())
                .or_default()
                .insert((stage.to_owned(), statistic.to_owned()), value);
        }
    }
    registers
}

/// The baseline file's text: `header` as comment lines, then one line per statistic.
pub fn format(registers: &Registers, header: &str) -> String {
    let mut text = String::new();
    for line in header.lines() {
        let _ = writeln!(text, "# {line}");
    }
    text.push_str("# pipeline\tstage\tstatistic\tvalue\n");
    for (pipeline, stats) in registers {
        for ((stage, statistic), value) in stats {
            let _ = writeln!(text, "{pipeline}\t{stage}\t{statistic}\t{value}");
        }
    }
    text
}

/// One statistic of one pipeline that moved.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// The pipeline ([`pipeline_name`]).
    pub pipeline: String,
    /// The executable (stage), as the driver names it.
    pub stage: String,
    /// The statistic, as the driver names it.
    pub statistic: String,
    /// Its value in the baseline.
    pub before: f64,
    /// Its value now.
    pub now: f64,
}

/// What moved against the baseline.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /// Statistics that grew: the warnings.
    pub grew: Vec<Change>,
    /// Statistics that shrank.
    pub shrank: Vec<Change>,
    /// Pipelines the baseline does not have.
    pub new: Vec<String>,
    /// The baseline's pipelines not measured (only meaningful when every scene ran).
    pub gone: Vec<String>,
}

/// Compares `measured` with `baseline`.
pub fn compare(baseline: &Registers, measured: &Registers) -> Changes {
    let mut changes = Changes::default();
    for (pipeline, stats) in measured {
        let Some(before) = baseline.get(pipeline) else {
            changes.new.push(pipeline.clone());
            continue;
        };
        for ((stage, statistic), &now) in stats {
            let Some(&was) = before.get(&(stage.clone(), statistic.clone())) else {
                continue;
            };
            let change = || Change {
                pipeline: pipeline.clone(),
                stage: stage.clone(),
                statistic: statistic.clone(),
                before: was,
                now,
            };
            if now > was {
                changes.grew.push(change());
            } else if now < was {
                changes.shrank.push(change());
            }
        }
    }
    changes.gone = baseline
        .keys()
        .filter(|p| !measured.contains_key(*p))
        .cloned()
        .collect();
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_rhi::PipelineStatistic;

    fn statistic(stage: &str, name: &str, value: f64) -> PipelineStatistic {
        PipelineStatistic {
            stage: stage.to_owned(),
            name: name.to_owned(),
            value,
        }
    }

    fn registers(rows: &[(&str, &str, &str, f64)]) -> Registers {
        let mut r = Registers::new();
        for &(pipeline, stage, name, value) in rows {
            r.entry(pipeline.to_owned())
                .or_default()
                .insert((stage.to_owned(), name.to_owned()), value);
        }
        r
    }

    #[test]
    fn watches_vector_registers_and_spills_only() {
        // AMD's proprietary driver and NVIDIA's, as they name them (ref-1, 2026-10-15).
        for name in ["numUsedVgprs", "scratchMemUsageInBytes", "Register Count"] {
            assert!(watched(name), "{name}");
        }
        for name in [
            "numUsedSgprs",
            "numAvailableVgprs",
            "ldsSizePerLocalWorkGroup",
            "ldsUsageSizeInBytes",
            "Binary Size",
            "Stack Size",
            "Local Memory Size",
            "Shared Memory Size",
            "Input Count",
        ] {
            assert!(!watched(name), "{name}");
        }
    }

    #[test]
    fn pipelines_are_named_by_their_shader_files() {
        let shaders = HashMap::from([(1u128, "forward.vertexMain".to_owned())]);
        let record = PipelineRecord {
            stages: vec![
                ("vertexMain".to_owned(), 1),
                ("fragmentMain".to_owned(), 0xabcd_ef01 << 96),
            ],
            specialization: vec![1, 0],
            attachments: "R16G16B16A16_SFLOAT/D32_SFLOAT x4".to_owned(),
            statistics: Vec::new(),
        };
        assert_eq!(
            pipeline_name(&record, &shaders),
            "forward.vertexMain+fragmentMain@abcdef01 spec=1,0 R16G16B16A16_SFLOAT/D32_SFLOAT x4"
        );
    }

    #[test]
    fn a_pipeline_recorded_twice_keeps_its_largest_values() {
        let record = |vgprs: f64| PipelineRecord {
            stages: vec![("main".to_owned(), 7)],
            specialization: Vec::new(),
            attachments: String::new(),
            statistics: vec![
                statistic("ShaderProperties.cs", "numUsedVgprs", vgprs),
                statistic("ShaderProperties.cs", "numUsedSgprs", 30.0),
            ],
        };
        let shaders = HashMap::from([(7u128, "blur.main".to_owned())]);
        let mut into = Registers::new();
        add(
            &mut into,
            &[record(40.0), record(48.0), record(44.0)],
            &shaders,
        );
        assert_eq!(
            into,
            registers(&[("blur.main", "ShaderProperties.cs", "numUsedVgprs", 48.0)])
        );
    }

    #[test]
    fn baselines_read_back_what_was_written() {
        let r = registers(&[
            ("a.main spec=1", "VS", "Register Count", 48.0),
            ("a.main spec=1", "FS", "Register Count", 56.0),
            (
                "b.main",
                "ShaderProperties.cs",
                "scratchMemUsageInBytes",
                0.0,
            ),
        ]);
        let path = std::env::temp_dir().join(format!("registers-{}.tsv", std::process::id()));
        std::fs::write(
            &path,
            format(
                &r,
                "registers
second line",
            ),
        )
        .unwrap();
        let back = read(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(back, r);
    }

    #[test]
    fn changes_against_the_baseline() {
        let baseline = registers(&[
            ("forward", "ps", "numUsedVgprs", 64.0),
            ("forward", "gs", "numUsedVgprs", 49.0),
            ("bloom", "cs", "numUsedVgprs", 32.0),
            ("old", "cs", "numUsedVgprs", 10.0),
        ]);
        let measured = registers(&[
            ("forward", "ps", "numUsedVgprs", 70.0),
            ("forward", "gs", "numUsedVgprs", 49.0),
            ("bloom", "cs", "numUsedVgprs", 24.0),
            ("new", "cs", "numUsedVgprs", 8.0),
        ]);
        let changes = compare(&baseline, &measured);
        let change = |pipeline: &str, stage: &str, before, now| Change {
            pipeline: pipeline.to_owned(),
            stage: stage.to_owned(),
            statistic: "numUsedVgprs".to_owned(),
            before,
            now,
        };
        assert_eq!(changes.grew, vec![change("forward", "ps", 64.0, 70.0)]);
        assert_eq!(changes.shrank, vec![change("bloom", "cs", 32.0, 24.0)]);
        assert_eq!(changes.new, vec!["new".to_owned()]);
        assert_eq!(changes.gone, vec!["old".to_owned()]);
    }
}
