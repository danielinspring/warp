//! Image attachment processing for local vision models: the same resize and size cap the Warp
//! client applies to user-attached images before they reach a model.

use image::{GenericImageView, ImageError};

/// Providers accept 5 MB per image; base64 inflates by a third, so raw bytes stop at 3.75 MB.
pub const MAX_IMAGE_SIZE_BYTES: usize = 3750 * 1000;

/// 1.15 megapixels.
pub const MAX_IMAGE_PIXELS: f64 = 1150. * 1000.;

/// Maximum width or height of a resized image.
pub const MAX_IMAGE_DIMENSION: f64 = 2000.;

/// Maximum number of images forwarded with one user query.
pub const MAX_IMAGE_COUNT_FOR_QUERY: usize = 20;

/// Resize an image that exceeds [`MAX_IMAGE_PIXELS`], keeping its format and honouring
/// [`MAX_IMAGE_DIMENSION`]. Images already within the limit are returned unchanged.
pub fn resize_image(image: &[u8]) -> Result<Vec<u8>, ImageError> {
    let img = image::load_from_memory(image)?;

    let (current_width, current_height) = img.dimensions();
    let current_pixels = (current_width * current_height) as f64;

    if current_pixels <= MAX_IMAGE_PIXELS {
        return Ok(image.to_vec());
    }

    let original_format = image::guess_format(image)?;

    let scale = (MAX_IMAGE_PIXELS / current_pixels).sqrt();

    let mut new_width = current_width as f64 * scale;
    let mut new_height = current_height as f64 * scale;

    let scale_by_width = MAX_IMAGE_DIMENSION / new_width;
    let scale_by_height = MAX_IMAGE_DIMENSION / new_height;
    let scale = scale_by_width.min(scale_by_height).min(1.0);

    new_width *= scale;
    new_height *= scale;

    let resized_img = img.thumbnail(new_width.round() as u32, new_height.round() as u32);

    let mut output_bytes: Vec<u8> = Vec::new();
    let mut writer = std::io::Cursor::new(&mut output_bytes);

    resized_img.write_to(&mut writer, original_format)?;

    Ok(output_bytes)
}

/// Result of preparing an image attachment for the model.
#[derive(Debug)]
pub enum ProcessImageResult {
    /// Image is within the size limit (resized if needed).
    Success { data: Vec<u8> },
    /// Image is too large even after resizing.
    TooLarge,
    /// Image could not be decoded or re-encoded.
    Error(ImageError),
}

/// Resize an attachment if needed and enforce [`MAX_IMAGE_SIZE_BYTES`].
pub fn process_image_for_agent(image_data: &[u8]) -> ProcessImageResult {
    match resize_image(image_data) {
        Ok(resized_bytes) => {
            if resized_bytes.len() > MAX_IMAGE_SIZE_BYTES {
                ProcessImageResult::TooLarge
            } else {
                ProcessImageResult::Success {
                    data: resized_bytes,
                }
            }
        }
        Err(err) => ProcessImageResult::Error(err),
    }
}
