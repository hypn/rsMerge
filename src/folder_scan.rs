//! Recursive comparison of two folders into a tree of entries.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

/// Size-and-date compare treats timestamps this close as equal (FAT has 2-second resolution).
const DATE_TOLERANCE: Duration = Duration::from_secs(2);
const COMPARE_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum CompareMethod {
    /// Byte-for-byte file contents.
    Contents,
    /// Same size and modification time.
    SizeAndDate,
}

#[derive(Clone)]
pub struct ScanOptions {
    pub method: CompareMethod,
    pub recursive: bool,
    /// Wildcard patterns (`*`, `?`) matched against file and folder names.
    pub exclude: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Identical,
    Different,
    LeftOnly,
    RightOnly,
    Error,
    /// A folder on both sides that wasn't descended into (subfolders off, or a symlink).
    NotScanned,
}

#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

pub struct Node {
    pub name: String,
    /// Path relative to both roots.
    pub rel: PathBuf,
    pub is_dir: bool,
    pub sides: [Option<Meta>; 2],
    pub status: Status,
    pub error: Option<String>,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub depth: usize,
    scanned: bool,
}

pub struct Tree {
    pub roots: [PathBuf; 2],
    /// Parents always come before their children.
    pub nodes: Vec<Node>,
    pub top: Vec<usize>,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

#[derive(Default)]
pub struct Progress {
    pub items: AtomicUsize,
    pub cancel: AtomicBool,
}

#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct Counts {
    pub identical: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
}

pub fn scan(left: &Path, right: &Path, options: &ScanOptions, progress: &Progress) -> Tree {
    let mut tree = Tree {
        roots: [left.to_path_buf(), right.to_path_buf()],
        nodes: Vec::new(),
        top: Vec::new(),
        errors: Vec::new(),
        cancelled: false,
    };
    tree.top = scan_dir(&mut tree, Path::new(""), [true, true], None, 0, options, progress);
    tree.cancelled = progress.cancel.load(Ordering::Relaxed);
    tree.update_folder_statuses();
    tree
}

struct Found {
    meta: Meta,
    symlink: bool,
}

fn scan_dir(
    tree: &mut Tree,
    rel: &Path,
    present: [bool; 2],
    parent: Option<usize>,
    depth: usize,
    options: &ScanOptions,
    progress: &Progress,
) -> Vec<usize> {
    // Folders first, then case-insensitive by name.
    let mut entries: BTreeMap<(bool, String, String), [Option<Found>; 2]> = BTreeMap::new();
    for (side, &here) in present.iter().enumerate() {
        if !here {
            continue;
        }
        let dir = tree.roots[side].join(rel);
        let reader = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("{}: {e}", dir.display());
                match parent {
                    Some(p) => tree.nodes[p].error = Some(msg),
                    None => tree.errors.push(msg),
                }
                continue;
            }
        };
        for entry in reader.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if options.exclude.iter().any(|p| wildcard_match(p, &name)) {
                continue;
            }
            let path = entry.path();
            let symlink = entry.file_type().is_ok_and(|t| t.is_symlink());
            // Follow symlinks for metadata, falling back to the link itself if it's broken.
            let Ok(md) = fs::metadata(&path).or_else(|_| entry.metadata()) else { continue };
            let meta = Meta { size: if md.is_dir() { 0 } else { md.len() }, modified: md.modified().ok() };
            let key = (!md.is_dir(), name.to_lowercase(), name);
            entries.entry(key).or_default()[side] = Some(Found { meta, symlink });
        }
    }

    let mut indices = Vec::with_capacity(entries.len());
    for ((not_dir, _, name), found) in entries {
        if progress.cancel.load(Ordering::Relaxed) {
            break;
        }
        progress.items.fetch_add(1, Ordering::Relaxed);
        let is_dir = !not_dir;
        let node_rel = rel.join(&name);
        let index = tree.nodes.len();
        let sides = [found[0].as_ref().map(|f| f.meta), found[1].as_ref().map(|f| f.meta)];
        let one_sided = match sides {
            [Some(_), None] => Some(Status::LeftOnly),
            [None, Some(_)] => Some(Status::RightOnly),
            _ => None,
        };
        tree.nodes.push(Node {
            name,
            rel: node_rel.clone(),
            is_dir,
            sides,
            status: one_sided.unwrap_or(Status::NotScanned),
            error: None,
            parent,
            children: Vec::new(),
            depth,
            scanned: false,
        });
        indices.push(index);

        if is_dir {
            let symlink = found.iter().flatten().any(|f| f.symlink);
            // Folders that exist on one side only are listed in full, so you can see what's missing.
            if (options.recursive || one_sided.is_some()) && !symlink {
                let present = [sides[0].is_some(), sides[1].is_some()];
                let children = scan_dir(tree, &node_rel, present, Some(index), depth + 1, options, progress);
                let node = &mut tree.nodes[index];
                node.children = children;
                node.scanned = true;
            }
        } else if one_sided.is_none() {
            tree.compare_file(index, options.method);
        }
    }
    indices
}

impl Tree {
    pub fn path(&self, side: usize, index: usize) -> PathBuf {
        self.roots[side].join(&self.nodes[index].rel)
    }

    fn compare_file(&mut self, index: usize, method: CompareMethod) {
        let (l, r) = (self.path(0, index), self.path(1, index));
        let node = &mut self.nodes[index];
        let [Some(lm), Some(rm)] = node.sides else { return };
        let result = match method {
            CompareMethod::SizeAndDate => Ok(lm.size == rm.size && same_time(lm.modified, rm.modified)),
            CompareMethod::Contents => {
                if lm.size == rm.size { same_contents(&l, &r).map_err(|e| e.to_string()) } else { Ok(false) }
            }
        };
        (node.status, node.error) = match result {
            Ok(true) => (Status::Identical, None),
            Ok(false) => (Status::Different, None),
            Err(e) => (Status::Error, Some(e)),
        };
    }

    /// Re-reads one file's metadata and contents, e.g. after it was edited and saved.
    pub fn recheck(&mut self, index: usize, method: CompareMethod) {
        if self.nodes[index].is_dir {
            return;
        }
        for side in 0..2 {
            let md = fs::metadata(self.path(side, index)).ok().filter(|m| m.is_file());
            self.nodes[index].sides[side] = md.map(|m| Meta { size: m.len(), modified: m.modified().ok() });
        }
        let node = &mut self.nodes[index];
        node.error = None;
        node.status = match node.sides {
            [Some(_), None] => Status::LeftOnly,
            [None, Some(_)] => Status::RightOnly,
            [None, None] => Status::Error,
            _ => Status::NotScanned,
        };
        if node.status == Status::Error {
            node.error = Some("Missing on both sides".into());
        } else if node.status == Status::NotScanned {
            self.compare_file(index, method);
        }
        // Saving a file that only existed on one side may have created its folders too.
        let mut parent = self.nodes[index].parent;
        while let Some(p) = parent {
            for side in 0..2 {
                let md = fs::metadata(self.path(side, p)).ok().filter(|m| m.is_dir());
                self.nodes[p].sides[side] = md.map(|m| Meta { size: 0, modified: m.modified().ok() });
            }
            parent = self.nodes[p].parent;
        }
        self.update_folder_statuses();
    }

    /// A folder is identical only if everything inside it is.
    fn update_folder_statuses(&mut self) {
        for i in (0..self.nodes.len()).rev() {
            let node = &self.nodes[i];
            if !node.is_dir {
                continue;
            }
            let status = match node.sides {
                [Some(_), None] => Status::LeftOnly,
                [None, Some(_)] => Status::RightOnly,
                _ if node.error.is_some() => Status::Error,
                _ if !node.scanned => Status::NotScanned,
                _ => {
                    let differs = node.children.iter().any(|&c| {
                        !matches!(self.nodes[c].status, Status::Identical | Status::NotScanned)
                    });
                    if differs { Status::Different } else { Status::Identical }
                }
            };
            self.nodes[i].status = status;
        }
    }

    /// File counts by status (folders aren't counted).
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for node in self.nodes.iter().filter(|n| !n.is_dir) {
            match node.status {
                Status::Identical => c.identical += 1,
                Status::Different | Status::Error => c.different += 1,
                Status::LeftOnly => c.left_only += 1,
                Status::RightOnly => c.right_only += 1,
                Status::NotScanned => {}
            }
        }
        c
    }
}

fn same_time(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let gap = a.duration_since(b).unwrap_or_else(|e| e.duration());
            gap <= DATE_TOLERANCE
        }
        _ => false,
    }
}

fn same_contents(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let mut ba = vec![0; COMPARE_CHUNK];
    let mut bb = vec![0; COMPARE_CHUNK];
    loop {
        let na = fill(&mut fa, &mut ba)?;
        let nb = fill(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

/// Reads until the buffer is full or the file ends.
fn fill(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Case-insensitive wildcard match supporting `*` and `?`.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Splits a comma/semicolon separated pattern list.
pub fn parse_patterns(text: &str) -> Vec<String> {
    text.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn find<'a>(tree: &'a Tree, rel: &str) -> &'a Node {
        tree.nodes.iter().find(|n| n.rel == Path::new(rel)).unwrap_or_else(|| panic!("{rel} not found"))
    }

    #[test]
    fn compares_trees() {
        let base = std::env::temp_dir().join(format!("rsmerge-folder-{}", std::process::id()));
        let (l, r) = (base.join("l"), base.join("r"));
        write(&l, "same.txt", "x");
        write(&r, "same.txt", "x");
        write(&l, "diff.txt", "a");
        write(&r, "diff.txt", "b");
        write(&l, "sub/only_left.txt", "1");
        write(&r, "sub/keep.txt", "k");
        write(&l, "sub/keep.txt", "k");
        write(&r, "new/deep/file.txt", "n");
        write(&l, ".git/HEAD", "ref");
        write(&r, ".git/HEAD", "other");

        let options = ScanOptions { method: CompareMethod::Contents, recursive: true, exclude: vec![".git".into()] };
        let tree = scan(&l, &r, &options, &Progress::default());
        assert_eq!(find(&tree, "same.txt").status, Status::Identical);
        assert_eq!(find(&tree, "diff.txt").status, Status::Different);
        assert_eq!(find(&tree, "sub/only_left.txt").status, Status::LeftOnly);
        assert_eq!(find(&tree, "sub").status, Status::Different);
        assert_eq!(find(&tree, "new").status, Status::RightOnly);
        assert_eq!(find(&tree, "new/deep/file.txt").status, Status::RightOnly);
        assert!(tree.nodes.iter().all(|n| n.name != ".git"));
        // Folders sort before files.
        assert!(tree.nodes[tree.top[0]].is_dir);
        assert_eq!(tree.counts(), Counts { identical: 2, different: 1, left_only: 1, right_only: 1 });

        let flat = ScanOptions { recursive: false, ..options };
        let tree = scan(&l, &r, &flat, &Progress::default());
        assert_eq!(find(&tree, "sub").status, Status::NotScanned);

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.tmp", "a.TMP"));
        assert!(wildcard_match("node_modules", "node_modules"));
        assert!(wildcard_match("a?c*", "abcdef"));
        assert!(!wildcard_match("*.tmp", "a.txt"));
        assert_eq!(parse_patterns(" .git, *.tmp ;; x "), vec![".git", "*.tmp", "x"]);
    }
}
