//! Archive readers must reject everything the registry rejects (registry/test/archives.test.mjs).

mod support;

use ppr_tool::archive::metadata_file;
use ppr_tool::names::Format;
use support::*;

const PACKAGE_JSON: &str = r#"{"name":"@test/archive","version":"1.0.0"}"#;

fn npm_entries(extra: &[(&str, &[u8], u8)]) -> Vec<u8> {
    let mut entries: Vec<(&str, &[u8], u8)> = vec![("package/package.json", PACKAGE_JSON.as_bytes(), b'0')];
    entries.extend_from_slice(extra);
    gzip(&tar(&entries))
}

#[track_caller]
fn rejected(format: Format, bytes: &[u8]) -> String {
    match metadata_file(format, bytes) {
        Ok(file) => panic!("archive was accepted (metadata {})", file.name),
        Err(error) => error.message,
    }
}

#[test]
fn reads_metadata_for_every_format() {
    let npm = metadata_file(Format::Npm, &npm_entries(&[])).unwrap();
    assert_eq!((npm.name.as_str(), npm.text.as_str()), ("package/package.json", PACKAGE_JSON));
    let cargo = metadata_file(Format::Cargo, &crate_archive("demo", "1.2.3")).unwrap();
    assert_eq!(cargo.name, "demo-1.2.3/Cargo.toml");
    let nuget = metadata_file(Format::Nuget, &nupkg("Demo.Sdk", "1.2.3")).unwrap();
    assert_eq!(nuget.name, "Demo.Sdk.nuspec");
    assert!(nuget.text.contains("<id>Demo.Sdk</id>"));
    let composer = metadata_file(Format::Composer, &composer("demo/sdk", "1.2.3")).unwrap();
    assert_eq!(composer.name, "composer.json");
}

#[test]
fn strips_utf8_bom_and_rejects_invalid_utf8() {
    let with_bom = tar_gz(&[("package/package.json", "\u{feff}{}".as_bytes())]);
    assert_eq!(metadata_file(Format::Npm, &with_bom).unwrap().text, "{}");
    let invalid = tar_gz(&[("package/package.json", b"{\xff}")]);
    assert!(rejected(Format::Npm, &invalid).contains("not valid UTF-8"));
}

#[test]
fn rejects_unsafe_tar_archives() {
    assert!(rejected(Format::Npm, &npm_entries(&[("../escape", b"bad", b'0')])).contains("unsafe"));
    assert!(rejected(Format::Npm, &npm_entries(&[("package/link", b"", b'2')])).contains("unsafe"));
    assert!(rejected(Format::Npm, &npm_entries(&[("package/package.json", PACKAGE_JSON.as_bytes(), b'0')])).contains("unsafe"));
    assert!(rejected(Format::Npm, &npm_entries(&[("package//duplicate", b"bad", b'0')])).contains("unsafe"));
    assert!(rejected(Format::Npm, &npm_entries(&[("package/dir/", b"x", b'5')])).contains("unsafe"));
    let bomb = vec![0u8; 64 * 1024 * 1024];
    assert!(rejected(Format::Npm, &npm_entries(&[("package/bomb", &bomb, b'0')])).contains("64 MiB"));
    assert!(rejected(Format::Npm, &tar_gz(&[("package/other.json", b"{}")])).contains("does not contain"));
}

#[test]
fn rejects_corrupt_tar_streams() {
    let mut raw = tar(&[("package/package.json", PACKAGE_JSON.as_bytes(), b'0')]);
    raw[0] ^= 1; // breaks the header checksum
    assert!(rejected(Format::Npm, &gzip(&raw)).contains("checksum"));

    let mut trailing = tar(&[("package/package.json", PACKAGE_JSON.as_bytes(), b'0')]);
    trailing.extend_from_slice(&[1u8; 512]);
    assert!(rejected(Format::Npm, &gzip(&trailing)).contains("after TAR end"));

    let valid = tar(&[("package/package.json", PACKAGE_JSON.as_bytes(), b'0')]);
    let truncated = &valid[..valid.len() - 1024];
    assert!(rejected(Format::Npm, &gzip(truncated)).contains("incomplete"));

    // DecompressionStream rejects concatenated members and garbage after the gzip trailer.
    let mut concatenated = gzip(&valid);
    concatenated.extend_from_slice(&gzip(b"more"));
    assert!(rejected(Format::Npm, &concatenated).contains("GZIP"));
    let mut garbage = gzip(&valid);
    garbage.push(0);
    assert!(rejected(Format::Npm, &garbage).contains("GZIP"));
    assert!(rejected(Format::Npm, b"not gzip").contains("GZIP"));
}

#[test]
fn rejects_unsafe_zip_archives() {
    let manifest = br#"{"name":"test/archive","version":"1.0.0"}"#;
    let ok = zip(&[ZipEntry::new("composer.json", manifest)]);
    assert!(metadata_file(Format::Composer, &ok).is_ok());

    assert!(rejected(Format::Composer, &zip(&[ZipEntry::new("composer.json", manifest), ZipEntry::new("../escape", b"bad")])).contains("unsafe"));
    assert!(rejected(Format::Composer, &ok[..ok.len() - 10]).contains("ZIP"));

    // Symlink mode bits in the external attributes.
    let mut link = ok.clone();
    let central = link.windows(4).position(|w| w == [0x50, 0x4b, 0x01, 0x02]).unwrap();
    link[central + 38..central + 42].copy_from_slice(&0xa000_0000u32.to_le_bytes());
    assert!(rejected(Format::Composer, &link).contains("unsafe"));

    // Corrupted payload fails the CRC check.
    let stored = zip(&[ZipEntry { deflate: false, ..ZipEntry::new("composer.json", manifest) }]);
    let mut corrupt = stored.clone();
    corrupt[30 + "composer.json".len()] ^= 1;
    assert!(rejected(Format::Composer, &corrupt).contains("integrity"));

    assert!(rejected(Format::Composer, &zip(&[ZipEntry::new("composer.json", manifest), ZipEntry::new("composer.json", manifest)])).contains("unsafe"));
}

#[test]
fn walks_zip_local_headers_like_the_server() {
    let manifest = br#"{"name":"test/archive","version":"1.0.0"}"#;
    // Entries streamed with data descriptors are fine.
    let described = zip(&[
        ZipEntry { descriptor: true, ..ZipEntry::new("composer.json", manifest) },
        ZipEntry { descriptor: true, deflate: false, ..ZipEntry::new("src/a.php", b"<?php") },
    ]);
    assert!(metadata_file(Format::Composer, &described).is_ok());

    // A local entry hidden from the central directory is seen by a streaming reader.
    let hidden = zip(&[ZipEntry::new("hidden.php", b"<?php evil")]);
    let hidden_local = &hidden[..hidden.windows(4).position(|w| w == [0x50, 0x4b, 0x01, 0x02]).unwrap()];
    let smuggled = zip_with(&[ZipEntry::new("composer.json", manifest)], hidden_local);
    assert!(rejected(Format::Composer, &smuggled).contains("ZIP"));

    // Local header name that differs from the central directory.
    let mut renamed = zip(&[ZipEntry::new("composer.json", manifest)]);
    renamed[30] = b'C';
    assert!(rejected(Format::Composer, &renamed).contains("unexpected ZIP entry"));

    // Local sizes that disagree with the central directory.
    let mut sizes = zip(&[ZipEntry::new("composer.json", manifest)]);
    sizes[22] ^= 1;
    assert!(rejected(Format::Composer, &sizes).contains("does not match"));
}

#[test]
fn reads_nuspec_at_the_archive_root_only() {
    let spec = nuspec("Demo", "1.0.0");
    let nested = zip(&[ZipEntry::new("content/Demo.nuspec", spec.as_bytes())]);
    assert!(rejected(Format::Nuget, &nested).contains("does not contain"));
    let two = zip(&[ZipEntry::new("Demo.nuspec", spec.as_bytes()), ZipEntry::new("Other.NUSPEC", spec.as_bytes())]);
    assert!(rejected(Format::Nuget, &two).contains("multiple"));
}
