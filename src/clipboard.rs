use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::tools::{ImageAttachment, VideoAttachment, video_mime};

pub fn read_clipboard_image() -> Result<Option<ImageAttachment>> {
    #[cfg(target_os = "linux")]
    if is_wsl() {
        return read_wsl_clipboard_image();
    }
    read_native_clipboard_image()
}

pub fn read_clipboard_videos() -> Result<Vec<VideoAttachment>> {
    #[cfg(target_os = "linux")]
    if is_wsl() {
        return read_wsl_clipboard_videos();
    }
    read_native_clipboard_videos()
}

pub fn read_clipboard_text() -> Result<Option<String>> {
    #[cfg(target_os = "linux")]
    if is_wsl() {
        return read_wsl_clipboard_text();
    }
    read_native_clipboard_text()
}

#[cfg(not(target_os = "linux"))]
fn read_native_clipboard_image() -> Result<Option<ImageAttachment>> {
    native_clipboard_image()
}

#[cfg(target_os = "linux")]
fn read_native_clipboard_image() -> Result<Option<ImageAttachment>> {
    native_clipboard_image()
}

#[cfg(not(target_os = "linux"))]
fn read_native_clipboard_videos() -> Result<Vec<VideoAttachment>> {
    native_clipboard_videos()
}

#[cfg(target_os = "linux")]
fn read_native_clipboard_videos() -> Result<Vec<VideoAttachment>> {
    native_clipboard_videos()
}

#[cfg(not(target_os = "linux"))]
fn read_native_clipboard_text() -> Result<Option<String>> {
    native_clipboard_text()
}

#[cfg(target_os = "linux")]
fn read_native_clipboard_text() -> Result<Option<String>> {
    native_clipboard_text()
}

fn native_clipboard_image() -> Result<Option<ImageAttachment>> {
    let mut clipboard =
        arboard::Clipboard::new().context("Could not access the system clipboard")?;
    match clipboard.get_image() {
        Ok(image) => ImageAttachment::from_rgba(image.width, image.height, &image.bytes).map(Some),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(error) => Err(error).context("Could not read an image from the system clipboard"),
    }
}

fn native_clipboard_videos() -> Result<Vec<VideoAttachment>> {
    let mut clipboard =
        arboard::Clipboard::new().context("Could not access the system clipboard")?;
    match clipboard.get().file_list() {
        Ok(paths) => read_video_paths(paths),
        Err(arboard::Error::ContentNotAvailable) => Ok(Vec::new()),
        Err(error) => Err(error).context("Could not read files from the system clipboard"),
    }
}

fn native_clipboard_text() -> Result<Option<String>> {
    let mut clipboard =
        arboard::Clipboard::new().context("Could not access the system clipboard")?;
    match clipboard.get_text() {
        Ok(text) => Ok(Some(text)),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(error) => Err(error).context("Could not read text from the system clipboard"),
    }
}

fn read_video_paths(paths: Vec<PathBuf>) -> Result<Vec<VideoAttachment>> {
    paths
        .iter()
        .filter(|path| video_mime(path).is_some())
        .map(|path| VideoAttachment::from_path(path))
        .collect()
}

#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::env::var_os("WSL_INTEROP").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_lowercase().contains("microsoft"))
}

#[cfg(target_os = "linux")]
fn read_wsl_clipboard_image() -> Result<Option<ImageAttachment>> {
    use std::path::Path;
    use std::process::Command;

    const POWERSHELL: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";
    const SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms,System.Drawing
$i = [System.Windows.Forms.Clipboard]::GetImage()
if ($i -eq $null) { exit 3 }
$m = New-Object System.IO.MemoryStream
$i.Save($m, [System.Drawing.Imaging.ImageFormat]::Png)
$b = $m.ToArray()
$o = [Console]::OpenStandardOutput()
$o.Write($b, 0, $b.Length)
$o.Flush()
$i.Dispose()
$m.Dispose()
"#;

    let executable = if Path::new(POWERSHELL).is_file() {
        POWERSHELL
    } else {
        "powershell.exe"
    };
    let output = Command::new(executable)
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", SCRIPT])
        .output()
        .context("Could not run Windows PowerShell to read the WSL clipboard")?;
    if output.status.code() == Some(3) {
        return Ok(None);
    }
    if !output.status.success() {
        bail!(
            "Windows clipboard read failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    ImageAttachment::from_png_bytes(output.stdout)
        .context("Windows clipboard did not return a valid PNG image")
        .map(Some)
}

#[cfg(target_os = "linux")]
fn read_wsl_clipboard_videos() -> Result<Vec<VideoAttachment>> {
    use std::process::Command;

    const POWERSHELL: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";
    const SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$files = [System.Windows.Forms.Clipboard]::GetFileDropList()
if ($files.Count -eq 0) { exit 3 }
$paths = @($files | ForEach-Object { $_.ToString() })
[Console]::Out.Write((ConvertTo-Json -InputObject $paths -Compress))
"#;

    let executable = if Path::new(POWERSHELL).is_file() {
        POWERSHELL
    } else {
        "powershell.exe"
    };
    let output = Command::new(executable)
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", SCRIPT])
        .output()
        .context("Could not run Windows PowerShell to read the WSL clipboard")?;
    if output.status.code() == Some(3) {
        return Ok(Vec::new());
    }
    if !output.status.success() {
        bail!(
            "Windows clipboard read failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let paths: Vec<PathBuf> = serde_json::from_slice(&output.stdout)
        .context("Windows clipboard did not return a valid file list")?;
    let mut videos = Vec::new();
    for path in paths.into_iter().filter(|path| video_mime(path).is_some()) {
        let path = windows_path_to_wsl(&path)?;
        videos.push(VideoAttachment::from_path(&path)?);
    }
    Ok(videos)
}

#[cfg(target_os = "linux")]
fn windows_path_to_wsl(path: &Path) -> Result<PathBuf> {
    use std::process::Command;

    let output = Command::new("wslpath")
        .arg("-a")
        .arg(path)
        .output()
        .context("Could not run wslpath for a clipboard video")?;
    if !output.status.success() {
        bail!(
            "Could not convert clipboard video path: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let path = String::from_utf8(output.stdout)
        .context("wslpath returned invalid UTF-8")?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if path.is_empty() {
        bail!("wslpath returned an empty clipboard video path");
    }
    Ok(PathBuf::from(path))
}

#[cfg(target_os = "linux")]
fn read_wsl_clipboard_text() -> Result<Option<String>> {
    use std::path::Path;
    use std::process::Command;

    const POWERSHELL: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";
    const SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$text = Get-Clipboard -Raw -TextFormatType UnicodeText
if ($null -eq $text) { exit 3 }
[Console]::Out.Write($text)
"#;

    let executable = if Path::new(POWERSHELL).is_file() {
        POWERSHELL
    } else {
        "powershell.exe"
    };
    let output = Command::new(executable)
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", SCRIPT])
        .output()
        .context("Could not run Windows PowerShell to read the WSL clipboard")?;
    if output.status.code() == Some(3) {
        return Ok(None);
    }
    if !output.status.success() {
        bail!(
            "Windows clipboard read failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)
        .context("Windows clipboard did not return valid UTF-8 text")
        .map(Some)
}
