use super::{CompressOptions, ConvertOptions, Format, Raster, Resize};
use crate::Result;
use rustix::fs::{CWD, RenameFlags, renameat_with};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

const CONVERT_HELP: &str = "\
Usage: perch convert IMAGE... --to FORMAT [OPTIONS]

Converts images to PNG, JPEG, WebP, AVIF, JXL, TIFF, BMP, GIF, ICO or QOI,
taking the first frame of animations. Each result is written next to its source
with the new extension, and a source that already has it gains a -converted
suffix. EXIF orientation is applied to the pixels. Color profiles are kept,
except in BMP, GIF and QOI, which are converted to sRGB and hold no metadata.
Lossy XYB JXL sources are converted to sRGB. TIFF metadata is limited to XMP.

Options:
  --to FORMAT       Output format
  --quality N       JPEG, WebP, AVIF and JXL quality from 1 to 100 (default 85)
  --lossless        Encode WebP, AVIF and JXL losslessly
  --resize SIZE     Scale to fit WxH, to a width Wx or height xH, or by N%
  --bg COLOR        Background for JPEG and BMP transparency (default #ffffff)
  --keep-metadata   Keep EXIF and XMP metadata
  --out PATH        Output directory, or output file for one image
  --overwrite       Replace existing files
  --jobs N          Images to process at once
";

const COMPRESS_HELP: &str = "\
Usage: perch compress IMAGE... [OPTIONS]

Shrinks PNG, JPEG, WebP, TIFF, AVIF and JXL images. Compression is lossless
unless --lossy is given, and EXIF and XMP metadata are removed while color
profiles and orientation stay, except that lossy XYB JXL is converted to sRGB.
Lossy AVIF and JXL images need --lossy, and animated ones, those above 8 bits
per channel and JXL with channels beyond RGB or grayscale and one alpha are
refused. Each result is written next to its source with a -min suffix, and
nothing is written when the result is not smaller.

Options:
  --lossy           Re-encode PNG, JPEG, WebP, AVIF and JXL stills lossily
  --quality N       Lossy quality from 1 to 100 (default 85)
  --keep-metadata   Keep EXIF and XMP metadata, of which TIFF keeps only XMP
  --out PATH        Output directory, or output file for one image
  --overwrite       Replace the originals or existing files
  --jobs N          Images to process at once
";

/// Runs `perch convert` or `perch compress` with the arguments after the command.
pub fn run(command: &str, args: &[OsString]) -> Result<()> {
    let convert = command == "convert";
    let mut files = Vec::new();
    let mut format = None;
    let mut quality = 85;
    let (mut lossless, mut lossy, mut keep_metadata, mut overwrite) = (false, false, false, false);
    let mut resize = None;
    let mut background = [255; 3];
    let mut out = None;
    let mut jobs = thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str().filter(|t| t.starts_with('-') && t.len() > 1) else {
            files.push(PathBuf::from(arg));
            continue;
        };
        let (name, inline) = match text.split_once('=') {
            Some((name, value)) => (name, Some(OsString::from(value))),
            None => (text, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next().cloned())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        let text = |value: OsString| {
            value
                .into_string()
                .map_err(|_| format!("invalid {name} value"))
        };
        match name {
            "-h" | "--help" => {
                print!("{}", if convert { CONVERT_HELP } else { COMPRESS_HELP });
                return Ok(());
            }
            "--" => files.extend(args.by_ref().map(PathBuf::from)),
            "--quality" => {
                quality = text(value()?)?
                    .parse()
                    .ok()
                    .filter(|q| (1..=100).contains(q))
                    .ok_or("--quality must be from 1 to 100")?;
            }
            "--keep-metadata" => keep_metadata = true,
            "--out" => out = Some(PathBuf::from(value()?)),
            "--overwrite" => overwrite = true,
            "--jobs" => {
                jobs = text(value()?)?
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or("--jobs must be a positive number")?;
            }
            "--to" if convert => {
                let value = text(value()?)?;
                format = Some(Format::parse(&value).ok_or(format!("unsupported format {value}"))?);
            }
            "--lossless" if convert => lossless = true,
            "--resize" if convert => {
                let value = text(value()?)?;
                resize = Some(Resize::parse(&value).ok_or(format!("invalid size {value}"))?);
            }
            "--bg" if convert => {
                let value = text(value()?)?;
                background = color(&value).ok_or(format!("invalid color {value}"))?;
            }
            "--lossy" if !convert => lossy = true,
            _ => return Err(format!("unknown option {name}, see perch {command} --help").into()),
        }
    }
    if files.is_empty() {
        return Err(format!("no images given, see perch {command} --help").into());
    }
    let directory = out.as_deref().is_some_and(Path::is_dir);
    if out.is_some() && !directory && files.len() > 1 {
        return Err("--out must be an existing directory for several images".into());
    }
    if convert {
        let format = format.ok_or("--to is required")?;
        let options = ConvertOptions {
            format,
            quality,
            lossless,
            resize,
            background,
            keep_metadata,
        };
        let targets = targets(&files, |source| {
            let target = source.with_extension(format.extension());
            Some(match &out {
                Some(dir) if directory => dir.join(target.file_name()?),
                Some(file) => file.clone(),
                None if target == source && !overwrite => suffixed(source, "-converted")?,
                None => target,
            })
        })?;
        batch(&targets, jobs, |source, target| {
            check(target, overwrite)?;
            let bytes = fs::read(source)?;
            let output = super::convert(Raster::load(&bytes, Some(source))?, &options)?;
            write(Some(source), target, &output, overwrite)?;
            Ok(format!(
                "{} -> {}: {} -> {}",
                source.display(),
                target.display(),
                size(bytes.len()),
                size(output.len())
            ))
        })
    } else {
        let options = CompressOptions {
            lossy,
            quality,
            keep_metadata,
        };
        let targets = targets(&files, |source| match &out {
            Some(dir) if directory => Some(dir.join(source.file_name()?)),
            Some(file) => Some(file.clone()),
            None if overwrite => Some(source.to_path_buf()),
            None => suffixed(source, "-min"),
        })?;
        batch(&targets, jobs, |source, target| {
            if target != source {
                check(target, overwrite)?;
            }
            let bytes = fs::read(source)?;
            let Some(output) = super::compress(&bytes, &options)? else {
                let note = if keep_metadata {
                    ""
                } else {
                    ", metadata unchanged"
                };
                return Ok(format!(
                    "{}: already optimal at {}{note}",
                    source.display(),
                    size(bytes.len())
                ));
            };
            write(Some(source), target, &output, overwrite)?;
            let saved = 100 - output.len() * 100 / bytes.len();
            Ok(format!(
                "{}: {} -> {} (-{saved}%)",
                target.display(),
                size(bytes.len()),
                size(output.len())
            ))
        })
    }
}

fn color(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    let value = u32::from_str_radix(hex, 16)
        .ok()
        .filter(|_| hex.len() == 6)?;
    let [_, r, g, b] = value.to_be_bytes();
    Some([r, g, b])
}

/// Pairs each source with its output and rejects outputs shared by several sources.
fn targets(
    files: &[PathBuf],
    target: impl Fn(&Path) -> Option<PathBuf>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut pairs = Vec::new();
    let mut seen = HashSet::new();
    for source in files {
        let output = target(source).ok_or_else(|| format!("invalid path {}", source.display()))?;
        if !seen.insert(resolved(&output)) {
            return Err(format!("several images would be written to {}", output.display()).into());
        }
        pairs.push((source.clone(), output));
    }
    Ok(pairs)
}

/// Resolves the directory of `path` so aliases of one output compare equal.
fn resolved(path: &Path) -> PathBuf {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    match (
        fs::canonicalize(parent.unwrap_or(Path::new("."))),
        path.file_name(),
    ) {
        (Ok(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

fn suffixed(path: &Path, suffix: &str) -> Option<PathBuf> {
    let mut name = path.file_stem()?.to_os_string();
    name.push(suffix);
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    Some(path.with_file_name(name))
}

fn check(target: &Path, overwrite: bool) -> Result<()> {
    if !overwrite && target.try_exists()? {
        return Err(format!("{} exists, use --overwrite to replace it", target.display()).into());
    }
    Ok(())
}

/// Writes a temporary file beside the target and renames it into place, with the
/// permissions of `source` when there is one.
pub(crate) fn write(
    source: Option<&Path>,
    target: &Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<()> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let name = target.file_name().ok_or("invalid output path")?;
    let (temp, mut file) = loop {
        let mut temp = OsString::from(".");
        temp.push(name);
        temp.push(format!(
            ".{}-{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = target.with_file_name(temp);
        // Only the owner may read the bytes until the source permissions apply.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if source.is_some() { 0o600 } else { 0o666 })
            .open(&temp);
        match file {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        if let Some(source) = source {
            file.set_permissions(fs::metadata(source)?.permissions())?;
        }
        file.sync_all()?;
        if overwrite {
            fs::rename(&temp, target)?;
        } else {
            renameat_with(CWD, &temp, CWD, target, RenameFlags::NOREPLACE).map_err(|e| {
                if e == rustix::io::Errno::EXIST {
                    let text =
                        format!("{} exists, use --overwrite to replace it", target.display());
                    std::io::Error::new(ErrorKind::AlreadyExists, text)
                } else {
                    e.into()
                }
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Processes images on up to `jobs` threads and reports each result as it finishes.
fn batch(
    targets: &[(PathBuf, PathBuf)],
    jobs: usize,
    task: impl Fn(&Path, &Path) -> Result<String> + Sync,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..jobs.min(targets.len()) {
            scope.spawn(|| {
                while let Some((source, target)) = targets.get(next.fetch_add(1, Ordering::Relaxed))
                {
                    match task(source, target) {
                        Ok(line) => println!("{line}"),
                        Err(error) => {
                            eprintln!("perch: {}: {error}", source.display());
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    match failed.into_inner() {
        0 => Ok(()),
        n => Err(format!("{n} of {} images failed", targets.len()).into()),
    }
}

pub(crate) fn size(bytes: usize) -> String {
    let mut value = bytes as f64;
    for unit in ["B", "KiB", "MiB"] {
        if value < 1024.0 {
            return if unit == "B" {
                format!("{bytes} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} GiB")
}
