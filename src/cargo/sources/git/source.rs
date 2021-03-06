use std::fmt::{self, Debug, Formatter};

use url::Url;

use core::source::{Source, SourceId};
use core::GitReference;
use core::{Package, PackageId, Summary, Registry, Dependency};
use util::{CargoResult, Config};
use util::hex::short_hash;
use sources::PathSource;
use sources::git::utils::{GitRemote, GitRevision};

/* TODO: Refactor GitSource to delegate to a PathSource
 */
pub struct GitSource<'cfg> {
    remote: GitRemote,
    reference: GitReference,
    source_id: SourceId,
    path_source: Option<PathSource<'cfg>>,
    rev: Option<GitRevision>,
    ident: String,
    config: &'cfg Config,
}

impl<'cfg> GitSource<'cfg> {
    pub fn new(source_id: &SourceId,
               config: &'cfg Config) -> GitSource<'cfg> {
        assert!(source_id.is_git(), "id is not git, id={}", source_id);

        let remote = GitRemote::new(source_id.url());
        let ident = ident(source_id.url());

        let reference = match source_id.precise() {
            Some(s) => GitReference::Rev(s.to_string()),
            None => source_id.git_reference().unwrap().clone(),
        };

        GitSource {
            remote: remote,
            reference: reference,
            source_id: source_id.clone(),
            path_source: None,
            rev: None,
            ident: ident,
            config: config,
        }
    }

    pub fn url(&self) -> &Url { self.remote.url() }

    pub fn read_packages(&mut self) -> CargoResult<Vec<Package>> {
        if self.path_source.is_none() {
            try!(self.update());
        }
        self.path_source.as_mut().unwrap().read_packages()
    }
}

fn ident(url: &Url) -> String {
    let url = canonicalize_url(url);
    let ident = url.path_segments().and_then(|mut s| s.next_back()).unwrap_or("");

    let ident = if ident == "" {
        "_empty"
    } else {
        ident
    };

    format!("{}-{}", ident, short_hash(&url))
}

// Some hacks and heuristics for making equivalent URLs hash the same
pub fn canonicalize_url(url: &Url) -> Url {
    let mut url = url.clone();

    // Strip a trailing slash
    if url.path().ends_with('/') {
        url.path_segments_mut().unwrap().pop_if_empty();
    }

    // HACKHACK: For github URL's specifically just lowercase
    // everything.  GitHub treats both the same, but they hash
    // differently, and we're gonna be hashing them. This wants a more
    // general solution, and also we're almost certainly not using the
    // same case conversion rules that GitHub does. (#84)
    if url.host_str() == Some("github.com") {
        url.set_scheme("https").unwrap();
        let path = url.path().to_lowercase();
        url.set_path(&path);
    }

    // Repos generally can be accessed with or w/o '.git'
    let needs_chopping = url.path().ends_with(".git");
    if needs_chopping {
        let last = {
            let last = url.path_segments().unwrap().next_back().unwrap();
            last[..last.len() - 4].to_owned()
        };
        url.path_segments_mut().unwrap().pop().push(&last);
    }

    url
}

impl<'cfg> Debug for GitSource<'cfg> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        try!(write!(f, "git repo at {}", self.remote.url()));

        match self.reference.to_ref_string() {
            Some(s) => write!(f, " ({})", s),
            None => Ok(())
        }
    }
}

impl<'cfg> Registry for GitSource<'cfg> {
    fn query(&mut self, dep: &Dependency) -> CargoResult<Vec<Summary>> {
        let src = self.path_source.as_mut()
                      .expect("BUG: update() must be called before query()");
        src.query(dep)
    }
}

impl<'cfg> Source for GitSource<'cfg> {
    fn update(&mut self) -> CargoResult<()> {
        let lock = try!(self.config.lock_git());

        let db_path = lock.parent().join("db").join(&self.ident);

        let reference_path = match self.source_id.git_reference() {
            Some(&GitReference::Branch(ref s)) |
            Some(&GitReference::Tag(ref s)) |
            Some(&GitReference::Rev(ref s)) => s,
            None => panic!("not a git source"),
        };
        let checkout_path = lock.parent().join("checkouts")
            .join(&self.ident).join(reference_path);

        // Resolve our reference to an actual revision, and check if the
        // databaes already has that revision. If it does, we just load a
        // database pinned at that revision, and if we don't we issue an update
        // to try to find the revision.
        let actual_rev = self.remote.rev_for(&db_path, &self.reference);
        let should_update = actual_rev.is_err() ||
                            self.source_id.precise().is_none();

        let (repo, actual_rev) = if should_update {
            try!(self.config.shell().status("Updating",
                format!("git repository `{}`", self.remote.url())));

            trace!("updating git source `{:?}`", self.remote);

            let repo = try!(self.remote.checkout(&db_path, &self.config));
            let rev = try!(repo.rev_for(&self.reference));
            (repo, rev)
        } else {
            (try!(self.remote.db_at(&db_path)), actual_rev.unwrap())
        };

        // Copy the database to the checkout location. After this we could drop
        // the lock on the database as we no longer needed it, but we leave it
        // in scope so the destructors here won't tamper with too much.
        try!(repo.copy_to(actual_rev.clone(), &checkout_path, &self.config));

        let source_id = self.source_id.with_precise(Some(actual_rev.to_string()));
        let path_source = PathSource::new_recursive(&checkout_path,
                                                    &source_id,
                                                    self.config);

        // Cache the information we just learned, and crucially also cache the
        // lock on the checkout location. We wouldn't want someone else to come
        // swipe our checkout location to another revision while we're using it!
        self.path_source = Some(path_source);
        self.rev = Some(actual_rev);
        self.path_source.as_mut().unwrap().update()
    }

    fn download(&mut self, id: &PackageId) -> CargoResult<Package> {
        trace!("getting packages for package id `{}` from `{:?}`", id,
               self.remote);
        self.path_source.as_mut()
                        .expect("BUG: update() must be called before get()")
                        .download(id)
    }

    fn fingerprint(&self, _pkg: &Package) -> CargoResult<String> {
        Ok(self.rev.as_ref().unwrap().to_string())
    }
}

#[cfg(test)]
mod test {
    use url::Url;
    use super::ident;
    use util::ToUrl;

    #[test]
    pub fn test_url_to_path_ident_with_path() {
        let ident = ident(&url("https://github.com/carlhuda/cargo"));
        assert!(ident.starts_with("cargo-"));
    }

    #[test]
    pub fn test_url_to_path_ident_without_path() {
        let ident = ident(&url("https://github.com"));
        assert!(ident.starts_with("_empty-"));
    }

    #[test]
    fn test_canonicalize_idents_by_stripping_trailing_url_slash() {
        let ident1 = ident(&url("https://github.com/PistonDevelopers/piston/"));
        let ident2 = ident(&url("https://github.com/PistonDevelopers/piston"));
        assert_eq!(ident1, ident2);
    }

    #[test]
    fn test_canonicalize_idents_by_lowercasing_github_urls() {
        let ident1 = ident(&url("https://github.com/PistonDevelopers/piston"));
        let ident2 = ident(&url("https://github.com/pistondevelopers/piston"));
        assert_eq!(ident1, ident2);
    }

    #[test]
    fn test_canonicalize_idents_by_stripping_dot_git() {
        let ident1 = ident(&url("https://github.com/PistonDevelopers/piston"));
        let ident2 = ident(&url("https://github.com/PistonDevelopers/piston.git"));
        assert_eq!(ident1, ident2);
    }

    #[test]
    fn test_canonicalize_idents_different_protocls() {
        let ident1 = ident(&url("https://github.com/PistonDevelopers/piston"));
        let ident2 = ident(&url("git://github.com/PistonDevelopers/piston"));
        assert_eq!(ident1, ident2);
    }

    fn url(s: &str) -> Url {
        s.to_url().unwrap()
    }
}
