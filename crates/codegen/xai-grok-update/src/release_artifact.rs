use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use super::{
    download_silent, download_with_progress, parse_published_checksum, publish_downloaded_artifact,
    sha256_file, tmp_download_path,
};

const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy)]
enum Codec {
    Zstd,
    Gzip,
    TarGzip,
    Raw,
}

struct TemporaryArtifact(PathBuf);

impl Drop for TemporaryArtifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(super) async fn download_verified(
    base: &str,
    tag: &str,
    asset: &str,
    destination: &Path,
    with_progress: bool,
) -> Result<()> {
    for (suffix, codec) in [
        (".zst", Codec::Zstd),
        (".gz", Codec::Gzip),
        (".tar.gz", Codec::TarGzip),
        ("", Codec::Raw),
    ] {
        let name = format!("{asset}{suffix}");
        let url = format!("{base}/download/{tag}/{name}");
        let downloaded = TemporaryArtifact(tmp_download_path(destination));
        let result = if with_progress {
            download_with_progress(&url, &downloaded.0).await
        } else {
            download_silent(&url, &downloaded.0).await
        };
        if let Err(error) = result {
            if !suffix.is_empty()
                && error
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|error| error.status() == Some(reqwest::StatusCode::NOT_FOUND))
            {
                continue;
            }
            return Err(error)
                .with_context(|| format!("downloading Open Grok release asset {name}"));
        }
        let checksum = TemporaryArtifact(tmp_download_path(destination));
        download_silent(&format!("{url}.sha256"), &checksum.0)
            .await
            .context("downloading published Open Grok SHA-256")?;
        let checksum_contents = tokio::fs::read_to_string(&checksum.0).await?;
        let expected = parse_published_checksum(&checksum_contents, &name)?;
        let actual = sha256_file(&downloaded.0).await?;
        if actual != expected {
            anyhow::bail!(
                "Open Grok SHA-256 verification failed (expected {expected}, got {actual}); current version was not changed"
            );
        }
        if matches!(codec, Codec::Raw) {
            return publish_downloaded_artifact(&downloaded.0, destination).await;
        }
        let decoded = TemporaryArtifact(tmp_download_path(destination));
        let asset = asset.to_owned();
        let decoded = tokio::task::spawn_blocking(move || -> Result<TemporaryArtifact> {
            decode(&downloaded.0, &decoded.0, codec, &asset, MAX_DECODED_BYTES)?;
            Ok(decoded)
        })
        .await
        .context("decoding Open Grok release artifact")??;
        return publish_downloaded_artifact(&decoded.0, destination).await;
    }
    anyhow::bail!("no Open Grok release asset found")
}

fn decode(source: &Path, destination: &Path, codec: Codec, asset: &str, limit: u64) -> Result<()> {
    let source = std::fs::File::open(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    match codec {
        Codec::Zstd => copy_capped(
            zstd::stream::read::Decoder::new(source)?,
            &mut output,
            limit,
        )?,
        Codec::Gzip => copy_capped(flate2::read::GzDecoder::new(source), &mut output, limit)?,
        Codec::TarGzip => {
            let decoder = flate2::read::GzDecoder::new(source);
            let mut archive = tar::Archive::new(decoder);
            let mut entries = archive.entries()?;
            let mut entry = entries.next().context("release archive is empty")??;
            if !entry.header().entry_type().is_file() || entry.size() > limit {
                anyhow::bail!("release archive must contain a bounded regular binary");
            }
            let path = entry.path()?;
            let mut components = path.components();
            let Some(Component::Normal(filename)) = components.next() else {
                anyhow::bail!("release archive contains an unsafe binary path");
            };
            let expected_command = if asset.ends_with(".exe") {
                "open-grok.exe"
            } else {
                "open-grok"
            };
            if components.next().is_some() || (filename != asset && filename != expected_command) {
                anyhow::bail!("release archive contains an unexpected binary path");
            }
            copy_capped(&mut entry, &mut output, limit)?;
            drop(entry);
            if entries.next().transpose()?.is_some() {
                anyhow::bail!("release archive must contain exactly one binary");
            }
            drop(entries);
            let mut decoder = archive.into_inner().take(1025);
            let trailing = std::io::copy(&mut decoder, &mut std::io::sink())?;
            if trailing > 1024 {
                anyhow::bail!("release archive contains excessive trailing data");
            }
        }
        Codec::Raw => copy_capped(source, &mut output, limit)?,
    }
    output.flush()?;
    output.sync_all()?;
    Ok(())
}

fn copy_capped(source: impl Read, destination: &mut impl Write, limit: u64) -> Result<()> {
    let written = std::io::copy(&mut source.take(limit + 1), destination)
        .context("decoding release binary")?;
    if written == 0 || written > limit {
        anyhow::bail!("decoded release binary is empty or exceeds the {limit}-byte cap");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    async fn mount(server: &MockServer, suffix: &str, bytes: &[u8], digest: &str) {
        let name = format!("open-grok-test{suffix}");
        let asset_path = format!("/download/v1.0.0/{name}");
        Mock::given(method("GET"))
            .and(path(asset_path.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{asset_path}.sha256")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{digest}  {name}\n")))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn verified_compressed_and_legacy_raw_assets_download() {
        let bytes = b"binary payload";
        for (suffix, encoded) in [
            (
                ".zst",
                zstd::stream::encode_all(bytes.as_slice(), 1).unwrap(),
            ),
            (".gz", gzip(bytes)),
            ("", bytes.to_vec()),
        ] {
            let server = MockServer::start().await;
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("binary");
            mount(
                &server,
                suffix,
                &encoded,
                &format!("{:x}", Sha256::digest(&encoded)),
            )
            .await;
            download_verified(
                &server.uri(),
                "v1.0.0",
                "open-grok-test",
                &destination,
                false,
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(destination).unwrap(), bytes);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn checksum_failure_never_falls_back_or_replaces_existing_binary() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("binary");
        std::fs::write(&destination, "old binary").unwrap();
        mount(&server, ".zst", b"tampered", &"0".repeat(64)).await;
        let error = download_verified(
            &server.uri(),
            "v1.0.0",
            "open-grok-test",
            &destination,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("SHA-256"));
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "old binary");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn server_failure_never_falls_back_to_another_codec() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("binary");
        std::fs::write(&destination, "old binary").unwrap();
        Mock::given(method("GET"))
            .and(path("/download/v1.0.0/open-grok-test.zst"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let error = download_verified(
            &server.uri(),
            "v1.0.0",
            "open-grok-test",
            &destination,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status),
            Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| { request.url.path() == "/download/v1.0.0/open-grok-test.zst" })
        );
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "old binary");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn decode_rejects_oversized_empty_and_corrupt_streams() {
        for (bytes, limit) in [
            (gzip(&[1; 100]), 10),
            (gzip(b""), 100),
            (b"bad gzip".to_vec(), 100),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("compressed");
            std::fs::write(&source, bytes).unwrap();
            assert!(
                decode(
                    &source,
                    &directory.path().join("output"),
                    Codec::Gzip,
                    "open-grok",
                    limit
                )
                .is_err()
            );
        }
    }

    #[test]
    fn tar_archive_accepts_only_one_expected_regular_binary() {
        for (name, kind, duplicate, accepted) in [
            ("open-grok", tar::EntryType::Regular, false, true),
            ("nested/open-grok", tar::EntryType::Regular, false, false),
            ("../open-grok", tar::EntryType::Regular, false, false),
            ("/open-grok", tar::EntryType::Regular, false, false),
            ("other", tar::EntryType::Regular, false, false),
            ("open-grok", tar::EntryType::Symlink, false, false),
            ("open-grok", tar::EntryType::Link, false, false),
            ("open-grok", tar::EntryType::Directory, false, false),
            ("open-grok", tar::EntryType::Regular, true, false),
        ] {
            let mut archive = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_mode(0o755);
            header.set_size(7);
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            archive.append(&header, b"payload".as_slice()).unwrap();
            if duplicate {
                archive.append(&header, b"payload".as_slice()).unwrap();
            }
            let bytes = gzip(&archive.into_inner().unwrap());
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("archive");
            std::fs::write(&source, bytes).unwrap();
            let output = directory.path().join("output");
            assert_eq!(
                decode(&source, &output, Codec::TarGzip, "open-grok-test", 100).is_ok(),
                accepted,
                "{name}"
            );
            if accepted {
                assert_eq!(std::fs::read(output).unwrap(), b"payload");
            }
        }
    }
}
