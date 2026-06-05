//! Attachment sub-resources for runs and units.

use std::path::Path;
use reqwest::header::CONTENT_TYPE;
use crate::runs::RunsClient;
use crate::units::UnitsClient;
use crate::error::{Error, Result};

fn content_type_for(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        _ => "application/octet-stream",
    }
}

async fn upload_to_presigned_url(http: &reqwest::Client, url: &str, bytes: Vec<u8>, mime: &str) -> Result<()> {
    let resp = http
        .put(url)
        .header(CONTENT_TYPE, mime)
        .body(bytes)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::UnexpectedStatus { status, body: format!("upload failed: {body}") });
    }
    Ok(())
}

async fn download_to_file(http: &reqwest::Client, url: &str, dest: impl AsRef<Path>) -> Result<()> {
    let resp = http.get(url).send().await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::UnexpectedStatus { status, body: format!("download failed: {body}") });
    }
    let bytes = resp.bytes().await?;
    tokio::fs::write(dest, bytes).await?;
    Ok(())
}

/// Sub-resource for run attachments: `client.runs().attachments().create()` / `.download()`
pub struct RunAttachments<'a> {
    pub(crate) client: &'a RunsClient<'a>,
}

impl<'a> RunAttachments<'a> {
    /// Upload a file and attach it to a run. Returns the attachment ID.
    pub async fn upload(&self, run_id: &str, path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let name = path.file_name().and_then(|n| n.to_str())
            .ok_or_else(|| Error::Validation("could not extract file name".to_string()))?;
        let bytes = tokio::fs::read(path).await?;
        let mime = content_type_for(name);

        let result = self.client.create_attachment().id(run_id).name(name).send().await?;
        upload_to_presigned_url(&self.client.client.http_external, &result.upload_url, bytes, mime).await?;
        Ok(result.id)
    }

    /// Download an attachment to a local file.
    pub async fn download(&self, url: &str, dest: impl AsRef<Path>) -> Result<()> {
        if url.is_empty() {
            return Err(Error::Validation("download URL cannot be empty".to_string()));
        }
        download_to_file(&self.client.client.http_external, url, dest).await
    }
}

/// Sub-resource for unit attachments: `client.units().attachments().create()` / `.download()` / `.delete()`
pub struct UnitAttachments<'a> {
    pub(crate) client: &'a UnitsClient<'a>,
}

impl<'a> UnitAttachments<'a> {
    /// Upload a file and attach it to a unit. Returns the attachment ID.
    pub async fn upload(&self, serial_number: &str, path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let name = path.file_name().and_then(|n| n.to_str())
            .ok_or_else(|| Error::Validation("could not extract file name".to_string()))?;
        let bytes = tokio::fs::read(path).await?;
        let mime = content_type_for(name);

        let result = self.client.create_attachment().serial_number(serial_number).name(name).send().await?;
        upload_to_presigned_url(&self.client.client.http_external, &result.upload_url, bytes, mime).await?;
        Ok(result.id)
    }

    /// Download an attachment to a local file.
    pub async fn download(&self, url: &str, dest: impl AsRef<Path>) -> Result<()> {
        if url.is_empty() {
            return Err(Error::Validation("download URL cannot be empty".to_string()));
        }
        download_to_file(&self.client.client.http_external, url, dest).await
    }

    /// Delete attachments from a unit by their IDs.
    pub async fn delete(&self, serial_number: &str, ids: Vec<String>) -> Result<crate::types::UnitDeleteAttachmentResponse> {
        self.client.delete_attachment().serial_number(serial_number).ids(ids).send().await
    }
}

impl<'a> RunsClient<'a> {
    /// Access attachment sub-resource.
    pub fn attachments(&self) -> RunAttachments<'_> {
        RunAttachments { client: self }
    }
}

impl<'a> UnitsClient<'a> {
    /// Access attachment sub-resource.
    pub fn attachments(&self) -> UnitAttachments<'_> {
        UnitAttachments { client: self }
    }
}
