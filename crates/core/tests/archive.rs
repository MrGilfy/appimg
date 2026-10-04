//! Finding the one AppImage in a zip or tar archive and writing it out,
//! against archives built right here, well-formed, broken and hostile. All
//! of it inside a temporary directory.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use appimg_core::archive::{self, Extracted};
use appimg_core::Error;
use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, Crc};

/// The front of an AppImage of type 2, as far as anyone looking for one
/// reads: the ELF magic, and `AI` with the type where the ELF header leaves
/// room. The rest is `marker`.
fn appimage(marker: &str) -> Vec<u8> {
    [&b"\x7fELF\x02\x01\x01\x00AI\x02"[..], marker.as_bytes()].concat()
}

/// An ELF file that is no AppImage, the way a shared library is.
fn library() -> Vec<u8> {
    b"\x7fELF\x02\x01\x01\x00\x00\x00\x00 not an AppImage".to_vec()
}

/// One entry of a zip archive, as [`zip`] writes it.
#[derive(Clone)]
struct ZipEntry {
    name: Vec<u8>,
    data: Vec<u8>,
    deflate: bool,
    flags: u16,
    extra: Vec<u8>,
    /// The uncompressed size to claim instead of the real one.
    claimed_size: Option<u32>,
}

fn entry(name: &str, data: &[u8]) -> ZipEntry {
    ZipEntry {
        name: name.as_bytes().to_vec(),
        data: data.to_vec(),
        deflate: true,
        flags: 0,
        extra: Vec::new(),
        claimed_size: None,
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(data);
    crc.sum()
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// A zip archive as PKWARE's APPNOTE lays one out: local headers with the
/// data, the central directory, and the end record.
fn zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let packed = if entry.deflate { deflate(&entry.data) } else { entry.data.clone() };
        let method: u16 = if entry.deflate { 8 } else { 0 };
        let size = entry.claimed_size.unwrap_or(entry.data.len() as u32);
        let offset = out.len() as u32;
        let common = |out: &mut Vec<u8>| {
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&entry.flags.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]); // time, date
            out.extend_from_slice(&crc32(&entry.data).to_le_bytes());
            out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&(entry.extra.len() as u16).to_le_bytes());
        };

        out.extend_from_slice(b"PK\x03\x04");
        common(&mut out);
        out.extend_from_slice(&entry.name);
        out.extend_from_slice(&entry.extra);
        out.extend_from_slice(&packed);

        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&(3u16 << 8 | 20).to_le_bytes());
        common(&mut central);
        central.extend_from_slice(&[0; 2]); // comment length
        central.extend_from_slice(&[0; 4]); // disk, internal attributes
        central.extend_from_slice(&(0o100644u32 << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(&entry.name);
        central.extend_from_slice(&entry.extra);
    }
    let directory_at = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&directory_at.to_le_bytes());
    out.extend_from_slice(&[0; 2]);
    out
}

/// A tar header for `name`, `size` bytes of type `kind`, in ustar form.
fn tar_header(name: &[u8], size: u64, kind: u8, link: &[u8]) -> Vec<u8> {
    let mut header = vec![0u8; 512];
    let name = &name[..name.len().min(100)];
    header[..name.len()].copy_from_slice(name);
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].copy_from_slice(b"        ");
    header[156] = kind;
    header[157..157 + link.len()].copy_from_slice(link);
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum: u32 = header.iter().map(|byte| *byte as u32).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    header
}

fn padded(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    out.resize(data.len().next_multiple_of(512), 0);
    out
}

/// A tar file of regular files. A name longer than a header holds goes
/// ahead in a GNU long name record, the way GNU tar writes one.
fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        if name.len() > 100 {
            let long = [name.as_bytes(), b"\0"].concat();
            out.extend(tar_header(b"././@LongLink", long.len() as u64, b'L', b""));
            out.extend(padded(&long));
        }
        out.extend(tar_header(name.as_bytes(), data.len() as u64, b'0', b""));
        out.extend(padded(data));
    }
    out.extend([0u8; 1024]);
    out
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// A temporary directory with an archive in it and a place to unpack to.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self { dir: tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap() }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn archive(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root().join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn dest(&self) -> PathBuf {
        self.root().join("out/app.AppImage.new")
    }

    /// Unpacks `bytes`, stored under `name`, into [`Scratch::dest`].
    fn unpack(&self, name: &str, bytes: &[u8]) -> appimg_core::Result<Extracted> {
        let archive = self.archive(name, bytes);
        fs::create_dir_all(self.dest().parent().unwrap()).unwrap();
        archive::extract_appimage(&archive, &self.dest())
    }

    fn unpack_within(
        &self,
        name: &str,
        bytes: &[u8],
        limit: u64,
    ) -> appimg_core::Result<Extracted> {
        let archive = self.archive(name, bytes);
        fs::create_dir_all(self.dest().parent().unwrap()).unwrap();
        archive::extract_appimage_within(&archive, &self.dest(), limit)
    }

    /// Every file and directory below the root.
    fn files(&self) -> BTreeSet<PathBuf> {
        let mut found = BTreeSet::new();
        let mut stack = vec![self.root().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                found.insert(path.strip_prefix(self.root()).unwrap().to_path_buf());
                if entry.file_type().unwrap().is_dir() {
                    stack.push(path);
                }
            }
        }
        found
    }
}

/// Why an archive was refused.
fn refusal(result: appimg_core::Result<Extracted>) -> String {
    match result {
        Err(Error::Archive { reason, .. }) => reason,
        other => panic!("expected an archive error, got {other:?}"),
    }
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_appimage_in_a_zip_is_found_by_its_bytes_whatever_it_is_called() {
    let soh = appimage(&"soh ".repeat(4000));
    for deflate in [true, false] {
        let scratch = Scratch::new();
        let entries: Vec<ZipEntry> = [
            entry("SoH/", b""),
            entry("SoH/readme.txt", b"Ship of Harkinian"),
            entry("SoH/lib/libSDL2.so", &library()),
            entry("SoH/soh.appimage", &soh),
            entry("SoH/gamecontrollerdb.txt", b"..."),
        ]
        .into_iter()
        .map(|entry| ZipEntry { deflate, ..entry })
        .collect();

        let extracted = scratch.unpack("SoH-Ackbar-Delta-Linux.zip", &zip(&entries)).unwrap();
        assert_eq!(
            extracted,
            Extracted { entry: "SoH/soh.appimage".to_string(), size: soh.len() as u64 }
        );
        assert_eq!(fs::read(scratch.dest()).unwrap(), soh);
        assert_eq!(mode(&scratch.dest()), 0o755);
    }
}

#[test]
fn the_appimage_in_a_tar_is_found_plain_or_gzipped_long_name_or_not() {
    let app = appimage(&"app ".repeat(4000));
    let long = format!("{}/app.AppImage", "deep/".repeat(30));
    for name in ["App/App-x86_64.appimage", long.as_str()] {
        let entries: &[(&str, &[u8])] =
            &[("App/README", b"hello"), (name, &app), ("App/lib.so", &library())];
        for (file, bytes) in [("app.tar", tar(entries)), ("app.tar.gz", gzip(&tar(entries)))] {
            let scratch = Scratch::new();
            let extracted = scratch.unpack(file, &bytes).unwrap();
            assert_eq!(extracted.entry, name, "{file}");
            assert_eq!(fs::read(scratch.dest()).unwrap(), app, "{file}");
        }
    }

    // A pax record names it as well.
    let path = "pax/named/app.AppImage";
    // The length counts itself: two digits, then " path=", the path and "\n".
    let record = format!("{} path={path}\n", 2 + " path=".len() + path.len() + 1);
    let mut bytes = tar_header(b"PaxHeaders/app", record.len() as u64, b'x', b"");
    bytes.extend(padded(record.as_bytes()));
    bytes.extend(tar(&[("short", &app)]));
    let scratch = Scratch::new();
    assert_eq!(scratch.unpack("pax.tgz", &gzip(&bytes)).unwrap().entry, path);
}

#[test]
fn an_archive_without_an_appimage_is_refused_and_says_what_it_holds() {
    let entries = [entry("game/readme.txt", b"hello"), entry("game/soh.elf", &library())];
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("game.zip", &zip(&entries)));
    assert_eq!(reason, "it holds no AppImage, only game/readme.txt, game/soh.elf");
    assert!(!scratch.dest().exists());

    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("game.tgz", &gzip(&tar(&[("only.txt", b"text")]))));
    assert_eq!(reason, "it holds no AppImage, only only.txt");

    let scratch = Scratch::new();
    assert_eq!(
        refusal(scratch.unpack("empty.zip", &zip(&[entry("dir/", b"")]))),
        "it holds no files"
    );

    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("plain.zip", b"PK\x03\x04 but nothing else"));
    assert!(reason.contains("no end of central directory"), "{reason}");
    let reason = refusal(scratch.unpack("notes.txt", b"just text"));
    assert_eq!(reason, "it is no zip or tar archive");
}

#[test]
fn more_than_one_appimage_is_refused_and_names_them() {
    let (one, two) = (appimage("one"), appimage("two"));
    let scratch = Scratch::new();
    let entries = [entry("a/One.AppImage", &one), entry("note", b"x"), entry("b/two", &two)];
    let reason = refusal(scratch.unpack("two.zip", &zip(&entries)));
    assert_eq!(reason, "it holds 2 AppImages, and appimg takes exactly one: a/One.AppImage, b/two");
    assert!(!scratch.dest().exists());

    // A tar is read front to back, so the first one is written before the
    // second turns up. It does not stay.
    let scratch = Scratch::new();
    let bytes = gzip(&tar(&[("a/One.AppImage", &one), ("b/two", &two)]));
    let reason = refusal(scratch.unpack("two.tar.gz", &bytes));
    assert!(reason.starts_with("it holds 2 AppImages"), "{reason}");
    assert!(!scratch.dest().exists());
}

#[test]
fn encrypted_and_zip64_entries_are_refused() {
    let app = appimage("app");

    let encrypted = ZipEntry { flags: 1, ..entry("app.AppImage", &app) };
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("locked.zip", &zip(&[entry("readme", b"x"), encrypted])));
    assert_eq!(reason, "the entry app.AppImage is encrypted, which appimg does not read");
    assert!(!scratch.dest().exists());

    // The zip64 extra field, id 1, with sizes in it.
    let extra = [&1u16.to_le_bytes()[..], &16u16.to_le_bytes(), &[0u8; 16]].concat();
    let zip64 = ZipEntry { extra, ..entry("big.AppImage", &app) };
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("big.zip", &zip(&[zip64])));
    assert_eq!(
        reason,
        "the entry big.AppImage is stored in zip64 form, which appimg does not read"
    );

    // An end record that hands over to a zip64 one.
    let mut bytes = zip(&[entry("app.AppImage", &app)]);
    let end = bytes.len() - 22;
    bytes[end + 8..end + 12].copy_from_slice(&[0xff; 4]);
    let scratch = Scratch::new();
    assert_eq!(
        refusal(scratch.unpack("huge.zip", &bytes)),
        "it is a zip64 archive, which appimg does not read"
    );

    // A compression method other than stored and deflate.
    let mut bytes = zip(&[entry("app.AppImage", &app)]);
    let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    bytes[central + 10..central + 12].copy_from_slice(&14u16.to_le_bytes());
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("lzma.zip", &bytes));
    assert!(reason.contains("is compressed with method 14"), "{reason}");
}

#[test]
fn damage_is_noticed_and_leaves_nothing_behind() {
    let app = appimage(&"payload".repeat(200));

    // One byte of a stored entry changed after its checksum was taken.
    let mut bytes = zip(&[ZipEntry { deflate: false, ..entry("app.AppImage", &app) }]);
    let at = bytes.windows(7).position(|w| w == b"payload").unwrap();
    bytes[at] = b'P';
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("flipped.zip", &bytes));
    assert_eq!(
        reason,
        "app.AppImage does not match the checksum the archive holds for it: it is damaged"
    );
    assert!(!scratch.dest().exists());

    // A gzip stream cut off halfway.
    let bytes = gzip(&tar(&[("app.AppImage", &app)]));
    let scratch = Scratch::new();
    let result = scratch.unpack("cut.tgz", &bytes[..bytes.len() / 2]);
    assert!(matches!(result, Err(Error::Archive { .. })), "{result:?}");
    assert!(!scratch.dest().exists());

    // The gzip trailer, which only a reader that goes past the end of the
    // tar file checks.
    let mut bytes = gzip(&tar(&[("app.AppImage", &app)]));
    let crc = bytes.len() - 8;
    bytes[crc] ^= 0xff;
    let scratch = Scratch::new();
    let result = scratch.unpack("crc.tgz", &bytes);
    assert!(matches!(result, Err(Error::Archive { .. })), "{result:?}");
    assert!(!scratch.dest().exists());

    // A header whose checksum is off is no tar file.
    let mut bytes = tar(&[("app.AppImage", &app)]);
    bytes[0] = b'X';
    let scratch = Scratch::new();
    assert_eq!(refusal(scratch.unpack("bad.tar", &bytes)), "it is no tar archive");
}

/// The limit holds whatever the archive claims: a size it states above the
/// limit is refused before anything is written, and data that runs on past
/// a smaller claim is cut off at the limit.
#[test]
fn the_size_cap_stops_what_the_archive_would_unpack_to() {
    let app = appimage(&"x".repeat(5000));

    let scratch = Scratch::new();
    let reason =
        refusal(scratch.unpack_within("big.zip", &zip(&[entry("app.AppImage", &app)]), 1000));
    assert!(
        reason.starts_with("app.AppImage would unpack to more than 1000 B (1000 bytes)"),
        "{reason}"
    );
    assert!(!scratch.dest().exists());

    let lying = ZipEntry { claimed_size: Some(10), ..entry("app.AppImage", &app) };
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack_within("lying.zip", &zip(&[lying]), 1000));
    assert!(reason.contains("would unpack to more than"), "{reason}");
    assert!(!scratch.dest().exists());

    let scratch = Scratch::new();
    let reason =
        refusal(scratch.unpack_within("big.tgz", &gzip(&tar(&[("app.AppImage", &app)])), 1000));
    assert!(reason.contains("would unpack to more than"), "{reason}");
    assert!(!scratch.dest().exists());

    // Within the limit it is all there.
    let scratch = Scratch::new();
    scratch
        .unpack_within("fits.zip", &zip(&[entry("app.AppImage", &app)]), app.len() as u64)
        .unwrap();
    assert_eq!(fs::read(scratch.dest()).unwrap(), app);
}

/// The limit an archive gets without asking: 70 MiB of zeros behind the
/// AppImage magic packs into a third of a megabyte, far less than an
/// AppImage that size ever does.
#[test]
fn a_compression_bomb_is_stopped_by_the_limit_its_own_size_sets() {
    let mut bomb = appimage("");
    bomb.resize(70 << 20, 0);

    let bytes = zip(&[entry("bomb.AppImage", &bomb)]);
    assert!(archive::limit_for(bytes.len() as u64) < bomb.len() as u64);
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("bomb.zip", &bytes));
    assert!(reason.contains("would unpack to more than"), "{reason}");
    assert!(!scratch.dest().exists());

    let bytes = gzip(&tar(&[("bomb.AppImage", &bomb)]));
    let scratch = Scratch::new();
    let reason = refusal(scratch.unpack("bomb.tar.gz", &bytes));
    assert!(reason.contains("would unpack to more than"), "{reason}");
    assert!(!scratch.dest().exists());
}

/// Names that climb out of the directory, absolute ones, links pointing
/// anywhere: none of it is ever a path anything is written to. The one file
/// written is the one asked for.
#[test]
fn no_path_out_of_the_archive_is_ever_written() {
    let scratch = Scratch::new();
    let id = scratch.root().file_name().unwrap().to_string_lossy().into_owned();
    let outside = scratch.root().parent().unwrap().to_path_buf();
    let app = appimage("evil");
    let names = [
        format!("../../escape-{id}.AppImage"),
        format!("/tmp/absolute-{id}"),
        format!("../sibling-{id}.txt"),
        format!("dir/../../up-{id}"),
    ];

    let zip_bytes = zip(&[
        entry(&names[0], &app),
        entry(&names[1], b"x"),
        entry(&names[2], b"x"),
        entry(&names[3], b"x"),
    ]);
    let mut tar_bytes = tar_header(format!("link-{id}").as_bytes(), 0, b'2', b"/etc/passwd");
    tar_bytes.extend(tar_header(format!("hard-{id}").as_bytes(), 0, b'1', b"../../etc/shadow"));
    tar_bytes.extend(tar(&[
        (&names[0], &app),
        (&names[1], b"x"),
        (&names[2], b"x"),
        (&names[3], b"x"),
    ]));

    for (file, bytes) in [("hostile.zip", zip_bytes), ("hostile.tar.gz", gzip(&tar_bytes))] {
        let before = scratch.files();
        let extracted = scratch.unpack(file, &bytes).unwrap();
        assert_eq!(extracted.entry, names[0]);
        assert_eq!(fs::read(scratch.dest()).unwrap(), app);

        let mut expected = before.clone();
        expected.extend([
            PathBuf::from(file),
            PathBuf::from("out"),
            PathBuf::from("out/app.AppImage.new"),
        ]);
        assert_eq!(scratch.files(), expected, "{file}");
        for name in ["escape", "absolute", "sibling", "up", "link", "hard"] {
            assert!(!outside.join(format!("{name}-{id}")).exists(), "{name}");
            assert!(!outside.join(format!("{name}-{id}.AppImage")).exists(), "{name}");
            assert!(!Path::new("/tmp").join(format!("{name}-{id}")).exists(), "{name}");
        }
        fs::remove_file(scratch.dest()).unwrap();
    }
}

#[test]
fn names_shown_from_an_archive_cannot_drive_a_terminal() {
    let scratch = Scratch::new();
    let entries = [entry("\x1b]0;owned\x07.txt", b"x")];
    let reason = refusal(scratch.unpack("escape.zip", &zip(&entries)));
    assert_eq!(reason, "it holds no AppImage, only ?]0;owned?.txt");
}

/// What an install, an adoption and the terminal interface take an
/// AppImage out of a file on disk with: named after the archive, never
/// after anything in it, within the limit the archive's size sets, and
/// given the checks a download gets, with the archive named when it fails.
#[test]
fn unpacking_for_an_install_names_the_file_after_the_archive_and_keeps_the_rules() {
    use appimg_core::install;

    let scratch = Scratch::new();
    let out = scratch.root().join("out");
    fs::create_dir_all(&out).unwrap();
    let app = appimage("soh");
    let archive = scratch.archive(
        "SoH-9.2.3-Linux.zip",
        &zip(&[entry("readme.txt", b"x"), entry("../../soh.appimage", &app)]),
    );
    let (file, extracted) = install::unpack_archive(&archive, &out).unwrap();
    assert_eq!(file, out.join("SoH-9.2.3-Linux.AppImage"));
    assert_eq!(extracted.entry, "../../soh.appimage");
    assert_eq!(fs::read(&file).unwrap(), app);
    assert_eq!(mode(&file), 0o755);
    fs::remove_file(&file).unwrap();

    // Without an ending that makes it an archive, the whole name is kept.
    let archive = scratch.archive("download", &gzip(&tar(&[("soh", &app)])));
    let (file, _) = install::unpack_archive(&archive, &out).unwrap();
    assert_eq!(file, out.join("download.AppImage"));
    fs::remove_file(&file).unwrap();

    let mut bomb = appimage("");
    bomb.resize(70 << 20, 0);
    let archive = scratch.archive("bomb.zip", &zip(&[entry("bomb.AppImage", &bomb)]));
    let error = install::unpack_archive(&archive, &out).unwrap_err().to_string();
    assert!(error.contains("would unpack to more than"), "{error}");

    let two = zip(&[entry("a.AppImage", &app), entry("b", &app)]);
    let archive = scratch.archive("two.zip", &two);
    let error = install::unpack_archive(&archive, &out).unwrap_err().to_string();
    assert!(error.contains("it holds 2 AppImages, and appimg takes exactly one"), "{error}");

    // Its ELF header puts the section table far behind its end.
    let mut short = appimage("");
    short.resize(64, 0);
    short[40..48].copy_from_slice(&1_000_000u64.to_le_bytes()); // e_shoff
    short[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    short[60..62].copy_from_slice(&1u16.to_le_bytes()); // e_shnum
    let archive = scratch.archive("short.zip", &zip(&[entry("soh.appimage", &short)]));
    let error = install::unpack_archive(&archive, &out).unwrap_err().to_string();
    assert_eq!(
        error,
        format!(
            "{}: soh.appimage: it is cut short, a complete file is at least 1000064 bytes and \
             this one is 64",
            archive.display()
        )
    );

    // Whatever failed, nothing is left behind.
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
}
