use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageBuffer, ImageFormat, Rgba};
use std::{
    borrow::Cow,
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use wl_clipboard_rs::{
    copy::{
        ClipboardType as CopyClipboardType, MimeType as CopyMimeType, Options as CopyOptions,
        Seat as CopySeat, Source as CopySource, clear as wl_clear,
    },
    paste::{ClipboardType, MimeType, Seat, get_contents},
};

pub fn copy_text_to_clipboard(text: &str) -> Result<()> {
    let bytes = text.as_bytes().to_vec().into_boxed_slice();
    if CopyOptions::new()
        .copy(CopySource::Bytes(bytes), CopyMimeType::Text)
        .is_ok()
    {
        return Ok(());
    }

    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_owned()))
        .context("Failed to copy text to clipboard")
}

pub fn read_text_from_clipboard() -> Result<String> {
    if let Ok((mut pipe, _)) =
        get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text)
        && let Ok(text) = std::io::read_to_string(&mut pipe)
    {
        if text.is_empty() {
            bail!("Clipboard is empty");
        }
        return Ok(text);
    }

    let text = arboard::Clipboard::new()
        .and_then(|mut cb| cb.get_text())
        .context("Failed to read text from clipboard")?;
    if text.is_empty() {
        bail!("Clipboard is empty");
    }
    Ok(text)
}

pub fn read_image_from_clipboard() -> Result<image::DynamicImage> {
    if let Some(bytes) = read_wayland_clipboard_image() {
        return image::load_from_memory(&bytes)
            .context("Failed to decode image from Wayland clipboard");
    }

    if let Some(buffer) = arboard::Clipboard::new()
        .ok()
        .and_then(|mut cb| cb.get_image().ok())
        .and_then(|img| {
            ImageBuffer::<Rgba<u8>, _>::from_raw(
                img.width as u32,
                img.height as u32,
                img.bytes.into_owned(),
            )
        })
    {
        return Ok(image::DynamicImage::ImageRgba8(buffer));
    }

    bail!("No valid image found in clipboard")
}

pub fn clear_clipboard() -> Result<()> {
    if wl_clear(CopyClipboardType::Regular, CopySeat::All).is_ok() {
        return Ok(());
    }

    arboard::Clipboard::new()
        .and_then(|mut cb| cb.clear())
        .context("Failed to clear clipboard")
}

pub fn copy_image_to_clipboard(path: &Path) -> Result<(u32, u32)> {
    let img =
        image::open(path).with_context(|| format!("Cannot open image: {}", path.display()))?;
    let dims = (img.width(), img.height());

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut png_bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)
            .context("Failed to encode image as PNG")?;

        if CopyOptions::new()
            .copy(
                CopySource::Bytes(png_bytes.into_boxed_slice()),
                CopyMimeType::Specific("image/png".into()),
            )
            .is_ok()
        {
            return Ok(dims);
        }
    }

    let rgba = img.to_rgba8();
    let img_data = arboard::ImageData {
        width: dims.0 as usize,
        height: dims.1 as usize,
        bytes: Cow::Owned(rgba.into_raw()),
    };
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_image(img_data))
        .context("Failed to copy image to clipboard")?;
    Ok(dims)
}

fn read_wayland_clipboard_image() -> Option<Vec<u8>> {
    ["image/png", "image/jpeg", "image/webp", "image/bmp"]
        .into_iter()
        .find_map(|mime| {
            let (mut pipe, _) = get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                MimeType::Specific(mime),
            )
            .ok()?;
            let mut buf = Vec::new();
            (pipe.read_to_end(&mut buf).is_ok() && !buf.is_empty()).then_some(buf)
        })
}

#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub path: PathBuf,
    pub name: String,
    pub dimensions: (u32, u32),
    pub size_kb: u64,
}

pub struct ImageStaging {
    _tmp_dir: TempDir,
    img_dir: PathBuf,
}

impl ImageStaging {
    pub fn new() -> Result<Self> {
        let tmp_dir = tempfile::Builder::new().prefix("archon-").tempdir()?;
        let img_dir = tmp_dir.path().join("images");
        fs::create_dir_all(&img_dir)?;
        Ok(Self {
            _tmp_dir: tmp_dir,
            img_dir,
        })
    }

    pub fn list_attachments(&self) -> Vec<ImageAttachment> {
        let mut paths: Vec<PathBuf> = fs::read_dir(&self.img_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "png"))
            .collect();

        paths.sort_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });

        paths
            .into_iter()
            .filter_map(|path| {
                let dimensions = image::image_dimensions(&path).ok()?;
                let size_kb = (path.metadata().ok()?.len() / 1024).max(1);
                let name = path.file_name()?.to_string_lossy().into_owned();
                Some(ImageAttachment {
                    path,
                    name,
                    dimensions,
                    size_kb,
                })
            })
            .collect()
    }

    pub fn encode_bytes(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    pub fn paste_clipboard(&self) -> Result<PathBuf> {
        let out_path = self.gen_path("paste");
        read_image_from_clipboard()?.save_with_format(&out_path, ImageFormat::Png)?;
        Ok(out_path)
    }

    pub fn import_file(&self, src: impl AsRef<Path>) -> Result<PathBuf> {
        let src = src.as_ref();
        let out_path = self.gen_path("file");
        image::open(src)
            .with_context(|| format!("Cannot open image: {}", src.display()))?
            .save_with_format(&out_path, ImageFormat::Png)?;
        Ok(out_path)
    }

    pub fn restore_from_bytes(&self, assets: &[(String, Vec<u8>)]) -> Vec<PathBuf> {
        self.clear();
        assets
            .iter()
            .enumerate()
            .filter_map(|(idx, (raw_name, bytes))| {
                let safe_name = Path::new(raw_name)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map_or_else(|| format!("restored_{idx}.png"), String::from);
                let dest = self.img_dir.join(safe_name);
                fs::write(&dest, bytes).ok().map(|()| dest)
            })
            .collect()
    }

    pub fn clear(&self) {
        for entry in fs::read_dir(&self.img_dir).into_iter().flatten().flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }

    fn gen_path(&self, prefix: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        self.img_dir.join(format!("{prefix}_{ts}.png"))
    }
}
