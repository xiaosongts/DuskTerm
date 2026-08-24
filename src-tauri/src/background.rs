use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

const MAX_EDGE: u32 = 7680;
const MAX_SOURCE_PIXELS: u64 = 40_000_000;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
const THUMBNAIL_WIDTH: u32 = 480;
const THUMBNAIL_HEIGHT: u32 = 270;
const JPEG_QUALITY: u8 = 88;
const THUMBNAIL_QUALITY: u8 = 78;
const STALE_TEMPORARY_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const ORPHAN_RESOURCE_MIN_AGE: Duration = Duration::from_secs(5 * 60);

fn default_media_type() -> String {
    "image".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BackgroundAsset {
    pub resource_id: String,
    pub file_name: String,
    #[serde(default = "default_media_type")]
    pub media_type: String,
    pub original_path: String,
    pub optimized_path: String,
    #[serde(default)]
    pub thumbnail_path: String,
    pub width: u32,
    pub height: u32,
}

fn root_dir() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .map(|dir| dir.join("duskterm").join("backgrounds"))
        .ok_or_else(|| "Failed to locate application data directory".to_string())
}

fn validate_resource_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err("Invalid background resource id".to_string());
    }
    Ok(())
}

fn is_older_than(path: &Path, now: SystemTime, minimum_age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= minimum_age)
}

fn remove_path(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|error| error.to_string())
    } else {
        fs::remove_file(path).map_err(|error| error.to_string())
    }
}

fn cleanup_stale_temporary_entries(root: &Path) {
    let now = SystemTime::now();
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp-") {
            if is_older_than(&path, now, STALE_TEMPORARY_AGE) {
                let _ = remove_path(&path);
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let Ok(cache_entries) = fs::read_dir(&path) else {
            continue;
        };
        for cache_entry in cache_entries.filter_map(Result::ok) {
            let cache_path = cache_entry.path();
            if cache_entry
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-")
                && is_older_than(&cache_path, now, STALE_TEMPORARY_AGE)
            {
                let _ = remove_path(&cache_path);
            }
        }
    }
}

fn validate_source(path: &Path) -> Result<(u32, u32), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("Background image does not exist".to_string());
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err("Background image file cannot exceed 128 MB".to_string());
    }
    let dimensions = ImageReader::open(path)
        .map_err(|error| format!("Failed to open background image: {error}"))?
        .with_guessed_format()
        .map_err(|error| format!("Failed to detect background image format: {error}"))?
        .into_dimensions()
        .map_err(|error| format!("Failed to read background image dimensions: {error}"))?;
    let pixels = dimensions.0 as u64 * dimensions.1 as u64;
    if dimensions.0 == 0 || dimensions.1 == 0 || pixels > MAX_SOURCE_PIXELS {
        return Err("Background image pixel count is too large".to_string());
    }
    Ok(dimensions)
}

fn validate_video_source(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("Background video does not exist".to_string());
    }
    if metadata.len() > MAX_VIDEO_BYTES {
        return Err("Background video file cannot exceed 256 MB".to_string());
    }
    Ok(())
}

fn detected_image_format(path: &Path) -> Result<ImageFormat, String> {
    ImageReader::open(path)
        .map_err(|error| format!("Failed to open background image: {error}"))?
        .with_guessed_format()
        .map_err(|error| format!("Failed to detect background image format: {error}"))?
        .format()
        .ok_or_else(|| "Unsupported background image format".to_string())
}

fn source_has_alpha(path: &Path) -> Result<bool, String> {
    let decoder = ImageReader::open(path)
        .map_err(|error| format!("Failed to open background image: {error}"))?
        .with_guessed_format()
        .map_err(|error| format!("Failed to detect background image format: {error}"))?
        .into_decoder()
        .map_err(|error| format!("Failed to inspect background image: {error}"))?;
    Ok(decoder.color_type().has_alpha())
}

fn chunk_is_present(path: &Path, format: ImageFormat) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    match format {
        ImageFormat::Png => {
            let mut signature = [0u8; 8];
            file.read_exact(&mut signature)
                .map_err(|error| error.to_string())?;
            if signature != [137, 80, 78, 71, 13, 10, 26, 10] {
                return Ok(false);
            }
            loop {
                let mut header = [0u8; 8];
                if file.read_exact(&mut header).is_err() {
                    return Ok(false);
                }
                let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
                let kind = &header[4..8];
                if kind == b"acTL" {
                    return Ok(true);
                }
                if kind == b"IDAT" || kind == b"IEND" {
                    return Ok(false);
                }
                let skip = i64::try_from(length.saturating_add(4))
                    .map_err(|_| "Invalid PNG chunk length".to_string())?;
                file.seek(SeekFrom::Current(skip))
                    .map_err(|error| error.to_string())?;
            }
        }
        ImageFormat::WebP => {
            let mut signature = [0u8; 12];
            file.read_exact(&mut signature)
                .map_err(|error| error.to_string())?;
            if &signature[..4] != b"RIFF" || &signature[8..] != b"WEBP" {
                return Ok(false);
            }
            loop {
                let mut header = [0u8; 8];
                if file.read_exact(&mut header).is_err() {
                    return Ok(false);
                }
                let length = u32::from_le_bytes(header[4..].try_into().unwrap()) as u64;
                if &header[..4] == b"ANIM" || &header[..4] == b"ANMF" {
                    return Ok(true);
                }
                let skip = i64::try_from(length.saturating_add(length % 2))
                    .map_err(|_| "Invalid WebP chunk length".to_string())?;
                file.seek(SeekFrom::Current(skip))
                    .map_err(|error| error.to_string())?;
            }
        }
        _ => Ok(false),
    }
}

fn reject_animated_image(path: &Path) -> Result<(), String> {
    if chunk_is_present(path, detected_image_format(path)?)? {
        return Err("Animated images are not supported; use MP4 or WebM video instead".to_string());
    }
    Ok(())
}

fn decode_source(path: &Path) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::open(path)
        .map_err(|error| format!("Failed to open background image: {error}"))?
        .with_guessed_format()
        .map_err(|error| format!("Failed to detect background image format: {error}"))?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|error| format!("Failed to decode background image: {error}"))
}

fn target_dimensions(width: u32, height: u32, target_width: u32, target_height: u32) -> (u32, u32) {
    let limit_width = target_width.clamp(1, MAX_EDGE);
    let limit_height = target_height.clamp(1, MAX_EDGE);
    let scale = (limit_width as f64 / width as f64)
        .max(limit_height as f64 / height as f64)
        .min(MAX_EDGE as f64 / width as f64)
        .min(MAX_EDGE as f64 / height as f64)
        .min(1.0);
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

fn should_reuse_original(width: u32, height: u32, target_width: u32, target_height: u32) -> bool {
    let limit_width = target_width.clamp(1, MAX_EDGE);
    let limit_height = target_height.clamp(1, MAX_EDGE);
    width <= limit_width && height <= limit_height
}

fn write_jpeg_atomic(image: &image::RgbImage, target: &Path, quality: u8) -> Result<(), String> {
    let temporary = target.with_file_name(format!(
        ".tmp-{}-{}",
        Uuid::new_v4(),
        target
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("background.jpg")
    ));
    let result = (|| {
        let mut file = File::create(&temporary)
            .map_err(|error| format!("Failed to create background cache: {error}"))?;
        let mut encoder = JpegEncoder::new_with_quality(&mut file, quality);
        encoder
            .encode_image(image)
            .map_err(|error| format!("Failed to encode background cache: {error}"))?;
        drop(encoder);
        drop(file);
        match fs::rename(&temporary, target) {
            Ok(()) => Ok(()),
            Err(_) if target.is_file() => Ok(()),
            Err(error) => Err(format!("Failed to install background cache: {error}")),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn write_png_atomic(image: &image::RgbaImage, target: &Path) -> Result<(), String> {
    let temporary = target.with_file_name(format!(
        ".tmp-{}-{}",
        Uuid::new_v4(),
        target
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("background.png")
    ));
    let result = (|| {
        let file = File::create(&temporary)
            .map_err(|error| format!("Failed to create background cache: {error}"))?;
        PngEncoder::new(file)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| format!("Failed to encode background cache: {error}"))?;
        match fs::rename(&temporary, target) {
            Ok(()) => Ok(()),
            Err(_) if target.is_file() => Ok(()),
            Err(error) => Err(format!("Failed to install background cache: {error}")),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn encode_thumbnail(source: &Path, target: &Path) -> Result<(), String> {
    let thumbnail = decode_source(source)?
        .thumbnail(THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT)
        .into_rgb8();
    write_jpeg_atomic(&thumbnail, target, THUMBNAIL_QUALITY)
}

fn encode_cache(
    source: &Path,
    target: &Path,
    thumbnail_target: &Path,
    source_dimensions: (u32, u32),
    preserve_alpha: bool,
    target_width: u32,
    target_height: u32,
) -> Result<(u32, u32), String> {
    let (source_width, source_height) = source_dimensions;
    let image = decode_source(source)?;
    let (width, height) =
        target_dimensions(source_width, source_height, target_width, target_height);
    let output = if width != source_width || height != source_height {
        image.resize_exact(width, height, FilterType::Triangle)
    } else {
        image
    };
    let thumbnail = output
        .thumbnail(THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT)
        .into_rgb8();
    write_jpeg_atomic(&thumbnail, thumbnail_target, THUMBNAIL_QUALITY)?;
    if preserve_alpha {
        let rgba = output.into_rgba8();
        write_png_atomic(&rgba, target)?;
    } else {
        let rgb = output.into_rgb8();
        write_jpeg_atomic(&rgb, target, JPEG_QUALITY)?;
    }
    Ok((width, height))
}

fn original_in(directory: &Path) -> Result<PathBuf, String> {
    fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file() && path.file_stem().and_then(|value| value.to_str()) == Some("original")
        })
        .ok_or_else(|| "Background original image does not exist".to_string())
}

fn asset_from_directory(
    resource_id: &str,
    directory: &Path,
    target_width: u32,
    target_height: u32,
) -> Result<BackgroundAsset, String> {
    let original = original_in(directory)?;
    let metadata = fs::read(directory.join("metadata.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BackgroundAsset>(&bytes).ok());
    let inferred_media_type = match original
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "webm" => "video",
        _ => "image",
    };
    let media_type = metadata
        .as_ref()
        .map(|asset| asset.media_type.as_str())
        .unwrap_or(inferred_media_type)
        .to_string();
    let file_name = metadata
        .as_ref()
        .map(|asset| asset.file_name.clone())
        .unwrap_or_else(|| {
            original
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("background")
                .to_string()
        });

    if media_type == "video" {
        validate_video_source(&original)?;
        let original_path = original.to_string_lossy().into_owned();
        return Ok(BackgroundAsset {
            resource_id: resource_id.to_string(),
            file_name,
            media_type,
            original_path: original_path.clone(),
            optimized_path: original_path,
            thumbnail_path: String::new(),
            width: metadata.as_ref().map(|asset| asset.width).unwrap_or(0),
            height: metadata.as_ref().map(|asset| asset.height).unwrap_or(0),
        });
    }

    let source_dimensions = validate_source(&original)?;
    reject_animated_image(&original)?;
    let preserve_alpha = source_has_alpha(&original)?;
    let use_original = should_reuse_original(
        source_dimensions.0,
        source_dimensions.1,
        target_width,
        target_height,
    );
    let optimized = if use_original {
        original.clone()
    } else {
        directory.join(if preserve_alpha {
            "optimized.png"
        } else {
            "optimized.jpg"
        })
    };
    let thumbnail = directory.join("thumbnail.jpg");
    let desired = target_dimensions(
        source_dimensions.0,
        source_dimensions.1,
        target_width,
        target_height,
    );
    if !use_original && optimized.is_file() {
        match image::image_dimensions(&optimized) {
            Ok((width, height)) if width >= desired.0 && height >= desired.1 => {}
            _ => {
                let _ = fs::remove_file(&optimized);
            }
        }
    }
    let (width, height) = if use_original {
        if !thumbnail.is_file() {
            encode_thumbnail(&original, &thumbnail)?;
        }
        source_dimensions
    } else if optimized.is_file() {
        let dimensions = image::image_dimensions(&optimized)
            .map_err(|error| format!("Background cache is corrupted: {error}"))?;
        if !thumbnail.is_file() {
            encode_thumbnail(&optimized, &thumbnail)?;
        }
        dimensions
    } else {
        encode_cache(
            &original,
            &optimized,
            &thumbnail,
            source_dimensions,
            preserve_alpha,
            target_width,
            target_height,
        )?
    };
    Ok(BackgroundAsset {
        resource_id: resource_id.to_string(),
        file_name,
        media_type,
        original_path: original.to_string_lossy().into_owned(),
        optimized_path: optimized.to_string_lossy().into_owned(),
        thumbnail_path: thumbnail.to_string_lossy().into_owned(),
        width,
        height,
    })
}

#[tauri::command]
pub async fn import_background_image(
    source_path: String,
    target_width: u32,
    target_height: u32,
) -> Result<BackgroundAsset, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let source = PathBuf::from(&source_path);
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let media_type = match extension.as_str() {
            "png" | "jpg" | "jpeg" | "webp" => "image",
            "mp4" | "webm" => "video",
            _ => {
                return Err(
                    "Only PNG, JPG, JPEG, WebP, MP4 and WebM backgrounds are supported".to_string(),
                )
            }
        };
        let (source_dimensions, preserve_alpha) = if media_type == "image" {
            let dimensions = validate_source(&source)?;
            reject_animated_image(&source)?;
            (dimensions, source_has_alpha(&source)?)
        } else {
            validate_video_source(&source)?;
            ((0, 0), false)
        };
        let resource_id = Uuid::new_v4().to_string();
        let root = root_dir()?;
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        cleanup_stale_temporary_entries(&root);
        let temporary = root.join(format!(".tmp-{resource_id}"));
        let destination = root.join(&resource_id);
        let result = (|| {
            fs::create_dir_all(&temporary).map_err(|error| error.to_string())?;
            let original = temporary.join(format!("original.{extension}"));
            fs::copy(&source, &original).map_err(|error| error.to_string())?;
            let final_original = destination.join(format!("original.{extension}"));
            let final_thumbnail = destination.join("thumbnail.jpg");
            let (width, height, optimized_path, thumbnail_path) = if media_type == "video" {
                (
                    0,
                    0,
                    final_original.to_string_lossy().into_owned(),
                    String::new(),
                )
            } else {
                let thumbnail = temporary.join("thumbnail.jpg");
                let use_original = should_reuse_original(
                    source_dimensions.0,
                    source_dimensions.1,
                    target_width,
                    target_height,
                );
                if use_original {
                    encode_thumbnail(&original, &thumbnail)?;
                    (
                        source_dimensions.0,
                        source_dimensions.1,
                        final_original.to_string_lossy().into_owned(),
                        final_thumbnail.to_string_lossy().into_owned(),
                    )
                } else {
                    let optimized_file_name = if preserve_alpha {
                        "optimized.png"
                    } else {
                        "optimized.jpg"
                    };
                    let optimized = temporary.join(optimized_file_name);
                    let (width, height) = encode_cache(
                        &original,
                        &optimized,
                        &thumbnail,
                        source_dimensions,
                        preserve_alpha,
                        target_width,
                        target_height,
                    )?;
                    (
                        width,
                        height,
                        destination
                            .join(optimized_file_name)
                            .to_string_lossy()
                            .into_owned(),
                        final_thumbnail.to_string_lossy().into_owned(),
                    )
                }
            };
            let asset = BackgroundAsset {
                resource_id: resource_id.clone(),
                file_name: source
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("background")
                    .to_string(),
                media_type: media_type.to_string(),
                original_path: final_original.to_string_lossy().into_owned(),
                optimized_path,
                thumbnail_path,
                width,
                height,
            };
            fs::write(
                temporary.join("metadata.json"),
                serde_json::to_vec_pretty(&asset).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            fs::rename(&temporary, &destination).map_err(|error| error.to_string())?;
            Ok(asset)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&temporary);
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn ensure_background_image(
    resource_id: String,
    target_width: u32,
    target_height: u32,
) -> Result<BackgroundAsset, String> {
    tauri::async_runtime::spawn_blocking(move || {
        validate_resource_id(&resource_id)?;
        let root = root_dir()?;
        cleanup_stale_temporary_entries(&root);
        asset_from_directory(
            &resource_id,
            &root.join(&resource_id),
            target_width,
            target_height,
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn delete_background_image(resource_id: String) -> Result<(), String> {
    validate_resource_id(&resource_id)?;
    let directory = root_dir()?.join(resource_id);
    if directory.exists() {
        fs::remove_dir_all(directory).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn cleanup_background_resources(retained_resource_ids: Vec<String>) -> Result<(), String> {
    for resource_id in &retained_resource_ids {
        validate_resource_id(resource_id)?;
    }
    let retained: HashSet<_> = retained_resource_ids.into_iter().collect();
    let root = root_dir()?;
    if !root.is_dir() {
        return Ok(());
    }
    cleanup_stale_temporary_entries(&root);
    let now = SystemTime::now();
    let mut failures = Vec::new();
    for entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures.push(error.to_string());
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir()
            || name.starts_with(".tmp-")
            || validate_resource_id(&name).is_err()
            || retained.contains(&name)
            || !path.join("metadata.json").is_file()
            || !is_older_than(&path, now, ORPHAN_RESOURCE_MIN_AGE)
        {
            continue;
        }
        if let Err(error) = fs::remove_dir_all(&path) {
            failures.push(format!("{name}: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Failed to clean {} background resource(s): {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        chunk_is_present, should_reuse_original, target_dimensions, validate_source,
        BackgroundAsset,
    };
    use image::{DynamicImage, ImageFormat};
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn does_not_upscale_small_images() {
        assert_eq!(target_dimensions(1200, 800, 3840, 2160), (1200, 800));
    }

    #[test]
    fn covers_target_without_losing_aspect_ratio() {
        assert_eq!(target_dimensions(8000, 4000, 3840, 2160), (4320, 2160));
    }

    #[test]
    fn reuses_reasonable_sized_images_for_fast_import() {
        assert!(should_reuse_original(3200, 1800, 3840, 2160));
    }

    #[test]
    fn does_not_reuse_oversized_images() {
        assert!(!should_reuse_original(8000, 4000, 3840, 2160));
    }

    #[test]
    fn detects_dimensions_from_content_when_extension_is_inaccurate() {
        let path =
            std::env::temp_dir().join(format!("duskterm-background-format-{}.jpg", Uuid::new_v4()));
        let mut file = fs::File::create(&path).expect("create temporary image");
        DynamicImage::new_rgba8(7, 5)
            .write_to(&mut file, ImageFormat::Png)
            .expect("write PNG content");

        assert_eq!(validate_source(&path).expect("read dimensions"), (7, 5));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn detects_apng_animation_chunk() {
        let path =
            std::env::temp_dir().join(format!("duskterm-background-apng-{}.png", Uuid::new_v4()));
        let mut bytes = vec![137, 80, 78, 71, 13, 10, 26, 10];
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"acTL");
        fs::write(&path, bytes).expect("write temporary APNG header");

        assert!(chunk_is_present(&path, ImageFormat::Png).expect("inspect APNG chunks"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn detects_animated_webp_chunk() {
        let path = std::env::temp_dir().join(format!(
            "duskterm-background-animated-{}.webp",
            Uuid::new_v4()
        ));
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        bytes.extend_from_slice(b"ANIM");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        fs::write(&path, bytes).expect("write temporary animated WebP header");

        assert!(chunk_is_present(&path, ImageFormat::WebP).expect("inspect WebP chunks"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reads_legacy_metadata_with_image_defaults() {
        let asset: BackgroundAsset = serde_json::from_str(
            r#"{
                "resource_id": "legacy",
                "file_name": "background.png",
                "original_path": "original.png",
                "optimized_path": "optimized.jpg",
                "width": 1920,
                "height": 1080
            }"#,
        )
        .expect("deserialize legacy metadata");

        assert_eq!(asset.media_type, "image");
        assert!(asset.thumbnail_path.is_empty());
    }
}
