// Copyright 2026 Devin R O'Loughlin / Droco LLC
// SPDX-License-Identifier: MIT

//! Which directories are projects SessionGuard should act on.
//!
//! # Why this exists
//!
//! The watcher reports every rename under a watched tree, and in a working
//! tree almost none of them are project moves: an editor's atomic save, git's
//! lock-file dance (one `git init && git commit` produced 9 rename events),
//! Cargo renaming its incremental-compilation directories (a hello-world
//! build produced 3). Through v0.10 the daemon treated each as a project move
//! and planned a re-key across every session store for it — ~1.2 s and ~3.9 GB
//! of memory per event on a real machine — which made running the daemon on a
//! real development tree untenable.
//!
//! The fix is to ask the only question that matters: *was the renamed
//! directory a project we know about, or does it contain one?* "Known" means
//! a path some session store is keyed to (from the census, which reads each
//! store's keys in bounded memory) or a project registered with `watch`/`scan`.
//! Anything else is ignored before any store is touched.
//!
//! "Contains" matters as much as "is": moving `~/work` to `~/archive/work`
//! moves every project under it, and each of their store keys must follow.
//! [`KnownProjects::pairs_for_move`] returns one `(old, new)` pair per known
//! project at or beneath the moved directory.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Paths that must never be treated as a movable project key, even if a
/// session store is keyed to them (sessions started in `$HOME` or `/`). A
/// rename can't move these, and matching them as "ancestors" would make every
/// move look like a move of everything.
fn is_structural(p: &Path, home: Option<&Path>) -> bool {
    p.parent().is_none() || home.is_some_and(|h| p == h || h.starts_with(p))
}

/// An index of known project paths.
#[derive(Debug, Clone)]
pub struct KnownProjects {
    /// `(lookup form, key as the store recorded it)`. A key can appear under
    /// two lookup forms: as recorded, and canonicalized — the filesystem
    /// watcher reports canonical paths (`/private/var/…` on macOS), while
    /// tools record whatever their working directory looked like (`/var/…`).
    entries: Vec<(PathBuf, PathBuf)>,
    built_at: Instant,
}

impl KnownProjects {
    /// Index an explicit set of project keys. `home` keys and ancestors of it
    /// are dropped (see [`is_structural`]).
    pub fn from_keys(keys: impl IntoIterator<Item = PathBuf>, home: Option<&Path>) -> Self {
        let mut entries: Vec<(PathBuf, PathBuf)> = Vec::new();
        for key in keys {
            if key.as_os_str().is_empty() || !key.is_absolute() || is_structural(&key, home) {
                continue;
            }
            if let Ok(canon) = std::fs::canonicalize(&key) {
                if canon != key {
                    entries.push((canon, key.clone()));
                }
            }
            entries.push((key.clone(), key));
        }
        entries.sort();
        entries.dedup();
        Self {
            entries,
            built_at: Instant::now(),
        }
    }

    /// Index everything this machine knows: every session-store key the
    /// census resolves under `census_root` (skipping raw, undecodable store
    /// names) plus every registered project.
    pub fn build(
        census_root: &Path,
        tools: &crate::tools::ToolRegistry,
        registry: &crate::registry::Registry,
    ) -> Self {
        let mut keys: Vec<PathBuf> = Vec::new();
        let home = (!census_root.as_os_str().is_empty()).then_some(census_root);
        if let Some(root) = home {
            let env = |var: &str| std::env::var(var).ok();
            let stores = crate::sessions::resolve_stores(tools.all(), Some(&env));
            for g in crate::sessions::census(root, &stores, false) {
                if g.confidence != crate::sessions::DecodeConfidence::Unresolved {
                    keys.push(PathBuf::from(g.project_path));
                }
            }
        }
        if let Ok(projects) = registry.list_projects() {
            keys.extend(projects.into_iter().map(|p| p.path));
        }
        Self::from_keys(keys, home)
    }

    pub fn len(&self) -> usize {
        let mut recorded: Vec<&PathBuf> = self.entries.iter().map(|(_, k)| k).collect();
        recorded.sort();
        recorded.dedup();
        recorded.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn age(&self) -> Duration {
        self.built_at.elapsed()
    }

    /// The store keys a directory move `from → to` must re-key: `from` itself
    /// if it is a known project, plus every known project beneath it, each
    /// translated to the same place under `to`. Empty when the directory holds
    /// no known project — the common case, and the signal to do nothing.
    pub fn pairs_for_move(&self, from: &Path, to: &Path) -> Vec<(PathBuf, PathBuf)> {
        // `from` no longer exists, so it can't be canonicalized directly; its
        // parent usually still does.
        let mut forms = vec![from.to_path_buf()];
        if let (Some(parent), Some(name)) = (from.parent(), from.file_name()) {
            if let Ok(canon_parent) = std::fs::canonicalize(parent) {
                let canon = canon_parent.join(name);
                if canon != from {
                    forms.push(canon);
                }
            }
        }

        let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (form, recorded) in &self.entries {
            for f in &forms {
                // Component-wise: `/work/app` is not a prefix of `/work/app-two`.
                if let Ok(rel) = form.strip_prefix(f) {
                    let new = if rel.as_os_str().is_empty() {
                        to.to_path_buf()
                    } else {
                        to.join(rel)
                    };
                    if !out.iter().any(|(old, _)| old == recorded) {
                        out.push((recorded.clone(), new));
                    }
                    break;
                }
            }
        }
        out.sort();
        out
    }

    /// After a move is handled, the moved projects are known at their new
    /// paths — so moving one again doesn't need a rebuild to be recognised.
    pub fn apply_move(&mut self, pairs: &[(PathBuf, PathBuf)]) {
        for (old, new) in pairs {
            self.entries.retain(|(_, recorded)| recorded != old);
            self.entries.push((new.clone(), new.clone()));
        }
        self.entries.sort();
        self.entries.dedup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn idx(keys: &[&str]) -> KnownProjects {
        KnownProjects::from_keys(keys.iter().map(PathBuf::from), Some(Path::new("/home/me")))
    }

    #[test]
    fn a_rename_of_an_unknown_directory_yields_nothing() {
        // The cargo/git/editor case: none of these directories is a project.
        let k = idx(&["/home/me/work/app"]);
        for (from, to) in [
            (
                "/home/me/work/app/.git/refs.lock",
                "/home/me/work/app/.git/refs",
            ),
            (
                "/home/me/work/app/target/debug/incremental/s-abc-working",
                "/home/me/work/app/target/debug/incremental/s-abc",
            ),
            ("/home/me/work/app-two", "/home/me/work/app-three"),
        ] {
            assert!(
                k.pairs_for_move(Path::new(from), Path::new(to)).is_empty(),
                "{from} is not a known project and must be ignored"
            );
        }
    }

    #[test]
    fn a_known_project_moving_yields_exactly_its_own_pair() {
        let k = idx(&["/home/me/work/app", "/home/me/work/other"]);
        assert_eq!(
            k.pairs_for_move(
                Path::new("/home/me/work/app"),
                Path::new("/home/me/elsewhere/app")
            ),
            vec![(
                PathBuf::from("/home/me/work/app"),
                PathBuf::from("/home/me/elsewhere/app")
            )]
        );
    }

    #[test]
    fn moving_a_folder_moves_every_project_beneath_it() {
        // The junk-drawer reorg: a parent folder moves, and every project
        // inside must follow. Before this, a folder move re-keyed nothing.
        let k = idx(&[
            "/home/me/junk/rndm/peoples",
            "/home/me/junk/rndm/deep/nested",
            "/home/me/junk/keep",
        ]);
        let pairs = k.pairs_for_move(
            Path::new("/home/me/junk/rndm"),
            Path::new("/home/me/junk/legal"),
        );
        assert_eq!(
            pairs,
            vec![
                (
                    PathBuf::from("/home/me/junk/rndm/deep/nested"),
                    PathBuf::from("/home/me/junk/legal/deep/nested")
                ),
                (
                    PathBuf::from("/home/me/junk/rndm/peoples"),
                    PathBuf::from("/home/me/junk/legal/peoples")
                ),
            ]
        );
    }

    #[test]
    fn home_and_its_ancestors_are_never_keys() {
        // Sessions started in $HOME are keyed to it. If $HOME counted, it
        // would be an "ancestor" of nothing movable — but the root `/` would
        // make every move look like a move of everything.
        let k = KnownProjects::from_keys(
            ["/", "/home", "/home/me", "/home/me/work/app"].map(PathBuf::from),
            Some(Path::new("/home/me")),
        );
        assert_eq!(k.len(), 1);
        assert!(
            k.pairs_for_move(Path::new("/home/me/work/app"), Path::new("/home/me/x"))
                .len()
                == 1
        );
    }

    #[test]
    fn matches_through_a_symlinked_prefix() {
        // macOS: a key recorded under /var/folders/… is reported by the
        // watcher as /private/var/folders/… (/var is a symlink). A temp dir is
        // exactly that shape.
        let t = TempDir::new().unwrap();
        let recorded = t.path().join("app");
        std::fs::create_dir_all(&recorded).unwrap();
        let k = KnownProjects::from_keys([recorded.clone()], None);
        let canon_parent = std::fs::canonicalize(t.path()).unwrap();
        let from = canon_parent.join("app");
        let to = canon_parent.join("moved");
        let pairs = k.pairs_for_move(&from, &to);
        assert_eq!(
            pairs.len(),
            1,
            "canonical event path must find the recorded key"
        );
        assert_eq!(
            pairs[0].0, recorded,
            "re-key uses the key as the STORE recorded it"
        );
    }

    #[test]
    fn apply_move_makes_the_new_path_known() {
        let mut k = idx(&["/home/me/a"]);
        let pairs = k.pairs_for_move(Path::new("/home/me/a"), Path::new("/home/me/b"));
        k.apply_move(&pairs);
        assert!(k
            .pairs_for_move(Path::new("/home/me/a"), Path::new("/home/me/z"))
            .is_empty());
        assert_eq!(
            k.pairs_for_move(Path::new("/home/me/b"), Path::new("/home/me/c"))
                .len(),
            1,
            "moving it again is recognised without a rebuild"
        );
    }
}
