//! Locating the chunk storage of a world.
//!
//! Two on-disk layouts exist:
//!
//! * **Classic** (up to 1.21.x): the overworld lives directly in the world
//!   folder (`region/`, `poi/`, `entities/`), the Nether in `DIM-1/`, the End
//!   in `DIM1/` and custom dimensions in `dimensions/<namespace>/<path>/`.
//! * **26.1 and newer**: every dimension, including the vanilla ones, lives
//!   in `dimensions/<namespace>/<path>/`.

use anyhow::{Result, bail};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// The chunk storage directories of one dimension.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dimension {
    /// Dimension id such as `minecraft:overworld`, or the folder path if the
    /// dimension was given as a folder.
    pub name: String,
    pub region: PathBuf,
    pub poi: Option<PathBuf>,
    pub entities: Option<PathBuf>,
}

impl Dimension {
    /// Use `dir` as a dimension folder if it has a `region` directory.
    pub fn from_dir(name: impl Into<String>, dir: &Path) -> Option<Dimension> {
        let region = dir.join("region");
        if !region.is_dir() {
            return None;
        }
        let optional = |name: &str| Some(dir.join(name)).filter(|p| p.is_dir());
        Some(Dimension { name: name.into(), region, poi: optional("poi"), entities: optional("entities") })
    }
}

/// Split a dimension id into namespace and path. The namespace defaults to
/// `minecraft`.
fn parse_dimension_id(id: &str) -> Result<(&str, &str)> {
    let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
    let valid =
        |s: &str, extra: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.".contains(c) || extra.contains(c));
    // `..` segments must not escape the world folder
    if !valid(namespace, "") || !valid(path, "/") || path.split('/').any(|part| part.is_empty() || part.chars().all(|c| c == '.')) {
        bail!("invalid dimension id \"{id}\"");
    }
    Ok((namespace, path))
}

/// Find the storage of the dimension `id` in the world folder `world`.
fn find_dimension(world: &Path, id: &str) -> Result<Dimension> {
    let (namespace, path) = parse_dimension_id(id)?;
    let name = format!("{namespace}:{path}");
    // The 26.1+ location comes first: an upgraded world may still contain
    // stale folders of the classic layout.
    let mut candidates = vec![world.join("dimensions").join(namespace).join(path)];
    match name.as_str() {
        "minecraft:overworld" => candidates.push(world.to_path_buf()),
        "minecraft:the_nether" => candidates.push(world.join("DIM-1")),
        "minecraft:the_end" => candidates.push(world.join("DIM1")),
        _ => {}
    }
    for dir in &candidates {
        if let Some(dimension) = Dimension::from_dir(name.clone(), dir) {
            return Ok(dimension);
        }
    }
    bail!(
        "could not find dimension {name} in \"{}\" (no region folder in {})",
        world.display(),
        candidates.iter().map(|c| format!("\"{}\"", c.display())).collect::<Vec<_>>().join(" or ")
    );
}

/// Every dimension stored in a world folder, in a stable order.
fn find_all_dimensions(world: &Path) -> Vec<Dimension> {
    let mut found: Vec<Dimension> = Vec::new();
    let mut add = |dimension: Option<Dimension>| {
        if let Some(d) = dimension
            && !found.iter().any(|f| f.name == d.name)
        {
            found.push(d);
        }
    };
    // `dimensions/<namespace>/<path...>`: a path may have several segments.
    fn walk(dir: &Path, namespace: &str, path: &str, depth: u32, add: &mut dyn FnMut(Option<Dimension>)) {
        if !path.is_empty() {
            add(Dimension::from_dir(format!("{namespace}:{path}"), dir));
        }
        if depth >= 8 {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut children: Vec<_> = entries.flatten().filter(|e| e.path().is_dir()).collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let Some(name) = child.file_name().to_str().map(str::to_owned) else { continue };
            if !path.is_empty() && matches!(name.as_str(), "region" | "poi" | "entities" | "data") {
                continue;
            }
            let child_path = if path.is_empty() { name } else { format!("{path}/{name}") };
            walk(&child.path(), namespace, &child_path, depth + 1, add);
        }
    }
    if let Ok(namespaces) = fs::read_dir(world.join("dimensions")) {
        let mut namespaces: Vec<_> = namespaces.flatten().filter(|e| e.path().is_dir()).collect();
        namespaces.sort_by_key(|e| e.file_name());
        for namespace in namespaces {
            if let Some(name) = namespace.file_name().to_str() {
                walk(&namespace.path(), name, "", 0, &mut add);
            }
        }
    }
    add(Dimension::from_dir("minecraft:overworld", world));
    add(Dimension::from_dir("minecraft:the_nether", &world.join("DIM-1")));
    add(Dimension::from_dir("minecraft:the_end", &world.join("DIM1")));
    found
}

/// Work out which dimensions to prune.
///
/// * Without `dimensions`, `path` may be a world folder (then the overworld
///   is used, wherever the world's format keeps it) or any folder containing
///   a `region` directory, such as `world/DIM-1`.
/// * With `dimensions`, `path` must be a world folder and each entry is a
///   dimension id (`the_nether`, `minecraft:the_end`, `mymod:mine`) or `all`.
pub fn resolve_dimensions(path: &Path, dimensions: &[String]) -> Result<Vec<Dimension>> {
    if !path.is_dir() {
        bail!("could not find world at \"{}\"", path.display());
    }
    if dimensions.is_empty() {
        let is_world = path.join("level.dat").is_file() || path.join("dimensions").is_dir();
        if is_world && let Ok(overworld) = find_dimension(path, "minecraft:overworld") {
            return Ok(vec![overworld]);
        }
        return match Dimension::from_dir(path.display().to_string(), path) {
            Some(dimension) => Ok(vec![dimension]),
            None if is_world => bail!("could not find the overworld in \"{}\"; use --dimension to choose a dimension", path.display()),
            None => bail!("could not find world at \"{}\" (it has no region folder)", path.display()),
        };
    }

    let mut result: Vec<Dimension> = Vec::new();
    for id in dimensions {
        let found = if id == "all" {
            let all = find_all_dimensions(path);
            if all.is_empty() {
                bail!("could not find any dimension in \"{}\"", path.display());
            }
            all
        } else {
            vec![find_dimension(path, id)?]
        };
        for dimension in found {
            if !result.iter().any(|d| d.region == dimension.region) {
                result.push(dimension);
            }
        }
    }
    Ok(result)
}

/// Holds the world's `session.lock` while pruning, like the game does while
/// a world is open.
pub struct SessionLock {
    _file: Option<File>,
}

/// Outcome of trying to lock a world.
pub enum LockResult {
    /// The world is not in use (and now locked), or it has no lock file.
    Acquired(SessionLock),
    /// Another process, most likely the game or a server, has the world open.
    InUse(PathBuf),
    /// The lock could not be checked.
    Unknown(PathBuf, std::io::Error),
}

/// Find `session.lock` in the folder itself or one of its parents (a
/// dimension folder is up to four levels below the world folder).
fn find_session_lock(start: &Path) -> Option<PathBuf> {
    let start = fs::canonicalize(start).ok()?;
    start.ancestors().take(5).map(|dir| dir.join("session.lock")).find(|lock| lock.is_file())
}

/// Try to take the session lock of the world that `path` belongs to.
pub fn lock_world(path: &Path) -> LockResult {
    let Some(lock_path) = find_session_lock(path) else {
        return LockResult::Acquired(SessionLock { _file: None });
    };
    let file = match File::options().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(e) => return LockResult::Unknown(lock_path, e),
    };
    match try_lock(&file) {
        Ok(true) => LockResult::Acquired(SessionLock { _file: Some(file) }),
        Ok(false) => LockResult::InUse(lock_path),
        Err(e) => LockResult::Unknown(lock_path, e),
    }
}

/// The game locks the file through Java's `FileChannel.lock()`, which is a
/// POSIX record lock on Unix. `File::try_lock` would use `flock`, which does
/// not interact with record locks, so `fcntl` is used here.
#[cfg(unix)]
fn try_lock(file: &File) -> std::io::Result<bool> {
    use rustix::fs::{FlockOperation, fcntl_lock};
    use rustix::io::Errno;
    match fcntl_lock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(Errno::AGAIN) | Err(Errno::ACCESS) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// On Windows both Java and the standard library use `LockFileEx`.
#[cfg(not(unix))]
fn try_lock(file: &File) -> std::io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region::testing::TempDir;

    fn mkdirs(root: &Path, dirs: &[&str]) {
        for dir in dirs {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
    }

    fn names(dimensions: &[Dimension]) -> Vec<&str> {
        dimensions.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn classic_layout() {
        let tmp = TempDir::new("classic");
        let w = tmp.path();
        mkdirs(w, &["region", "poi", "entities", "DIM-1/region", "DIM-1/poi", "DIM1/region", "dimensions/mymod/mine/region", "data"]);
        fs::write(w.join("level.dat"), b"").unwrap();

        let d = resolve_dimensions(w, &[]).unwrap();
        assert_eq!(names(&d), ["minecraft:overworld"]);
        assert_eq!(d[0].region, w.join("region"));
        assert_eq!(d[0].poi, Some(w.join("poi")));
        assert_eq!(d[0].entities, Some(w.join("entities")));

        let d = resolve_dimensions(w, &["the_nether".into()]).unwrap();
        assert_eq!(d[0].region, w.join("DIM-1").join("region"));
        assert_eq!(d[0].poi, Some(w.join("DIM-1").join("poi")));
        assert_eq!(d[0].entities, None);

        let d = resolve_dimensions(w, &["minecraft:the_end".into(), "mymod:mine".into(), "the_end".into()]).unwrap();
        assert_eq!(names(&d), ["minecraft:the_end", "mymod:mine"]);

        let d = resolve_dimensions(w, &["all".into()]).unwrap();
        assert_eq!(names(&d), ["mymod:mine", "minecraft:overworld", "minecraft:the_nether", "minecraft:the_end"]);

        // a dimension folder can be given directly, like with the original tool
        let d = resolve_dimensions(&w.join("DIM-1"), &[]).unwrap();
        assert_eq!(d[0].region, w.join("DIM-1").join("region"));

        assert!(resolve_dimensions(w, &["minecraft:nope".into()]).is_err());
        assert!(resolve_dimensions(w, &["../x".into()]).is_err());
        assert!(resolve_dimensions(w, &["a:b/../../c".into()]).is_err());
        assert!(resolve_dimensions(&w.join("data"), &[]).is_err());
        assert!(resolve_dimensions(&w.join("missing"), &[]).is_err());
    }

    #[test]
    fn layout_26_1() {
        let tmp = TempDir::new("modern");
        let w = tmp.path();
        mkdirs(
            w,
            &[
                "dimensions/minecraft/overworld/region",
                "dimensions/minecraft/overworld/entities",
                "dimensions/minecraft/the_nether/region",
                "dimensions/minecraft/the_end/region",
                "dimensions/mymod/deep/mine/region",
                "dimensions/mymod/deep/mine/data",
                "players/data",
            ],
        );
        fs::write(w.join("level.dat"), b"").unwrap();

        let d = resolve_dimensions(w, &[]).unwrap();
        assert_eq!(names(&d), ["minecraft:overworld"]);
        assert_eq!(d[0].region, w.join("dimensions/minecraft/overworld/region"));
        assert_eq!(d[0].poi, None);
        assert_eq!(d[0].entities, Some(w.join("dimensions/minecraft/overworld/entities")));

        let d = resolve_dimensions(w, &["the_nether".into(), "mymod:deep/mine".into()]).unwrap();
        assert_eq!(names(&d), ["minecraft:the_nether", "mymod:deep/mine"]);

        let d = resolve_dimensions(w, &["all".into()]).unwrap();
        assert_eq!(names(&d), ["minecraft:overworld", "minecraft:the_end", "minecraft:the_nether", "mymod:deep/mine"]);

        // a leftover classic region folder of an upgraded world is not used
        mkdirs(w, &["region"]);
        let d = resolve_dimensions(w, &[]).unwrap();
        assert_eq!(d[0].region, w.join("dimensions/minecraft/overworld/region"));
        assert_eq!(resolve_dimensions(w, &["all".into()]).unwrap().len(), 4);

        let d = resolve_dimensions(&w.join("dimensions/minecraft/the_end"), &[]).unwrap();
        assert_eq!(d[0].region, w.join("dimensions/minecraft/the_end/region"));
    }

    #[test]
    fn session_lock() {
        let tmp = TempDir::new("lock");
        let w = tmp.path();
        mkdirs(w, &["dimensions/minecraft/overworld/region"]);
        assert!(matches!(lock_world(w), LockResult::Acquired(_)));
        fs::write(w.join("session.lock"), "\u{2603}").unwrap();
        let deep = w.join("dimensions/minecraft/overworld");
        assert_eq!(find_session_lock(&deep), Some(fs::canonicalize(w).unwrap().join("session.lock")));
        let lock = lock_world(&deep);
        assert!(matches!(lock, LockResult::Acquired(SessionLock { _file: Some(_) })));
    }
}
