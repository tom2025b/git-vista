use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicU64, Ordering};

use git_vista_plan_runner::{
    checkpoint_from_yaml, checkpoint_to_yaml, manifest_from_yaml, manifest_sha256, run_remaining,
    Checkpoint, RunFailure,
};

const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn main() {
    match run() {
        Ok(()) => {}
        Err(CliFailure::Step { message, code }) => {
            eprintln!("gv-run: {message}");
            process::exit(normalize_exit_code(code));
        }
        Err(CliFailure::Other(message)) => {
            eprintln!("gv-run: {message}");
            process::exit(1);
        }
    }
}

enum CliFailure {
    Step { message: String, code: i32 },
    Other(String),
}

fn run() -> Result<(), CliFailure> {
    let (manifest_path, state_path) = parse_args()?;
    let bytes = read_manifest(&manifest_path)?;
    let yaml = std::str::from_utf8(&bytes)
        .map_err(|error| CliFailure::Other(format!("manifest is not UTF-8: {error}")))?;
    let manifest = manifest_from_yaml(yaml)
        .map_err(|error| CliFailure::Other(format!("manifest is invalid: {error}")))?;
    let digest = manifest_sha256(&bytes);
    let step_count = u32::try_from(manifest.steps.len())
        .map_err(|_| CliFailure::Other("manifest has too many steps".into()))?;
    let mut checkpoint = load_checkpoint(&state_path, &digest, step_count)?;
    let total = manifest.steps.len();

    let result = run_remaining(
        &manifest,
        &mut checkpoint,
        |step| {
            eprintln!("Step {}/{}: {}", step.number, total, step.why);
            let status = Command::new(&step.program)
                .args(&step.argv)
                .status()
                .map_err(|error| error.to_string())?;
            Ok(status.code().unwrap_or(128))
        },
        |checkpoint| {
            let yaml = checkpoint_to_yaml(checkpoint).map_err(|error| error.to_string())?;
            atomic_write(&state_path, yaml.as_bytes()).map_err(|error| error.to_string())
        },
    );

    match result {
        Ok(summary) => {
            eprintln!(
                "Complete: skipped {} durable step(s), completed {} step(s).",
                summary.skipped, summary.completed
            );
            Ok(())
        }
        Err(RunFailure::StepFailed { step, code }) => Err(CliFailure::Step {
            message: format!("step {step} failed; later steps were not run"),
            code,
        }),
        Err(other) => Err(CliFailure::Other(other.to_string())),
    }
}

fn parse_args() -> Result<(PathBuf, PathBuf), CliFailure> {
    let mut args = std::env::args_os().skip(1);
    let Some(manifest) = args.next() else {
        return Err(usage());
    };
    if manifest == "-h" || manifest == "--help" {
        println!("{}", usage_text());
        process::exit(0);
    }
    let manifest = PathBuf::from(manifest);
    let state = match args.next() {
        None => default_state_path(&manifest),
        Some(flag) if flag == "--state" => {
            let path = args.next().ok_or_else(usage)?;
            if args.next().is_some() {
                return Err(usage());
            }
            PathBuf::from(path)
        }
        Some(_) => return Err(usage()),
    };
    Ok((manifest, state))
}

fn usage() -> CliFailure {
    CliFailure::Other(usage_text().to_string())
}

fn usage_text() -> &'static str {
    "usage: gv-run <manifest.yaml> [--state <checkpoint.yaml>]\n\
     Executes exact argv in order, stops on the first error, and resumes from\n\
     the checkpoint. Default state: <manifest.yaml>.gv-state.yaml"
}

fn default_state_path(manifest: &Path) -> PathBuf {
    let mut value: OsString = manifest.as_os_str().to_owned();
    value.push(".gv-state.yaml");
    PathBuf::from(value)
}

fn read_manifest(path: &Path) -> Result<Vec<u8>, CliFailure> {
    let file = File::open(path).map_err(|error| {
        CliFailure::Other(format!("cannot open manifest {}: {error}", path.display()))
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CliFailure::Other(format!("cannot read manifest {}: {error}", path.display()))
        })?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(CliFailure::Other(format!(
            "manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit"
        )));
    }
    Ok(bytes)
}

fn load_checkpoint(path: &Path, digest: &str, step_count: u32) -> Result<Checkpoint, CliFailure> {
    match fs::read_to_string(path) {
        Ok(yaml) => checkpoint_from_yaml(&yaml, digest, step_count)
            .map_err(|error| CliFailure::Other(format!("checkpoint is invalid: {error}"))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(Checkpoint::new(digest.to_string()))
        }
        Err(error) => Err(CliFailure::Other(format!(
            "cannot read checkpoint {}: {error}",
            path.display()
        ))),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let mut last_collision = None;
    for _ in 0..100 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".gv-run-state-{}-{sequence}.tmp", process::id()));
        match create_private(&temp) {
            Ok(mut file) => {
                let result = (|| {
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    fs::rename(&temp, path)?;
                    File::open(parent)?.sync_all()?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&temp);
                }
                return result;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_collision.unwrap_or_else(|| io::Error::other("cannot allocate checkpoint temp file")))
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn normalize_exit_code(code: i32) -> i32 {
    if (1..=255).contains(&code) {
        code
    } else {
        1
    }
}
