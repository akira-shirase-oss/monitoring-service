//! N3 ruling: an applied migration's bytes are its identity — sqlx records a
//! checksum when a database applies the file, and an edited file makes every
//! such database refuse the next boot. A migration that any database may have
//! applied is therefore immutable: fixes land as a NEW numbered file. The
//! committed manifest is the tripwire — an in-place edit or an unrecorded file
//! can never pass this gate.
//!
//! Regenerate ONLY when ADDING a migration (never to bless an edit):
//!     (cd crates/monitor-db/migrations && shasum -a 256 -- *.sql) \
//!         > ../migrations.sha256

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

#[test]
fn every_migration_matches_its_committed_checksum() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations.sha256");
    let manifest = fs::read_to_string(&manifest_path).expect("the committed manifest");
    let mut expected: BTreeMap<String, String> = BTreeMap::new();
    let mut versions: BTreeMap<String, String> = BTreeMap::new();
    for line in manifest
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
    {
        let (sum, name) = line
            .split_once("  ")
            .unwrap_or_else(|| panic!("manifest line is not `sha256  name`: {line:?}"));
        let name = name.trim();
        // N-3 re-review: a name must appear exactly once — collecting into a
        // map made the LAST line win, so an appended shadow line could bless
        // an in-place edit while the base line stayed verbatim. And one
        // version number is one file: a renamed file under the same version
        // prefix is the same applied bytes under a new name.
        assert!(
            expected
                .insert(name.to_owned(), sum.trim().to_owned())
                .is_none(),
            "the manifest names {name} twice — an appended shadow line may never \
             re-bless an existing migration"
        );
        let version = name
            .split('_')
            .next()
            .unwrap_or_else(|| panic!("unnamed migration line: {line:?}"));
        assert!(
            versions
                .insert(version.to_owned(), name.to_owned())
                .is_none(),
            "two manifest lines share version prefix {version} ({name}) — one \
             applied version is one file's bytes, whatever it is renamed to"
        );
    }

    let mut files: Vec<String> = fs::read_dir(&dir)
        .expect("the migrations directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
        })
        .collect();
    files.sort();
    for name in &files {
        let bytes = fs::read(dir.join(name)).expect("read the migration");
        let sum = Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut out, byte| {
                use std::fmt::Write as _;
                let _ = write!(out, "{byte:02x}");
                out
            });
        assert_eq!(
            expected.get(name).map(String::as_str),
            Some(sum.as_str()),
            "{name} does not match the committed checksum — an applied migration \
             may never be edited; land the fix as a NEW numbered file and \
             regenerate migrations.sha256"
        );
    }
    let names: Vec<&String> = expected.keys().collect();
    assert_eq!(
        names,
        files.iter().collect::<Vec<_>>(),
        "the manifest must cover exactly the migration files"
    );
}
