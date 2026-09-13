use super::*;
use std::io::Cursor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Semaphore};
use tokio::time::{timeout, Duration};

fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in entries {
        archive
            .start_file(
                *name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored)
                    .unix_permissions(0o755),
            )
            .unwrap();
        archive.write_all(contents).unwrap();
    }
    archive.finish().unwrap().into_inner()
}

fn permit() -> OwnedSemaphorePermit {
    Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap()
}

fn prepare(bytes: &[u8], temp: &TempDir) -> Result<PreparedZip> {
    prepare_zip(
        Cursor::new(bytes),
        &temp.path().join("archive.zip"),
        &temp.path().join("installed"),
        None,
        permit(),
        &AtomicBool::new(false),
        true,
    )
}

#[test]
fn local_headers_are_reused_only_after_central_directory_validation() {
    let temp = TempDir::new().unwrap();
    let bytes = zip_bytes(&[("package/src/file.php", b"contents")]);
    let position = Cell::new(0);
    let mut reader = PositionedReader {
        reader: Cursor::new(&bytes),
        position: &position,
    };
    let contents = temp.path().join("contents");
    fs::create_dir(&contents).unwrap();
    let cancelled = AtomicBool::new(false);
    let streamed = extract_local_entries(&mut reader, &position, &contents, &cancelled).unwrap();
    let mut archive = NamedTempFile::new().unwrap();
    archive.write_all(&bytes).unwrap();
    assert!(
        matches!(validate_streamed_entries(&archive, &streamed, &contents, &cancelled).unwrap(),
        StreamedValidation::Reuse(Some(prefix)) if prefix == "package/")
    );
    let prepared = prepare(&bytes, &temp).unwrap();
    prepared
        .publish(
            &temp.path().join("archive.zip"),
            &temp.path().join("installed"),
            &cancelled,
        )
        .unwrap();
    assert_eq!(
        fs::read(temp.path().join("installed/src/file.php")).unwrap(),
        b"contents"
    );
    assert_eq!(fs::read(temp.path().join("archive.zip")).unwrap(), bytes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(temp.path().join("installed/src/file.php"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}

#[test]
fn fallback_uses_central_names_and_checksums() {
    let original = zip_bytes(&[("package/original.php", b"contents")]);
    // Alter only the local name and CRC; the original seekable path follows the
    // authoritative central record and accepts both of these archive layouts.
    for mutate_crc in [false, true] {
        let temp = TempDir::new().unwrap();
        let mut bytes = original.clone();
        if mutate_crc {
            bytes[14] ^= 1;
        } else {
            bytes[30 + "package/".len()] = b'X';
        }
        let prepared = prepare(&bytes, &temp).unwrap();
        assert_eq!(
            fs::read(prepared.contents.join("original.php")).unwrap(),
            b"contents"
        );
        assert!(!prepared.contents.join("Xriginal.php").exists());
    }
}

#[test]
fn streaming_fallback_preserves_data_descriptors_and_empty_archives() {
    let bytes = zip_bytes(&[("package/file.php", b"contents")]);
    let central = zip::ZipArchive::new(Cursor::new(&bytes))
        .unwrap()
        .central_directory_start() as usize;
    let mut descriptor = bytes.clone();
    descriptor[6] |= 8;
    descriptor[central + 8] |= 8;
    // Append a signed data descriptor between member data and the central directory.
    let mut data_descriptor = vec![0x50, 0x4b, 0x07, 0x08];
    data_descriptor.extend_from_slice(&bytes[14..26]);
    descriptor[14..26].fill(0);
    descriptor.splice(central..central, data_descriptor);
    let end = descriptor.len() - 22;
    descriptor[end + 16..end + 20].copy_from_slice(&((central + 16) as u32).to_le_bytes());
    let temp = TempDir::new().unwrap();
    let prepared = prepare(&descriptor, &temp).unwrap();
    assert_eq!(
        fs::read(prepared.contents.join("file.php")).unwrap(),
        b"contents"
    );
    let empty = zip::ZipWriter::new(Cursor::new(Vec::new()))
        .finish()
        .unwrap()
        .into_inner();
    let empty = prepare(&empty, &temp).unwrap();
    assert_eq!(fs::read_dir(empty.contents).unwrap().count(), 0);
}

#[test]
fn invalid_archives_clean_staging_and_preserve_existing_installations() {
    let valid = zip_bytes(&[("package/file.php", b"contents")]);
    let mut corrupt = valid.clone();
    let data_start = zip::ZipArchive::new(Cursor::new(&corrupt))
        .unwrap()
        .by_index(0)
        .unwrap()
        .data_start()
        .unwrap() as usize;
    corrupt[data_start] ^= 1;
    let mut invalid = vec![corrupt, valid[..valid.len() - 22].to_vec()];
    for entries in [
        vec![("../escape", &b"bad"[..])],
        vec![("package/../escape", &b"bad"[..])],
        vec![
            ("package/A.php", &b"one"[..]),
            ("package/a.php", &b"two"[..]),
        ],
    ] {
        invalid.push(zip_bytes(&entries));
    }
    for bytes in invalid {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join("installed")).unwrap();
        fs::write(temp.path().join("installed/old"), b"old").unwrap();
        assert!(prepare(&bytes, &temp).is_err());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
        assert_eq!(fs::read(temp.path().join("installed/old")).unwrap(), b"old");
    }
}

#[test]
fn complete_body_including_trailing_bytes_is_hashed_and_cached() {
    let mut bytes = zip_bytes(&[("package/file.php", b"contents")]);
    bytes.extend_from_slice(b"trailing bytes");
    for kind in [
        ChecksumType::Sha1,
        ChecksumType::Sha256,
        ChecksumType::Sha384,
        ChecksumType::Sha512,
        ChecksumType::Md5,
    ] {
        let temp = TempDir::new().unwrap();
        let mut hasher = ChecksumHasher::new(kind);
        hasher.update(&bytes);
        let expected = hasher.finish().to_uppercase();
        let prepared = prepare_zip(
            Cursor::new(&bytes),
            &temp.path().join("archive.zip"),
            &temp.path().join("installed"),
            Some((kind, expected)),
            permit(),
            &AtomicBool::new(false),
            true,
        )
        .unwrap();
        assert_eq!(fs::read(prepared.archive.path()).unwrap(), bytes);
        drop(prepared);
        assert!(matches!(
            prepare_zip(
                Cursor::new(&bytes),
                &temp.path().join("archive.zip"),
                &temp.path().join("installed"),
                Some((kind, "wrong".into())),
                permit(),
                &AtomicBool::new(false),
                true
            ),
            Err(RiffError::ChecksumMismatch { .. })
        ));
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

async fn serve_split(
    bytes: Vec<u8>,
    split: usize,
    chunked: bool,
    truncate: bool,
) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/package.zip", listener.local_addr().unwrap());
    let (release, wait) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        let header = if chunked {
            "Transfer-Encoding: chunked\r\n".to_owned()
        } else {
            format!(
                "Content-Length: {}\r\n",
                bytes.len() + usize::from(truncate)
            )
        };
        stream
            .write_all(format!("HTTP/1.1 200 OK\r\n{header}Connection: close\r\n\r\n").as_bytes())
            .await
            .unwrap();
        if chunked {
            stream
                .write_all(format!("{split:x}\r\n").as_bytes())
                .await
                .unwrap();
        }
        stream.write_all(&bytes[..split]).await.unwrap();
        if chunked {
            stream.write_all(b"\r\n").await.unwrap();
        }
        let _ = wait.await;
        if truncate {
            return;
        }
        if chunked {
            let _ = stream
                .write_all(format!("{:x}\r\n", bytes.len() - split).as_bytes())
                .await;
        }
        let _ = stream.write_all(&bytes[split..]).await;
        if chunked {
            let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
        }
    });
    (url, release, server)
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

fn staged_file(temp: &TempDir) -> Option<PathBuf> {
    fs::read_dir(temp.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".riff-extract-")
        })
        .map(|entry| entry.path().join("contents/package/first.php"))
}

#[tokio::test]
async fn http_extracts_before_eof_with_and_without_content_length() {
    for chunked in [false, true] {
        let temp = TempDir::new().unwrap();
        let bytes = zip_bytes(&[
            ("package/first.php", b"first"),
            (
                "package/second.php",
                &vec![b's'; MIN_STREAMING_ZIP_SIZE as usize],
            ),
        ]);
        let split = zip::ZipArchive::new(Cursor::new(&bytes))
            .unwrap()
            .by_index(1)
            .unwrap()
            .header_start() as usize;
        let (url, release, server) = serve_split(bytes.clone(), split, chunked, false).await;
        let cache = temp.path().join("archive.zip");
        let dest = temp.path().join("installed");
        let task_cache = cache.clone();
        let task_dest = dest.clone();
        let task = tokio::spawn(async move {
            download_zip(
                &HttpClient::new().unwrap(),
                &url,
                &task_cache,
                &task_dest,
                None,
                permit(),
            )
            .await
        });
        wait_until(|| {
            staged_file(&temp).is_some_and(|path| fs::read(path).ok().as_deref() == Some(b"first"))
        })
        .await;
        assert!(!cache.exists());
        assert!(!dest.exists());
        release.send(()).unwrap();
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.await.unwrap();
        assert_eq!(fs::read(cache).unwrap(), bytes);
        assert_eq!(fs::read(dest.join("first.php")).unwrap(), b"first");
        let second = fs::read(dest.join("second.php")).unwrap();
        assert_eq!(second.len(), MIN_STREAMING_ZIP_SIZE as usize);
        assert!(second.iter().all(|byte| *byte == b's'));
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }
}

#[tokio::test]
async fn cancellation_and_truncated_http_clean_staging_and_release_worker_slot() {
    for cancel in [false, true] {
        let temp = TempDir::new().unwrap();
        let bytes = zip_bytes(&[
            ("package/first.php", b"first"),
            (
                "package/second.php",
                &vec![b's'; MIN_STREAMING_ZIP_SIZE as usize],
            ),
        ]);
        let split = zip::ZipArchive::new(Cursor::new(&bytes))
            .unwrap()
            .by_index(1)
            .unwrap()
            .header_start() as usize;
        let (url, release, server) = serve_split(bytes, split, false, true).await;
        let cache = temp.path().join("archive.zip");
        let dest = temp.path().join("installed");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("old"), b"old").unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned().unwrap();
        let task_cache = cache.clone();
        let task_dest = dest.clone();
        let task = tokio::spawn(async move {
            download_zip(
                &HttpClient::new().unwrap(),
                &url,
                &task_cache,
                &task_dest,
                None,
                permit,
            )
            .await
        });
        wait_until(|| staged_file(&temp).is_some_and(|path| path.is_file())).await;
        assert_eq!(slots.available_permits(), 0);
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            release.send(()).unwrap();
        } else {
            release.send(()).unwrap();
            assert!(matches!(
                task.await.unwrap(),
                Err(RiffError::DownloadFailed { .. })
            ));
        }
        server.await.unwrap();
        wait_until(|| {
            slots.available_permits() == 1 && fs::read_dir(temp.path()).unwrap().count() == 1
        })
        .await;
        assert!(!cache.exists());
        assert_eq!(fs::read(dest.join("old")).unwrap(), b"old");
    }
}

#[tokio::test]
async fn valid_zip_without_successful_http_completion_is_never_published() {
    for chunked in [false, true] {
        let temp = TempDir::new().unwrap();
        let bytes = zip_bytes(&[("package/first.php", b"first")]);
        let (url, release, server) = serve_split(bytes.clone(), bytes.len(), chunked, true).await;
        release.send(()).unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let result = download_zip(
            &HttpClient::new().unwrap(),
            &url,
            &temp.path().join("archive.zip"),
            &temp.path().join("installed"),
            None,
            slots.clone().try_acquire_owned().unwrap(),
        )
        .await;
        assert!(
            matches!(result, Err(RiffError::DownloadFailed { .. })),
            "{result:?}"
        );
        server.await.unwrap();
        wait_until(|| {
            slots.available_permits() == 1 && fs::read_dir(temp.path()).unwrap().count() == 0
        })
        .await;
    }
}

#[test]
fn zip64_and_prepended_archives_match_seekable_extraction() {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "package/file.php",
            zip::write::SimpleFileOptions::default().large_file(true),
        )
        .unwrap();
    writer.write_all(b"contents").unwrap();
    let zip64 = writer.finish().unwrap().into_inner();
    let mut prepended = b"executable prefix".to_vec();
    prepended.extend(zip_bytes(&[("package/file.php", b"contents")]));
    for bytes in [zip64, prepended] {
        let temp = TempDir::new().unwrap();
        let prepared = prepare(&bytes, &temp).unwrap();
        assert_eq!(
            fs::read(prepared.contents.join("file.php")).unwrap(),
            b"contents"
        );
    }
}

#[test]
fn cancelled_prepared_archive_preserves_destination_and_releases_resources() {
    let temp = TempDir::new().unwrap();
    let bytes = zip_bytes(&[("package/file.php", b"contents")]);
    let slots = Arc::new(Semaphore::new(1));
    let dest = temp.path().join("installed");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("old"), b"old").unwrap();
    let prepared = prepare_zip(
        Cursor::new(bytes),
        &temp.path().join("archive.zip"),
        &dest,
        None,
        slots.clone().try_acquire_owned().unwrap(),
        &AtomicBool::new(false),
        true,
    )
    .unwrap();
    assert_eq!(slots.available_permits(), 0);
    assert!(prepared
        .publish(
            &temp.path().join("archive.zip"),
            &dest,
            &AtomicBool::new(true)
        )
        .is_err());
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    assert_eq!(fs::read(dest.join("old")).unwrap(), b"old");
}

#[test]
fn failed_publication_restores_previous_installation() {
    let temp = TempDir::new().unwrap();
    let mut prepared = prepare(&zip_bytes(&[("package/file.php", b"contents")]), &temp).unwrap();
    // Simulate the staged directory disappearing before the final rename.
    fs::remove_dir_all(&prepared.contents).unwrap();
    prepared.contents = prepared.directory.path().join("missing");
    let destination = temp.path().join("installed");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("old"), b"old").unwrap();
    assert!(prepared
        .publish(
            &temp.path().join("archive.zip"),
            &destination,
            &AtomicBool::new(false)
        )
        .is_err());
    assert_eq!(fs::read(destination.join("old")).unwrap(), b"old");
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
}
