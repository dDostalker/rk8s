use std::{fs, path::Path};

use anyhow::{Result, bail};
use thiserror::Error;

use crate::bundle;

pub enum ImageType {
    Bundle,
    OCIImage,
}

#[derive(Error, Debug)]
pub enum UtilsError {
    #[error("invalid image path")]
    InvalidImagePath,
}

pub fn handle_oci_image<P: AsRef<Path>>(image: P, name: String) -> Result<()> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(bundle::convert_image_to_bundle(
            image,
            format!("/var/rkl/bundle/{name}"),
        ))
}

pub fn determine_image_path<P: AsRef<Path>>(target: P) -> Result<ImageType> {
    if !target.as_ref().is_dir() {
        bail!("invalid image path")
    }

    let path = fs::canonicalize(target.as_ref())?;

    // determine if if is bundle
    if path.join("config.json").exists() && path.join("rootfs").is_dir() {
        return Ok(ImageType::Bundle);
    }

    if path.join("index.json").exists()
        && path.join("blobs").is_dir()
        && path.join("oci-layout").exists()
    {
        return Ok(ImageType::OCIImage);
    }

    return Err(UtilsError::InvalidImagePath.into());
}
