use anyhow::{Context, Result, bail};

use crate::tools::ImageAttachment;

pub fn read_clipboard_image() -> Result<Option<ImageAttachment>> {
    #[cfg(target_os = "linux")]
    if is_wsl() {
        return read_wsl_clipboard_image();
    }
    read_native_clipboard_image()
}

#[cfg(not(target_os = "linux"))]
fn read_native_clipboard_image() -> Result<Option<ImageAttachment>> {
    native_clipboard_image()
}

#[cfg(target_os = "linux")]
fn read_native_clipboard_image() -> Result<Option<ImageAttachment>> {
    native_clipboard_image()
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
