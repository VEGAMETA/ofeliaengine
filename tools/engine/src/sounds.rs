//! `ofeliaengine sounds build` (ADR 0105): a game's sounds folder into a sound pack, every
//! file as it is (no re-encoding), for a release build to read.

use engine_assets::sounds::write_sound_pack;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

pub(crate) struct SoundsArgs {
    pub(crate) dir: PathBuf,
    pub(crate) out: PathBuf,
    pub(crate) level: i32,
}

pub(crate) fn parse_sounds(mut args: impl Iterator<Item = String>) -> Result<SoundsArgs, String> {
    match args.next().as_deref() {
        Some("build") => {}
        Some(other) => return Err(format!("unknown sounds command `{other}`")),
        None => return Err(String::from("missing `build`")),
    }
    let (mut dir, mut out, mut level) = (None, None, 19);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-o" | "--out" => out = Some(PathBuf::from(value("-o")?)),
            "--level" => {
                let given = value("--level")?;
                level = given
                    .parse()
                    .ok()
                    .filter(|l| (0..=22).contains(l))
                    .ok_or_else(|| format!("--level {given}: 0 to 22"))?;
            }
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            path if dir.is_none() => dir = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected `{extra}`")),
        }
    }
    Ok(SoundsArgs {
        dir: dir.ok_or("no sounds folder given")?,
        out: out.ok_or("no output given (-o <OUT.pack>)")?,
        level,
    })
}

pub(crate) fn run_sounds(args: &SoundsArgs) -> Result<ExitCode, String> {
    let start = Instant::now();
    let report = write_sound_pack(&args.dir, &args.out, args.level)
        .map_err(|e| format!("{}: {e}", args.out.display()))?;
    for (path, why) in &report.skipped {
        println!("left out {path}: {why}");
    }
    let streamed = report
        .sounds
        .iter()
        .filter(|(_, s)| {
            s.frames
                .is_none_or(|f| f as f64 >= 10.0 * f64::from(s.sample_rate.max(1)))
        })
        .count();
    println!(
        "{} sounds ({} of 10 s or more, streamed by default) into {}: {:.1} MiB in {:.0} ms",
        report.sounds.len(),
        streamed,
        args.out.display(),
        report.stats.file_bytes as f64 / f64::from(1u32 << 20),
        start.elapsed().as_secs_f64() * 1e3
    );
    Ok(ExitCode::SUCCESS)
}
