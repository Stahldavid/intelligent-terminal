use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

const FOOTER_MAGIC: &[u8; 16] = b"WTA-INSTALLER-V1";
const FOOTER_LEN: u64 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddedFile {
    name: String,
    offset: u64,
    length: u64,
}

fn main() {
    let exit_code = match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("installer bootstrap failed: {err}");
            1
        }
    };

    std::process::exit(exit_code);
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let exe_path = env::current_exe()?;
    let manifest = read_manifest(&exe_path)?;
    let temp_dir = create_temp_dir()?;

    let exit_code = (|| -> Result<i32, Box<dyn std::error::Error>> {
        extract_embedded_files(&exe_path, &temp_dir, &manifest)?;

        for required_file in [
            "install-local-terminal.ps1",
            "ComProxyRegistration.ps1",
            "payload.zip",
        ] {
            if !temp_dir.join(required_file).is_file() {
                return Err(format!("missing {required_file} in {}", temp_dir.display()).into());
            }
        }

        let status = run_installer(&temp_dir, env::args_os().skip(1))?;

        match status.code() {
            Some(code) => Ok(code),
            None => Err("installer terminated without an exit code".into()),
        }
    })();

    let cleanup_result = fs::remove_dir_all(&temp_dir);
    match (exit_code, cleanup_result) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(_), Err(err)) => Err(format!(
            "installation finished, but cleanup failed for {}: {err}",
            temp_dir.display()
        )
        .into()),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(cleanup_err)) => Err(format!(
            "{err}; cleanup also failed for {}: {cleanup_err}",
            temp_dir.display()
        )
        .into()),
    }
}

fn run_installer<I, S>(temp_dir: &Path, forwarded_args: I) -> io::Result<ExitStatus>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Avoid cmd.exe's special /c quote stripping entirely. It can turn a
    // batch file's `%~dp0` into the drive root (for example C:\), making the
    // sibling script and payload appear missing. The bootstrap already owns
    // the trusted extracted paths, so invoke PowerShell with those absolute
    // paths and translate only the three documented switches.
    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(temp_dir.join("install-local-terminal.ps1"))
        .arg("-PayloadZip")
        .arg(temp_dir.join("payload.zip"))
        .current_dir(temp_dir);

    for argument in forwarded_args {
        let argument = argument.as_ref().to_string_lossy();
        if argument.eq_ignore_ascii_case("/quiet") {
            command.arg("-Quiet");
        } else if argument.eq_ignore_ascii_case("/nopath") {
            command.arg("-NoPathUpdate");
        } else if argument.eq_ignore_ascii_case("/noshortcuts") {
            command.arg("-NoShortcuts");
        }
    }

    command.status()
}

fn create_temp_dir() -> io::Result<PathBuf> {
    let mut path = env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!(
        "intelligent-terminal-installer-{}-{}",
        std::process::id(),
        stamp
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn read_manifest(exe_path: &Path) -> Result<Vec<EmbeddedFile>, Box<dyn std::error::Error>> {
    let mut file = File::open(exe_path)?;
    let exe_len = file.metadata()?.len();
    if exe_len < FOOTER_LEN {
        return Err("installer payload footer is missing".into());
    }

    file.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;

    let mut magic = [0u8; 16];
    file.read_exact(&mut magic)?;
    if &magic != FOOTER_MAGIC {
        return Err("installer payload footer magic is invalid".into());
    }

    let mut len_buf = [0u8; 8];
    file.read_exact(&mut len_buf)?;
    let manifest_len = u64::from_le_bytes(len_buf);

    if manifest_len > exe_len.saturating_sub(FOOTER_LEN) {
        return Err("installer payload manifest length is invalid".into());
    }

    let manifest_offset = exe_len - FOOTER_LEN - manifest_len;
    file.seek(SeekFrom::Start(manifest_offset))?;

    let mut manifest_bytes = vec![0u8; manifest_len as usize];
    file.read_exact(&mut manifest_bytes)?;
    let manifest_text = String::from_utf8(manifest_bytes)?;

    let files = parse_manifest(&manifest_text)?;
    if files.is_empty() {
        return Err("installer payload manifest is empty".into());
    }

    for entry in &files {
        let end = entry
            .offset
            .checked_add(entry.length)
            .ok_or("installer payload entry overflowed")?;
        if end > manifest_offset {
            return Err(format!(
                "installer payload entry {} exceeds the embedded payload region",
                entry.name
            )
            .into());
        }
    }

    Ok(files)
}

fn parse_manifest(text: &str) -> Result<Vec<EmbeddedFile>, Box<dyn std::error::Error>> {
    let mut files = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.split('|');
        let kind = parts.next().ok_or("manifest entry kind is missing")?;
        if kind != "file" {
            return Err(format!("unsupported manifest entry kind: {kind}").into());
        }

        let name = parts.next().ok_or("manifest file name is missing")?;
        let offset = parts
            .next()
            .ok_or("manifest file offset is missing")?
            .parse::<u64>()?;
        let length = parts
            .next()
            .ok_or("manifest file length is missing")?
            .parse::<u64>()?;

        if parts.next().is_some() {
            return Err(format!("manifest entry has too many fields: {line}").into());
        }

        files.push(EmbeddedFile {
            name: name.to_string(),
            offset,
            length,
        });
    }

    Ok(files)
}

fn extract_embedded_files(
    exe_path: &Path,
    output_dir: &Path,
    files: &[EmbeddedFile],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open(exe_path)?;

    for entry in files {
        let target_path = output_dir.join(&entry.name);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        file.seek(SeekFrom::Start(entry.offset))?;
        let mut output = File::create(&target_path)?;
        let mut limited = std::io::Read::by_ref(&mut file).take(entry.length);
        io::copy(&mut limited, &mut output)?;
        output.flush()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{create_temp_dir, parse_manifest, run_installer, EmbeddedFile};
    use std::fs;

    #[test]
    fn parse_manifest_reads_multiple_entries() {
        let text = "file|install.cmd|10|20\nfile|payload.zip|30|40\n";
        let parsed = parse_manifest(text).expect("manifest should parse");

        assert_eq!(
            parsed,
            vec![
                EmbeddedFile {
                    name: "install.cmd".into(),
                    offset: 10,
                    length: 20,
                },
                EmbeddedFile {
                    name: "payload.zip".into(),
                    offset: 30,
                    length: 40,
                }
            ]
        );
    }

    #[test]
    fn parse_manifest_rejects_unknown_entry_kind() {
        let err = parse_manifest("dir|payload|0|10\n").expect_err("manifest should fail");
        assert!(err.to_string().contains("unsupported manifest entry kind"));
    }

    #[cfg(windows)]
    #[test]
    fn installer_uses_absolute_extracted_paths_and_forwards_documented_arguments() {
        let temp_dir = create_temp_dir().expect("temporary directory should be created");
        let script_path = temp_dir.join("install-local-terminal.ps1");
        fs::write(
            &script_path,
            "param([string]$PayloadZip,[switch]$Quiet,[switch]$NoPathUpdate,[switch]$NoShortcuts)\n@(\"PAYLOAD=$PayloadZip\", \"QUIET=$Quiet\", \"NOPATH=$NoPathUpdate\", \"NOSHORTCUTS=$NoShortcuts\") | Set-Content -Path invocation.txt\n",
        )
        .expect("test installer script should be written");
        fs::write(temp_dir.join("payload.zip"), b"test payload")
            .expect("test payload should be written");

        let status = run_installer(&temp_dir, ["/quiet", "/nopath", "/noshortcuts"])
            .expect("test installer should execute");
        assert!(status.success());

        let invocation = fs::read_to_string(temp_dir.join("invocation.txt"))
            .expect("test installer should record its invocation");
        let expected_payload = format!("PAYLOAD={}\\payload.zip", temp_dir.display());
        assert!(
            invocation.lines().any(|line| line == expected_payload),
            "absolute payload path was not preserved: {invocation:?}"
        );
        for expected in ["QUIET=True", "NOPATH=True", "NOSHORTCUTS=True"] {
            assert!(
                invocation.lines().any(|line| line == expected),
                "documented argument was not preserved: {invocation:?}"
            );
        }

        fs::remove_dir_all(temp_dir).expect("temporary directory should be removed");
    }
}
