use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn slug(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub trait Dirs {
    fn is_dir(&self, path: &Path) -> bool;
    fn child_dirs(&self, path: &Path) -> Vec<String>;
}

pub struct RealDirs;

impl Dirs for RealDirs {
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn child_dirs(&self, path: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(path) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            // `path()` + `is_dir` follows symlinks; `file_type` would not.
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderState {
    Ok,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub path: PathBuf,
    pub state: FolderState,
}

pub fn resolve(
    project_folder: &str,
    last_cwd: Option<&str>,
    home: &Path,
    dirs: &dyn Dirs,
) -> Folder {
    if let Some(cwd) = last_cwd {
        let cwd = Path::new(cwd);
        if dirs.is_dir(cwd) {
            return Folder {
                path: cwd.to_path_buf(),
                state: FolderState::Ok,
            };
        }
    }

    let rest = project_folder.strip_prefix('-').unwrap_or(project_folder);
    let mut deepest = Deepest {
        consumed: 0,
        path: None,
    };
    let root = Path::new("/");
    match walk(root, &rest.to_ascii_lowercase(), 0, dirs, &mut deepest) {
        Some(path) => Folder {
            path,
            state: FolderState::Ok,
        },
        None => Folder {
            path: deepest.path.unwrap_or_else(|| home.to_path_buf()),
            state: FolderState::Gone,
        },
    }
}

/// `resolve` walks the filesystem from `/`, and the threads of one project share their inputs.
pub struct FolderCache<'a> {
    home: &'a Path,
    dirs: &'a dyn Dirs,
    found: HashMap<(String, Option<String>), Folder>,
}

impl<'a> FolderCache<'a> {
    pub fn new(home: &'a Path, dirs: &'a dyn Dirs) -> Self {
        FolderCache { home, dirs, found: HashMap::new() }
    }

    pub fn is_dir(&self, path: &Path) -> bool {
        self.dirs.is_dir(path)
    }

    pub fn resolve(&mut self, project_folder: &str, last_cwd: Option<&str>) -> Folder {
        let (home, dirs) = (self.home, self.dirs);
        self.found
            .entry((project_folder.to_string(), last_cwd.map(String::from)))
            .or_insert_with_key(|(project, cwd)| resolve(project, cwd.as_deref(), home, dirs))
            .clone()
    }
}

struct Deepest {
    consumed: usize,
    path: Option<PathBuf>,
}

/// `rest` is the lowercased slug still to match below `dir`. A name's slug can
/// contain `-` (from `_`, `.`, spaces or a literal `-`), so the split between
/// folder names is ambiguous and every candidate child must be tried. Each step
/// consumes at least one character, which bounds the recursion depth.
fn walk(
    dir: &Path,
    rest: &str,
    consumed: usize,
    dirs: &dyn Dirs,
    deepest: &mut Deepest,
) -> Option<PathBuf> {
    let mut children: Vec<(String, String)> = dirs
        .child_dirs(dir)
        .into_iter()
        .map(|name| {
            let child_slug = slug(&name).to_ascii_lowercase();
            (name, child_slug)
        })
        .filter(|(_, s)| !s.is_empty())
        .collect();
    children.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    for (name, child_slug) in children {
        let path = dir.join(&name);
        if child_slug == rest {
            return Some(path);
        }
        let Some(after) = rest
            .strip_prefix(child_slug.as_str())
            .and_then(|r| r.strip_prefix('-'))
        else {
            continue;
        };
        let used = consumed + child_slug.len() + 1;
        if used > deepest.consumed {
            deepest.consumed = used;
            deepest.path = Some(path.clone());
        }
        if let Some(found) = walk(&path, after, used, dirs, deepest) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    struct FakeDirs(HashSet<PathBuf>);

    impl FakeDirs {
        fn new(paths: &[&str]) -> Self {
            let mut set = HashSet::new();
            for p in paths {
                let mut cur = PathBuf::new();
                for part in Path::new(p).components() {
                    cur.push(part);
                    set.insert(cur.clone());
                }
            }
            FakeDirs(set)
        }
    }

    impl Dirs for FakeDirs {
        fn is_dir(&self, path: &Path) -> bool {
            self.0.contains(path)
        }

        fn child_dirs(&self, path: &Path) -> Vec<String> {
            self.0
                .iter()
                .filter(|p| p.parent() == Some(path))
                .filter_map(|p| p.file_name()?.to_str().map(String::from))
                .collect()
        }
    }

    fn home() -> PathBuf {
        PathBuf::from("/Users/Demo")
    }

    fn ok(path: &str) -> Folder {
        Folder {
            path: PathBuf::from(path),
            state: FolderState::Ok,
        }
    }

    fn gone(path: &str) -> Folder {
        Folder {
            path: PathBuf::from(path),
            state: FolderState::Gone,
        }
    }

    #[test]
    fn slug_replaces_non_alphanumerics() {
        assert_eq!(
            slug("/Users/Demo/Dev/Work/my_app"),
            "-Users-Demo-Dev-Work-my-app"
        );
        assert_eq!(slug("/a/.claude/worktrees/x"), "-a--claude-worktrees-x");
    }

    #[test]
    fn existing_last_cwd_wins_even_when_slug_differs() {
        let dirs = FakeDirs::new(&["/Users/Demo/Other", "/Users/Demo/Dev/Work/my_app"]);
        let got = resolve(
            "-Users-Demo-Dev-Work-my-app",
            Some("/Users/Demo/Other"),
            &home(),
            &dirs,
        );
        assert_eq!(got, ok("/Users/Demo/Other"));
    }

    #[test]
    fn moved_tree_is_found_by_slug() {
        let dirs = FakeDirs::new(&["/Users/Demo/Dev/Work/my_app"]);
        let got = resolve(
            "-Users-Demo-Dev-Work-my-app",
            Some("/Users/Demo/Desktop/Work/Dev/my_app"),
            &home(),
            &dirs,
        );
        assert_eq!(got, ok("/Users/Demo/Dev/Work/my_app"));
    }

    #[test]
    fn slug_comparison_ignores_case_and_returns_real_names() {
        let dirs = FakeDirs::new(&["/Users/Demo/Dev/Work/my_app"]);
        let got = resolve("-Users-demo-Dev-Work-my-app", None, &home(), &dirs);
        assert_eq!(got, ok("/Users/Demo/Dev/Work/my_app"));
    }

    #[test]
    fn backtracks_from_a_shorter_sibling() {
        let dirs = FakeDirs::new(&["/Users/Demo/Dev/Work/my", "/Users/Demo/Dev/Work/my_app"]);
        let got = resolve("-Users-Demo-Dev-Work-my-app", None, &home(), &dirs);
        assert_eq!(got, ok("/Users/Demo/Dev/Work/my_app"));
    }

    #[test]
    fn backtracks_out_of_a_dead_end_branch() {
        let dirs = FakeDirs::new(&["/Users/Demo/a/b", "/Users/Demo/a-b/c"]);
        let got = resolve("-Users-Demo-a-b-c", None, &home(), &dirs);
        assert_eq!(got, ok("/Users/Demo/a-b/c"));
    }

    #[test]
    fn hidden_folders_resolve() {
        let dirs = FakeDirs::new(&["/Users/Demo/repo/.claude/worktrees/a1"]);
        let got = resolve(
            "-Users-Demo-repo--claude-worktrees-a1",
            None,
            &home(),
            &dirs,
        );
        assert_eq!(got, ok("/Users/Demo/repo/.claude/worktrees/a1"));
    }

    #[test]
    fn removed_folder_reports_deepest_reached() {
        let dirs = FakeDirs::new(&["/Users/Demo/Dev/wt"]);
        let got = resolve("-Users-Demo-Dev-wt-feature-x", None, &home(), &dirs);
        assert_eq!(got, gone("/Users/Demo/Dev/wt"));
    }

    #[test]
    fn no_match_falls_back_to_home() {
        let dirs = FakeDirs::new(&["/Elsewhere"]);
        let got = resolve("-Users-Demo-Dev-wt-feature-x", None, &home(), &dirs);
        assert_eq!(got, gone("/Users/Demo"));
    }

    #[test]
    fn terminates_on_repeated_dashes_and_shared_slugs() {
        let dirs = FakeDirs::new(&["/a_b/c", "/a-b/c", "/a.b/c", "/a b/c"]);
        let name = format!("-{}", "a-".repeat(200));
        let got = resolve(&name, None, &home(), &dirs);
        assert_eq!(got.state, FolderState::Gone);
    }

    #[test]
    fn real_dirs_lists_only_directories() {
        let root = std::env::temp_dir().join(format!("threads-folder-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();
        let mut names = RealDirs.child_dirs(&root);
        names.sort();
        assert_eq!(names, vec!["sub".to_string()]);
        assert!(RealDirs.is_dir(&root));
        assert!(RealDirs.child_dirs(&root.join("missing")).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
