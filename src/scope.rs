//! Coordinator documents are selected by registered identity, never by cwd,
//! task text or the presence of old project-root files.

use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::coordinator::Caller;
use crate::project::Project;

#[derive(Debug, Clone)]
pub struct Scope {
    pub dir: PathBuf,
    pub source: String,
    root: PathBuf,
}

fn checked(root: &Path, path: &Path, directory: bool) -> Result<()> {
    let relative = path.strip_prefix(root)?;
    let mut current = root.to_path_buf();
    let count = relative.components().count();
    for (i, part) in relative.components().enumerate() {
        let Component::Normal(part) = part else { bail!("invalid document path {}", path.display()); };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || if i + 1 < count || directory { !meta.is_dir() } else { !meta.is_file() } {
                    bail!("document scope refuses symlink or wrong file type at {}", current.display());
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("reading {}", current.display())),
        }
    }
    Ok(())
}

pub fn validate_name(project: &Project, name: &str) -> Result<()> {
    if !crate::names::is_valid(name) || !crate::names::is_coordinator_name(&project.slug, name) {
        bail!("`{name}` is not an exact coordinator name of `{}`", project.slug);
    }
    Ok(())
}

impl Scope {
    pub fn coordinator(project: &Project, name: &str) -> Result<Self> {
        validate_name(project, name)?;
        let root = project.canonical_dir();
        let scope = Self { dir: root.join("coordinators").join(name), source: name.into(), root };
        checked(&scope.root, &scope.dir, true)?;
        Ok(scope)
    }

    /// Read-only lookup for a human inspecting preserved documents. Historical
    /// names must belong to a recorded former slug and already exist; this is
    /// deliberately separate from current-name creation and task delegation.
    pub fn existing(project: &Project, name: &str) -> Result<Self> {
        let recognized = crate::names::is_valid(name)
            && (crate::names::is_coordinator_name(&project.slug, name)
                || project.former_slugs().iter().any(|slug| crate::names::is_coordinator_name(slug, name)));
        if !recognized { bail!("`{name}` is not a current or historical coordinator scope of `{}`", project.slug); }
        let root = project.canonical_dir();
        let scope = Self { dir: root.join("coordinators").join(name), source: name.into(), root };
        checked(&scope.root, &scope.dir, true)?;
        if !std::fs::symlink_metadata(&scope.dir).with_context(|| format!("scope `{name}` does not exist"))?.is_dir() {
            bail!("scope `{name}` is not a directory");
        }
        Ok(scope)
    }

    pub fn shared(project: &Project) -> Self {
        let root = project.canonical_dir();
        Self { dir: root.clone(), source: "shared".into(), root }
    }

    pub fn path(&self, relative: &str, directory: bool) -> Result<PathBuf> {
        let path = self.dir.join(relative);
        checked(&self.root, &path, directory)?;
        Ok(path)
    }

    pub fn read(&self, relative: &str) -> Result<String> {
        let path = self.path(relative, false)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Bounded automatic reads; one extra byte detects truncation without
    /// loading an arbitrarily large document.
    pub fn preview(&self, relative: &str, limit: usize) -> Result<String> {
        let path = self.path(relative, false)?;
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(String::new()),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1) as u64).read_to_end(&mut bytes)?;
        Ok(bounded(&String::from_utf8_lossy(&bytes), limit, &format!("read {}", path.display())))
    }

    /// Create only empty owned documents. Root legacy content is never copied.
    /// The caller holds the project lock when reserving a new identity.
    fn initialize(&self) -> Result<()> {
        checked(&self.root, &self.dir, true)?;
        std::fs::create_dir_all(&self.dir)?;
        for name in ["memory", "scratch"] {
            std::fs::create_dir_all(self.path(name, true)?)?;
        }
        for (name, text) in [("TASKS.md", "# Tasks\n\n## Backlog\n"), ("MEMORY.md", "# Memory\n")] {
            let path = self.path(name, false)?;
            match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(text.as_bytes())?;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

/// Human callers default to the legacy/shared source; coordinators default to
/// their own. Selecting shared is explicit; selecting another owner is human-only.
pub fn select(project: &Project, caller: &Caller, source: Option<&str>) -> Result<Scope> {
    match caller {
        Caller::Human => match source {
            None | Some("shared") => Ok(Scope::shared(project)),
            Some(name) => Scope::coordinator(project, name),
        },
        Caller::Coordinator { name, .. } => match source {
            Some("shared") => Ok(Scope::shared(project)),
            Some(other) if other != name => bail!("coordinator `{name}` cannot select `{other}`'s documents; use your own source"),
            _ => Scope::coordinator(project, name),
        },
        Caller::Stranger { pane } | Caller::Unverified { pane } => Err(crate::coordinator::unverified_refusal(&project.slug, pane)),
    }
}

pub fn ensure(project: &Project, name: &str) -> Result<Scope> {
    let _lock = project.lock()?;
    let scope = Scope::coordinator(project, name)?;
    scope.initialize()?;
    Ok(scope)
}

/// Scope directories are durable name reservations, even while empty. Old
/// records and owners reserve names too, before they have ever read context.
pub fn reserve(project: &Project, live: &[String], resume: Option<&str>) -> Result<String> {
    let _lock = project.lock()?;
    if let Some(name) = resume {
        let scope = Scope::coordinator(project, name)?;
        scope.initialize()?;
        return Ok(name.into());
    }
    let parent = project.canonical_dir().join("coordinators");
    checked(&project.canonical_dir(), &parent, true)?;
    let mut taken = live.to_vec();
    match std::fs::read_dir(&parent) {
        Ok(entries) => for entry in entries {
            taken.push(entry?.file_name().to_string_lossy().into_owned());
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(record) = project.coordinator() { taken.push(record.agent_name); }
    taken.extend(crate::coordinator::live(project).into_iter().map(|p| p.name));
    taken.extend(crate::thread::list(project).into_iter().map(|t| t.owner));
    taken.extend(crate::inbox::unhandled(project).into_iter().filter_map(|i| i.owner));
    let name = crate::names::free_coordinator(&project.slug, &taken);
    Scope::coordinator(project, &name)?.initialize()?;
    Ok(name)
}

/// Read-only sources for the person's popup, labelled by their exact origin.
pub fn sources(project: &Project) -> Result<Vec<Scope>> {
    let mut scopes = vec![Scope::shared(project)];
    let parent = project.canonical_dir().join("coordinators");
    checked(&project.canonical_dir(), &parent, true)?;
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(scopes),
        Err(error) => return Err(error.into()),
    };
    let mut names = Vec::new();
    for entry in entries { names.push(entry?.file_name().to_string_lossy().into_owned()); }
    names.sort();
    // A stale, malformed or symlinked entry must not hide unrelated valid
    // scopes. Explicit lookup still explains why such an entry is refused.
    for name in names {
        if let Ok(scope) = Scope::existing(project, &name) { scopes.push(scope); }
    }
    Ok(scopes)
}

/// Byte budget including the continuation marker, without splitting UTF-8.
pub fn bounded(text: &str, limit: usize, continuation: &str) -> String {
    if text.len() <= limit { return text.into(); }
    fn prefix(text: &str, limit: usize) -> &str {
        let mut end = limit.min(text.len());
        while !text.is_char_boundary(end) { end -= 1; }
        &text[..end]
    }
    let marker = format!("\n[truncated; {continuation}]\n");
    let marker = prefix(&marker, limit);
    format!("{}{marker}", prefix(text, limit.saturating_sub(marker.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_selection_is_exact_and_has_no_root_fallback() {
        let home = tempfile::tempdir().unwrap();
        let p = crate::project::create(home.path(), "demo", "", vec![]).unwrap();
        std::fs::write(p.dir().join("TASKS.md"), "ROOT").unwrap();
        let a = Caller::Coordinator { name: "hpc-demo".into(), pane: "p1".into() };
        assert_eq!(select(&p, &a, None).unwrap().read("TASKS.md").unwrap(), "");
        assert_eq!(select(&p, &a, Some("shared")).unwrap().read("TASKS.md").unwrap(), "ROOT");
        assert!(select(&p, &a, Some("hpc-demo-1")).is_err());
        assert!(select(&p, &Caller::Human, Some("hpc-demo-1")).is_ok());
        for name in ["../demo", "hpc-demo/..", "hpc-demo-+1", "hpc-demo-01", "hpc-other", ""] {
            assert!(Scope::coordinator(&p, name).is_err(), "{name}");
        }
        let scope = ensure(&p, "hpc-demo").unwrap();
        std::os::unix::fs::symlink(p.dir().join("TASKS.md"), scope.dir.join("bad.md")).unwrap();
        assert!(scope.read("bad.md").is_err());
        std::os::unix::fs::symlink(p.dir(), p.dir().join("coordinators/hpc-demo-1")).unwrap();
        assert!(Scope::coordinator(&p, "hpc-demo-1").is_err());
        std::fs::remove_dir(scope.dir.join("memory")).unwrap();
        std::os::unix::fs::symlink(p.dir().join("memory"), scope.dir.join("memory")).unwrap();
        assert!(scope.path("memory", true).is_err());
    }

    #[test]
    fn dormant_scopes_and_recorded_owners_reserve_names() {
        let home = tempfile::tempdir().unwrap();
        let p = crate::project::create(home.path(), "demo", "", vec![]).unwrap();
        assert_eq!(reserve(&p, &[], None).unwrap(), "hpc-demo");
        p.update_coordinator(|r| r.agent_name = "hpc-demo-1".into()).unwrap();
        crate::thread::allocate(&p, |t| t.owner = "hpc-demo-2".into()).unwrap();
        assert_eq!(reserve(&p, &[], None).unwrap(), "hpc-demo-3");
        assert_eq!(reserve(&p, &[], Some("hpc-demo")).unwrap(), "hpc-demo");
        assert_eq!(reserve(&p, &[], None).unwrap(), "hpc-demo-4");
    }

    #[test]
    fn historical_lookup_is_existing_read_only_and_does_not_poison_current_sources() {
        let home = tempfile::tempdir().unwrap();
        let p = crate::project::create(home.path(), "current", "", vec![]).unwrap();
        p.add_former_slug("old").unwrap();
        let current = ensure(&p, "hpc-current").unwrap();
        let historical = p.dir().join("coordinators/hpc-old");
        std::fs::create_dir(&historical).unwrap();
        std::fs::write(historical.join("MEMORY.md"), "OLD_MEMORY").unwrap();
        std::fs::write(p.dir().join("MEMORY.md"), "ROOT_MEMORY").unwrap();
        assert_eq!(Scope::existing(&p, "hpc-old").unwrap().read("MEMORY.md").unwrap(), "OLD_MEMORY");
        assert!(Scope::coordinator(&p, "hpc-old").is_err());
        assert!(ensure(&p, "hpc-old").is_err());
        assert!(select(&p, &Caller::Human, Some("hpc-old")).is_err(), "historical inspection must not become task delegation");
        let agent = Caller::Coordinator { name: "hpc-current".into(), pane: "p1".into() };
        assert!(select(&p, &agent, Some("hpc-old")).is_err());
        assert!(Scope::existing(&p, "hpc-old-9").is_err());
        assert!(Scope::existing(&p, "hpc-current-9").is_err());
        assert!(!p.dir().join("coordinators/hpc-old-9").exists());
        for name in ["../old", "hpc-old/..", "hpc-old-01", "hpc-unrelated"] {
            assert!(Scope::existing(&p, name).is_err());
        }
        std::fs::create_dir(p.dir().join("coordinators/hpc-unrelated")).unwrap();
        std::fs::write(p.dir().join("coordinators/not-a-directory"), "ignored").unwrap();
        std::os::unix::fs::symlink(&current.dir, p.dir().join("coordinators/hpc-old-1")).unwrap();
        assert!(Scope::existing(&p, "hpc-old-1").is_err());
        let listed = sources(&p).unwrap().into_iter().map(|s| s.source).collect::<Vec<_>>();
        assert_eq!(listed, ["shared", "hpc-current", "hpc-old"]);
        std::os::unix::fs::symlink(p.dir().join("MEMORY.md"), historical.join("TASKS.md")).unwrap();
        assert!(Scope::existing(&p, "hpc-old").unwrap().read("TASKS.md").is_err());
        assert_eq!(current.read("TASKS.md").unwrap(), "# Tasks\n\n## Backlog\n");
    }

    #[test]
    fn budgets_include_markers_and_handle_unicode() {
        let text = "界🦀".repeat(100);
        for limit in [0, 1, 20, 100] {
            let result = bounded(&text, limit, "read file");
            assert!(result.len() <= limit);
        }
        assert!(bounded(&text, 100, "read file").contains("truncated; read file"));
    }
}
