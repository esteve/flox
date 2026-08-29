//! The project-level catalog lock.
//!
//! A project has exactly one catalog lock — `.flox/catalog.lock` — pinning
//! the source of every `catalogs.*` reference made by the project's Nix
//! expressions. A single lock means a single revision always evaluates
//! against one consistent set of inputs, leaving no room for diamond
//! dependency conflicts between packages of the same project.
//!
//! The CLI owns the lock's entire lifecycle; the package builder only
//! passes the file it is handed (`CATALOG_LOCKFILE`) through to the NEF
//! evals. The committed lock is created explicitly (see
//! [lock_project_catalog], locking the union of every expression's
//! references), and consumed by builds exactly as found — deliberately
//! including one that no longer covers the expressions' references, in
//! which case the NEF eval fails and the user recreates the lock
//! explicitly. Without a committed lock the project builds locklessly:
//! [BuildCatalogLock::ensure] resolves a fresh ephemeral lock into a temp
//! file that lives only as long as the build, and nothing is ever written
//! into the project tree. Publish never creates the committed lock: it
//! projects the subset relevant to the one package being published out of
//! whichever lock the build consumes, and submits that subset with the
//! build.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use floxhub_client::{CatalogClientTrait, LockedInputEntry};
use nef_lock_catalog::{
    BuildLock,
    CatalogRef,
    LockError,
    LockfileError,
    ScanError,
    StaleLockError,
    lock_references,
    read_lock,
    render_unresolvable,
    scan_package,
    write_lock,
};
use thiserror::Error;
use tracing::debug;

/// File name of the project catalog lock, relative to the `.flox` directory.
pub const CATALOG_LOCKFILE_NAME: &str = "catalog.lock";

/// The location of the project catalog lock within `dot_flox_path`.
pub fn catalog_lockfile_path(dot_flox_path: impl AsRef<Path>) -> PathBuf {
    dot_flox_path.as_ref().join(CATALOG_LOCKFILE_NAME)
}

#[derive(Debug, Error)]
pub enum CatalogLockError {
    #[error(transparent)]
    Scan(#[from] ScanError),

    #[error(transparent)]
    Lock(#[from] LockError),

    /// Unresolvable references, pre-rendered with their dependency chains
    /// and the remediation footer (see [render_unresolvable]) so every CLI
    /// entry point that locks — build, publish, update-catalogs — reports
    /// the reference names and a next step rather than a bare count.
    #[error("{0}")]
    Unresolvable(String),

    #[error(transparent)]
    StaleLock(#[from] StaleLockError),

    #[error(transparent)]
    Lockfile(#[from] LockfileError),

    #[error("Could not create a temporary file for the catalog lock.")]
    CreateTempFile(#[source] std::io::Error),
}

/// Resolve `references` through the catalog, or produce an empty lock
/// without any request when there are none — the common case for projects
/// whose expressions make no catalog references.
async fn resolve_lock(
    client: &(impl CatalogClientTrait + Send + Sync),
    references: BTreeSet<CatalogRef>,
) -> Result<BuildLock, CatalogLockError> {
    if references.is_empty() {
        return Ok(BuildLock::default());
    }
    match lock_references(client, references).await {
        Ok(lock) => Ok(lock),
        Err(LockError::Unresolvable(entries)) => Err(CatalogLockError::Unresolvable(
            render_unresolvable(&entries),
        )),
        Err(err) => Err(err.into()),
    }
}

/// Scan every expression named by `rel_file_paths` (relative to
/// `expressions_dir`) and return the union of their catalog references.
fn scan_references(
    expressions_dir: impl AsRef<Path>,
    rel_file_paths: impl IntoIterator<Item = impl AsRef<Path>>,
) -> Result<BTreeSet<CatalogRef>, CatalogLockError> {
    let expressions_dir = expressions_dir.as_ref();
    let mut references = BTreeSet::new();
    for rel_file_path in rel_file_paths {
        references.extend(scan_package(expressions_dir, rel_file_path)?);
    }
    Ok(references)
}

/// Create (or re-create) the project catalog lock at `lockfile_path`.
///
/// Scans every expression named by `rel_file_paths` (relative to
/// `expressions_dir`) and locks the union of their catalog references in a
/// single request, so the resulting lock is internally consistent by
/// construction. Returns the scanned references.
pub async fn lock_project_catalog(
    client: &(impl CatalogClientTrait + Send + Sync),
    expressions_dir: impl AsRef<Path>,
    rel_file_paths: impl IntoIterator<Item = impl AsRef<Path>>,
    lockfile_path: impl AsRef<Path>,
) -> Result<BTreeSet<CatalogRef>, CatalogLockError> {
    let references = scan_references(expressions_dir, rel_file_paths)?;
    let lock = resolve_lock(client, references.clone()).await?;
    write_lock(&lock, &lockfile_path)?;
    debug!(
        path = %lockfile_path.as_ref().display(),
        references = references.len(),
        "wrote project catalog lock"
    );
    Ok(references)
}

/// The catalog references one package's expression makes, resolved through
/// its imports and dependency arguments. Publish scans locally — no build,
/// no network — to learn which lock entries are relevant to the package.
pub fn package_references(
    expressions_dir: impl AsRef<Path>,
    rel_file_path: impl AsRef<Path>,
) -> Result<BTreeSet<CatalogRef>, CatalogLockError> {
    Ok(scan_package(expressions_dir, rel_file_path)?)
}

/// The lock a build consumes, created before the package builder is
/// invoked and handed to it as `CATALOG_LOCKFILE`.
#[derive(Debug)]
pub struct BuildCatalogLock {
    path: PathBuf,
    lock: BuildLock,
    /// Keeps an ephemeral lock's temp file alive for as long as this value;
    /// `None` when the lock is the committed file.
    _ephemeral: Option<tempfile::TempPath>,
}

impl BuildCatalogLock {
    /// The committed `.flox/catalog.lock` exactly as found when one exists;
    /// otherwise a fresh ephemeral lock resolving the union of the
    /// references of the expressions named by `rel_file_paths`, written to
    /// a randomly named temp file under `temp_dir` that is removed when the
    /// returned value is dropped.
    pub async fn ensure(
        client: &(impl CatalogClientTrait + Send + Sync),
        dot_flox_path: impl AsRef<Path>,
        expressions_dir: impl AsRef<Path>,
        rel_file_paths: impl IntoIterator<Item = impl AsRef<Path>>,
        temp_dir: impl AsRef<Path>,
    ) -> Result<BuildCatalogLock, CatalogLockError> {
        let committed = catalog_lockfile_path(&dot_flox_path);
        if committed.exists() {
            let lock = read_lock(&committed)?;
            debug!(path = %committed.display(), "build consumes the committed catalog lock");
            return Ok(BuildCatalogLock {
                path: committed,
                lock,
                _ephemeral: None,
            });
        }

        let references = scan_references(expressions_dir, rel_file_paths)?;
        let lock = resolve_lock(client, references).await?;
        let temp_path = tempfile::Builder::new()
            .prefix("catalog.lock.")
            .tempfile_in(temp_dir)
            .map_err(CatalogLockError::CreateTempFile)?
            .into_temp_path();
        write_lock(&lock, &temp_path)?;
        debug!(path = %temp_path.display(), "build consumes a fresh ephemeral catalog lock");
        Ok(BuildCatalogLock {
            path: temp_path.to_path_buf(),
            lock,
            _ephemeral: Some(temp_path),
        })
    }

    /// The path to hand to the package builder as `CATALOG_LOCKFILE`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The direct-input subset of this lock selected by `references` — what
    /// a publish submits with the build. Empty `references` yield an empty
    /// map. Fails with [StaleLockError] naming the uncovered references
    /// when the lock predates them, which can only happen for the committed
    /// lock: an ephemeral lock is resolved from the same scan.
    pub fn subset(
        &self,
        references: &BTreeSet<CatalogRef>,
    ) -> Result<BTreeMap<String, LockedInputEntry>, StaleLockError> {
        if references.is_empty() {
            return Ok(BTreeMap::new());
        }
        self.lock.subset_direct(references)
    }

    /// Whether this is the committed `.flox/catalog.lock` rather than an
    /// ephemeral lock, e.g. to select stale-lock messaging.
    pub fn is_committed(&self) -> bool {
        self._ephemeral.is_none()
    }
}

pub mod test_helpers {
    use super::*;

    /// Construct a [BuildCatalogLock] from parts, for tests that need to
    /// exercise consumers (e.g. publish's stale-lock messaging) without a
    /// scan or a catalog round-trip.
    pub fn build_catalog_lock_from_parts(
        path: impl Into<PathBuf>,
        lock: BuildLock,
        committed: bool,
    ) -> BuildCatalogLock {
        BuildCatalogLock {
            path: path.into(),
            lock,
            _ephemeral: match committed {
                true => None,
                false => Some(
                    tempfile::NamedTempFile::new()
                        .expect("temp file for test lock")
                        .into_temp_path(),
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use floxhub_client::client::test_helpers::new_noop;
    use tempfile::tempdir;

    use super::*;

    /// A committed lock with one canonical entry, plus an expression that
    /// references it.
    const COMMITTED_LOCK: &str = r#"{
  "version": 1,
  "direct_catalog_inputs": {
    "myorg/hello": {
      "attr_path": ["hello"],
      "build_type": "nef",
      "catalog": "myorg",
      "locked_inputs_hash": "sha256-test",
      "source": {
        "dir": ".",
        "ref": "refs/heads/main",
        "rev": "0000000000000000000000000000000000000000",
        "type": "git",
        "url": "https://example.com/repo"
      }
    }
  },
  "catalogs": {}
}
"#;

    fn project_with_expression(expression: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let project = tempdir().unwrap();
        let dot_flox = project.path().join(".flox");
        let pkgs_dir = dot_flox.join("pkgs");
        std::fs::create_dir_all(&pkgs_dir).unwrap();
        std::fs::write(pkgs_dir.join("hello.nix"), expression).unwrap();
        (project, dot_flox, pkgs_dir)
    }

    #[test]
    fn no_references_scan_empty() {
        let expressions_dir = tempdir().unwrap();
        std::fs::write(
            expressions_dir.path().join("hello.nix"),
            "{ runCommand }: runCommand \"hello\" { } \"\"",
        )
        .unwrap();

        let references = package_references(expressions_dir.path(), "hello.nix").unwrap();
        assert_eq!(references, BTreeSet::new());
    }

    /// A project whose expressions make no catalog references resolves an
    /// empty ephemeral lock without any catalog request: the no-op client
    /// fails every request it is asked to make, so reaching the network at
    /// all fails this test.
    #[tokio::test]
    async fn no_references_resolve_without_a_catalog_request() {
        let (_project, dot_flox, pkgs_dir) =
            project_with_expression("{ runCommand }: runCommand \"hello\" { } \"\"");
        let temp_dir = tempdir().unwrap();

        let lock = BuildCatalogLock::ensure(
            &new_noop(),
            &dot_flox,
            &pkgs_dir,
            ["hello.nix"],
            temp_dir.path(),
        )
        .await
        .unwrap();

        assert!(!lock.is_committed());
        assert_eq!(lock.subset(&BTreeSet::new()).unwrap(), BTreeMap::new());
        assert_eq!(
            std::fs::read_to_string(lock.path()).unwrap(),
            "{\n  \"version\": 1,\n  \"direct_catalog_inputs\": {},\n  \"catalogs\": {}\n}\n"
        );
    }

    /// A committed lock is consumed exactly as found: no catalog request
    /// (no-op client), no rewrite (byte-identical file), and the subset
    /// selects the committed entry by the scanned reference.
    #[tokio::test]
    async fn committed_lock_is_consumed_as_found_without_a_catalog_request() {
        let (_project, dot_flox, pkgs_dir) =
            project_with_expression("{ catalogs }: catalogs.myorg.hello");
        std::fs::write(catalog_lockfile_path(&dot_flox), COMMITTED_LOCK).unwrap();
        let temp_dir = tempdir().unwrap();

        let lock = BuildCatalogLock::ensure(
            &new_noop(),
            &dot_flox,
            &pkgs_dir,
            ["hello.nix"],
            temp_dir.path(),
        )
        .await
        .unwrap();

        assert!(lock.is_committed());
        assert_eq!(lock.path(), catalog_lockfile_path(&dot_flox));
        assert_eq!(
            std::fs::read_to_string(catalog_lockfile_path(&dot_flox)).unwrap(),
            COMMITTED_LOCK,
            "the committed lock must not be rewritten"
        );

        let references = package_references(&pkgs_dir, "hello.nix").unwrap();
        let subset = lock.subset(&references).unwrap();
        assert_eq!(subset.keys().collect::<Vec<_>>(), vec![
            &"myorg/hello".to_string()
        ]);
    }

    /// A committed lock that does not cover a scanned reference is still
    /// consumed as found; the staleness surfaces from the subset, naming
    /// the uncovered reference.
    #[tokio::test]
    async fn stale_committed_lock_names_the_uncovered_reference() {
        let (_project, dot_flox, pkgs_dir) =
            project_with_expression("{ catalogs }: catalogs.myorg.world");
        std::fs::write(catalog_lockfile_path(&dot_flox), COMMITTED_LOCK).unwrap();
        let temp_dir = tempdir().unwrap();

        let lock = BuildCatalogLock::ensure(
            &new_noop(),
            &dot_flox,
            &pkgs_dir,
            ["hello.nix"],
            temp_dir.path(),
        )
        .await
        .unwrap();
        assert!(lock.is_committed());

        let references = package_references(&pkgs_dir, "hello.nix").unwrap();
        let err = lock
            .subset(&references)
            .expect_err("an uncovered reference must be stale");
        assert!(
            err.to_string().contains("myorg.world"),
            "the uncovered reference must be named, got: {err}"
        );
    }

    /// `lock_project_catalog` writes the committed lock file; with no
    /// references it does so without any catalog request.
    #[tokio::test]
    async fn lock_project_catalog_writes_an_empty_lock_without_a_catalog_request() {
        let (_project, dot_flox, pkgs_dir) =
            project_with_expression("{ runCommand }: runCommand \"hello\" { } \"\"");
        let lockfile_path = catalog_lockfile_path(&dot_flox);

        let references =
            lock_project_catalog(&new_noop(), &pkgs_dir, ["hello.nix"], &lockfile_path)
                .await
                .unwrap();

        assert_eq!(references, BTreeSet::new());
        assert_eq!(
            std::fs::read_to_string(&lockfile_path).unwrap(),
            "{\n  \"version\": 1,\n  \"direct_catalog_inputs\": {},\n  \"catalogs\": {}\n}\n"
        );
    }
}
