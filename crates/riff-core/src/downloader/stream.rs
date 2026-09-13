//! Download ZIPs through a bounded pipe to a synchronous extraction worker.
//!
//! Local headers allow common distribution ZIPs to decompress during download.
//! The completed central directory remains authoritative: mismatched metadata or
//! unsupported streaming layouts use the normal seekable extractor. Nothing is
//! published until the HTTP body, checksum and archive validation have succeeded.

use std::cell::Cell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use tempfile::{NamedTempFile, TempDir};
use tokio::io::AsyncWriteExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::SyncIoBridge;

use crate::http::{HttpClient, HttpRequestOptions};
use crate::{Result, RiffError};

use super::archive::{
    check_cancelled, copy_zip_entry, set_zip_permissions, validate_zip_relative_path,
    ArchiveExtractor, ZIP_COPY_BUFFER_SIZE,
};
use super::checksum::{ChecksumHasher, ChecksumType};

const PIPE_BUFFER_SIZE: usize = 256 * 1024;
// For small completed ZIPs, the in-memory seekable reader avoids a second
// metadata pass. Keep streaming large responses and unknown-length bodies.
const MIN_STREAMING_ZIP_SIZE: u64 = 128 * 1024;

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Own all filesystem state and the extraction slot until publication or cleanup.
struct PreparedZip {
    archive: NamedTempFile,
    directory: TempDir,
    contents: PathBuf,
    _permit: OwnedSemaphorePermit,
}

impl PreparedZip {
    fn publish(self, cache_path: &Path, destination: &Path, cancelled: &AtomicBool) -> Result<()> {
        check_cancelled(Some(cancelled))?;
        self.archive
            .persist(cache_path)
            .map_err(|error| error.error)?;

        // Move the previous installation aside so a failed rename can restore it.
        // Its recursive cleanup happens on this blocking worker as well.
        let previous = self.directory.path().join("previous");
        let replaced = destination.try_exists()?;
        if replaced {
            fs::rename(destination, &previous)?;
        }
        if let Err(error) = fs::rename(&self.contents, destination) {
            if replaced {
                if let Err(restore_error) = fs::rename(&previous, destination) {
                    let retained = self.directory.keep().join("previous");
                    return Err(RiffError::InstallationFailed(format!(
                        "Failed to publish ZIP: {error}; failed to restore the previous installation: {restore_error}; previous files retained at {}",
                        retained.display()
                    )));
                }
            }
            return Err(error.into());
        }
        Ok(())
    }
}

pub(super) async fn download_zip(
    http: &HttpClient,
    url: &str,
    cache_path: &Path,
    destination: &Path,
    checksum: Option<(ChecksumType, &str)>,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    let download_error = |error: String| RiffError::DownloadFailed {
        package: url.to_owned(),
        reason: error,
    };
    let response = http
        .get_with_accept_encoding(url, "identity", &HttpRequestOptions::default())
        .await
        .map_err(|error| download_error(error.to_string()))?;
    let stream_entries = response
        .content_length()
        .is_none_or(|size| size > MIN_STREAMING_ZIP_SIZE);
    let checksum = checksum.map(|(kind, expected)| (kind, expected.to_owned()));
    let cache_path = cache_path.to_path_buf();
    let destination = destination.to_path_buf();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancelled.clone());
    let worker_cancelled = cancelled.clone();
    let (sender, receiver) = tokio::io::duplex(PIPE_BUFFER_SIZE);
    let (authorize, authorized) = tokio::sync::oneshot::channel();
    let receiver = SyncIoBridge::new(receiver);
    let mut worker = tokio::task::spawn_blocking(move || {
        let prepared = prepare_zip(
            receiver,
            &cache_path,
            &destination,
            checksum,
            permit,
            &worker_cancelled,
            stream_entries,
        )?;
        // EOF on the pipe also occurs after HTTP errors or cancellation. Only
        // the producer's successful completion may authorize publication.
        authorized.blocking_recv().map_err(|_| {
            RiffError::InstallationFailed("ZIP download did not complete".to_owned())
        })?;
        prepared.publish(&cache_path, &destination, &worker_cancelled)
    });
    let download = async move {
        // Dropping this future closes the pipe on EOF, HTTP errors or cancellation.
        let mut sender = sender;
        let authorize = authorize;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|error| download_error(error.to_string()))?;
            if let Err(error) = sender.write_all(&chunk).await {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    // Report the worker's error instead of the resulting closed pipe.
                    break;
                }
                return Err(download_error(error.to_string()));
            }
        }
        drop(sender);
        let _ = authorize.send(());
        Ok::<_, RiffError>(())
    };
    tokio::select! {
        biased;
        result = download => {
            result?;
            worker.await
        }
        result = &mut worker => result,
    }
    .map_err(worker_error)?
}

fn worker_error(error: tokio::task::JoinError) -> RiffError {
    RiffError::InstallationFailed(format!("Streaming ZIP task failed: {error}"))
}

fn prepare_zip(
    source: impl Read,
    cache_path: &Path,
    destination: &Path,
    checksum: Option<(ChecksumType, String)>,
    permit: OwnedSemaphorePermit,
    cancelled: &AtomicBool,
    stream_entries: bool,
) -> Result<PreparedZip> {
    check_cancelled(Some(cancelled))?;
    let cache_parent = cache_path.parent().unwrap_or_else(|| Path::new("."));
    let destination_parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(cache_parent)?;
    fs::create_dir_all(destination_parent)?;
    let mut archive = tempfile::Builder::new()
        .prefix(".riff-download-")
        .suffix(".part")
        .tempfile_in(cache_parent)?;
    let directory = tempfile::Builder::new()
        .prefix(".riff-extract-")
        .tempdir_in(destination_parent)?;
    let contents = directory.path().join("contents");
    fs::create_dir(&contents)?;
    let mut hasher = checksum
        .as_ref()
        .map(|(kind, _)| ChecksumHasher::new(*kind));
    let position = Cell::new(0);
    let streamed = {
        let mut writer = BufWriter::with_capacity(ZIP_COPY_BUFFER_SIZE, &mut archive);
        let tee = ArchiveReader {
            source,
            cache: &mut writer,
            hasher: hasher.as_mut(),
            cancelled,
        };
        let mut reader = PositionedReader {
            reader: BufReader::with_capacity(ZIP_COPY_BUFFER_SIZE, tee),
            position: &position,
        };
        let streamed = stream_entries
            .then(|| extract_local_entries(&mut reader, &position, &contents, cancelled));
        // Always retain and hash the complete response, including the central
        // directory and trailing bytes, even when streaming extraction falls back.
        let mut buffer = vec![0; ZIP_COPY_BUFFER_SIZE];
        copy_zip_entry(&mut reader, &mut io::sink(), &mut buffer, Some(cancelled))?;
        writer.flush()?;
        streamed
    };
    if let (Some((_, expected)), Some(hasher)) = (checksum, hasher) {
        if !hasher.finish().eq_ignore_ascii_case(&expected) {
            return Err(RiffError::ChecksumMismatch {
                package: cache_path.display().to_string(),
            });
        }
    }
    check_cancelled(Some(cancelled))?;
    let prefix = match streamed {
        Some(Ok(streamed)) => validate_streamed_entries(&archive, &streamed, &contents, cancelled)?,
        Some(Err(error)) => {
            log::debug!("ZIP requires seekable extraction: {error}");
            StreamedValidation::Fallback
        }
        None => StreamedValidation::Fallback,
    };
    let contents = if let StreamedValidation::Reuse(prefix) = prefix {
        prefix.map_or(contents.clone(), |prefix| contents.join(prefix))
    } else {
        // The completed archive is authoritative. This also preserves formats
        // with data descriptors, prepended data, or a different directory order.
        if stream_entries {
            fs::remove_dir_all(&contents)?;
            fs::create_dir(&contents)?;
        }
        ArchiveExtractor::extract_zip_cancellable(archive.path(), &contents, Some(cancelled))?;
        contents
    };
    Ok(PreparedZip {
        archive,
        directory,
        contents,
        _permit: permit,
    })
}

struct ArchiveReader<'a, R, W> {
    source: R,
    cache: &'a mut W,
    hasher: Option<&'a mut ChecksumHasher>,
    cancelled: &'a AtomicBool,
}

impl<R: Read, W: Write> Read for ArchiveReader<'_, R, W> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        check_cancelled(Some(self.cancelled))?;
        let read = self.source.read(bytes)?;
        self.cache.write_all(&bytes[..read])?;
        if let Some(hasher) = &mut self.hasher {
            hasher.update(&bytes[..read]);
        }
        Ok(read)
    }
}

struct PositionedReader<'a, R> {
    reader: R,
    position: &'a Cell<u64>,
}

impl<R: Read> Read for PositionedReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let read = self.reader.read(bytes)?;
        self.position.set(self.position.get() + read as u64);
        Ok(read)
    }
}

struct StreamedEntry {
    name: String,
    header_start: u64,
    data_start: u64,
    compressed_size: u64,
    size: u64,
    crc32: u32,
    compression: zip::CompressionMethod,
}

struct StreamedEntries {
    entries: Vec<StreamedEntry>,
    central_directory_start: u64,
}

fn extract_local_entries(
    reader: &mut impl Read,
    position: &Cell<u64>,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<StreamedEntries> {
    let mut entries = Vec::new();
    let mut directories = HashSet::from([destination.to_path_buf()]);
    let mut buffer = vec![0; ZIP_COPY_BUFFER_SIZE];
    loop {
        check_cancelled(Some(cancelled))?;
        let header_start = position.get();
        let Some(mut file) = zip::read::read_zipfile_from_stream(reader)
            .map_err(|error| RiffError::InstallationFailed(error.to_string()))?
        else {
            return Ok(StreamedEntries {
                entries,
                central_directory_start: header_start,
            });
        };
        let entry = StreamedEntry {
            name: file.name().to_owned(),
            header_start,
            data_start: position.get(),
            compressed_size: file.compressed_size(),
            size: file.size(),
            crc32: file.crc32(),
            compression: file.compression(),
        };
        let path = file.enclosed_name().ok_or_else(|| {
            RiffError::InstallationFailed(format!(
                "Path traversal detected in archive: {}",
                file.name()
            ))
        })?;
        let output = destination.join(path);
        if file.is_dir() {
            ArchiveExtractor::create_zip_directory(&output, &mut directories)?;
        } else {
            if let Some(parent) = output.parent() {
                ArchiveExtractor::create_zip_directory(parent, &mut directories)?;
            }
            let mut writer = File::create(output)?;
            copy_zip_entry(&mut file, &mut writer, &mut buffer, Some(cancelled))?;
        }
        // Directory payloads must also be consumed before the next local header.
        copy_zip_entry(&mut file, &mut io::sink(), &mut buffer, Some(cancelled))?;
        entries.push(entry);
    }
}

enum StreamedValidation {
    Reuse(Option<String>),
    Fallback,
}

/// Reuse staged files only when all local metadata matches the central directory.
fn validate_streamed_entries(
    archive_file: &NamedTempFile,
    streamed: &StreamedEntries,
    contents: &Path,
    cancelled: &AtomicBool,
) -> Result<StreamedValidation> {
    let mut archive = zip::ZipArchive::new(BufReader::new(archive_file.reopen()?))
        .map_err(|error| RiffError::InstallationFailed(format!("Failed to open zip: {error}")))?;
    ArchiveExtractor::validate_zip_entry_names(&archive)?;
    if archive.len() != streamed.entries.len()
        || archive.central_directory_start() != streamed.central_directory_start
    {
        return Ok(StreamedValidation::Fallback);
    }
    let prefix = ArchiveExtractor::find_zip_common_prefix(&archive);
    for (index, entry) in streamed.entries.iter().enumerate() {
        check_cancelled(Some(cancelled))?;
        let file = archive.by_index_raw(index).map_err(|error| {
            RiffError::InstallationFailed(format!("Failed to read zip entry: {error}"))
        })?;
        if file.name() != entry.name
            || file.header_start() != entry.header_start
            || file.data_start() != Some(entry.data_start)
            || file.compressed_size() != entry.compressed_size
            || file.size() != entry.size
            || file.crc32() != entry.crc32
            || file.compression() != entry.compression
            || file.encrypted()
        {
            return Ok(StreamedValidation::Fallback);
        }
        let path = file.enclosed_name().ok_or_else(|| {
            RiffError::InstallationFailed(format!(
                "Path traversal detected in archive: {}",
                file.name()
            ))
        })?;
        let relative_path = prefix
            .as_ref()
            .and_then(|prefix| path.strip_prefix(prefix).ok())
            .unwrap_or(&path);
        validate_zip_relative_path(relative_path)?;
        if !file.is_dir() {
            set_zip_permissions(&file, &contents.join(path))?;
        }
    }
    Ok(StreamedValidation::Reuse(prefix))
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
