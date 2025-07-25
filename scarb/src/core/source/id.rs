use std::collections::HashSet;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smol_str::SmolStr;
use url::Url;

use crate::core::registry::DEFAULT_REGISTRY_INDEX;
use crate::core::source::Source;
use crate::core::{Config, PackageId};
use crate::internal::fsx::PathBufUtf8Ext;
use crate::internal::static_hash_cache::StaticHashCache;
use crate::sources::canonical_url::CanonicalUrl;
use scarb_stable_hash::short_hash;

/// Unique identifier for a source of packages.
///
/// See [`SourceIdInner`] for public fields reference.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SourceId(&'static SourceIdInner);

#[derive(Clone, Eq, Ord, PartialOrd)]
#[non_exhaustive]
pub struct SourceIdInner {
    /// The source URL.
    pub url: Url,
    /// The source kind.
    pub kind: SourceKind,
    /// The canonical URL of this source, used for internal comparison purposes.
    pub canonical_url: CanonicalUrl,
}

impl PartialEq for SourceIdInner {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.canonical_url == other.canonical_url
    }
}

impl Hash for SourceIdInner {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.canonical_url.hash(state);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SourceKind {
    /// A local path.
    Path,
    /// A git repository.
    Git(GitSourceSpec),
    /// A remote registry.
    Registry,
    /// The Cairo standard library.
    Std,
}

impl SourceKind {
    pub fn as_git_source_spec(&self) -> Option<&GitSourceSpec> {
        match self {
            SourceKind::Git(r) => Some(r),
            _ => None,
        }
    }

    /// Returns `true`, if self coming from the lock file, can lock dependency with `other`.
    ///
    /// * If both kinds are [`SourceKind::Git`] and both have `Some` value of the `precise` field,
    ///   then they must be equal.
    /// * If both kinds are [`SourceKind::Git`] and `self` has `Some` `precise` value, while the
    ///   `other` has `None`, then both kinds must be equal ignoring the `precise` value.
    /// * Otherwise; the regular equality check is performed.
    fn can_lock_source_kind(&self, other: &Self) -> bool {
        if self == other {
            return true;
        }

        match self {
            // We can reject specs without precise,
            // as they would need to be identical anyway.
            SourceKind::Git(spec) if spec.precise.is_none() => false,
            SourceKind::Git(spec) => {
                let other_precise = other
                    .as_git_source_spec()
                    .and_then(|other_spec| other_spec.precise.clone());

                // If the other source kind has a precise revision locked,
                // and the other source kind does not equal self,
                // then self cannot lock the other source kind.
                if other_precise.is_some() {
                    return false;
                }

                spec.precise
                    .clone()
                    .and_then(|precise| {
                        // Compare other attributes apart from precise revision.
                        // Note that `other` with different source kind defaults to false on unwrap.
                        other
                            .as_git_source_spec()
                            // Overwrite precise in other.
                            .map(|p| p.clone().with_precise(precise))
                            .map(|s| s == *spec)
                    })
                    .unwrap_or(false)
            }
            // Reject rest as handled by equality check.
            _ => false,
        }
    }
}

const PATH_SOURCE_PROTOCOL: &str = "path";
const GIT_SOURCE_PROTOCOL: &str = "git";
const REGISTRY_SOURCE_PROTOCOL: &str = "registry";
const STD_SOURCE_PROTOCOL: &str = "std";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GitSourceSpec {
    pub reference: GitReference,
    pub precise: Option<String>,
}

impl GitSourceSpec {
    pub fn new(reference: GitReference) -> Self {
        Self {
            reference,
            precise: None,
        }
    }

    pub fn with_precise(self, precise: String) -> Self {
        Self {
            precise: Some(precise),
            ..self
        }
    }
}

/// Information to find a specific commit in a Git repository.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum GitReference {
    /// From a tag.
    Tag(SmolStr),
    /// From a branch.
    Branch(SmolStr),
    /// From a specific revision.
    Rev(SmolStr),
    /// The default branch of the repository, the reference named `HEAD`.
    DefaultBranch,
}

impl SourceId {
    fn new(url: Url, kind: SourceKind) -> Result<Self> {
        let canonical_url = CanonicalUrl::new(&url)?;
        Ok(Self::intern(SourceIdInner {
            url,
            kind,
            canonical_url,
        }))
    }

    /// Creates a new `SourceId` from this source with the given `precise`.
    pub fn with_precise(self, v: String) -> Result<SourceId> {
        let kind = self
            .kind
            .as_git_source_spec()
            .map(|spec| spec.clone().with_precise(v.clone()))
            .map(SourceKind::Git)
            .ok_or_else(|| anyhow!("cannot set precise version for non-git source: {self}"))?;

        Ok(Self::intern(SourceIdInner {
            kind,
            ..(*self).clone()
        }))
    }

    pub fn can_lock_source_id(self, other: Self) -> bool {
        if self == other {
            return true;
        }

        let can_lock = self.kind.can_lock_source_kind(&other.kind);

        // Check if other attributes apart from kind are equal.
        can_lock && self.equals_ignoring_kind(other)
    }

    fn equals_ignoring_kind(self, other: Self) -> bool {
        let first = SourceIdInner {
            kind: SourceKind::Std,
            ..(self.0).clone()
        };
        let second = SourceIdInner {
            kind: SourceKind::Std,
            ..(other.0).clone()
        };
        first == second
    }

    fn intern(inner: SourceIdInner) -> Self {
        static CACHE: StaticHashCache<SourceIdInner> = StaticHashCache::new();
        Self(CACHE.intern(inner))
    }

    pub fn for_path(path: &Utf8Path) -> Result<Self> {
        let url = if path.is_dir() {
            Url::from_directory_path(path)
        } else {
            Url::from_file_path(path)
        };
        let url = url.map_err(|_| anyhow!("path ({}) is not absolute", path))?;
        Self::new(url, SourceKind::Path)
    }

    pub fn for_git(url: &Url, reference: &GitReference) -> Result<Self> {
        let reference = GitSourceSpec::new(reference.clone());
        Self::new(url.clone(), SourceKind::Git(reference))
    }

    pub fn for_registry(url: &Url) -> Result<Self> {
        Self::new(url.clone(), SourceKind::Registry)
    }

    pub fn for_std() -> Self {
        static CACHE: LazyLock<SourceId> = LazyLock::new(|| {
            let url = Url::parse("scarb:/std").unwrap();
            SourceId::new(url, SourceKind::Std).unwrap()
        });
        *CACHE
    }

    pub fn default_registry() -> Self {
        static CACHE: LazyLock<SourceId> = LazyLock::new(|| {
            let url = Url::parse(DEFAULT_REGISTRY_INDEX).unwrap();
            SourceId::new(url, SourceKind::Registry).unwrap()
        });
        *CACHE
    }

    pub fn is_registry(self) -> bool {
        self.kind == SourceKind::Registry
    }

    pub fn is_default_registry(self) -> bool {
        self == Self::default_registry()
    }

    pub fn is_path(self) -> bool {
        self.kind == SourceKind::Path
    }

    pub fn to_path(self) -> Option<Utf8PathBuf> {
        match self.kind {
            SourceKind::Path => Some(
                self.url
                    .to_file_path()
                    .expect("this has to be a file:// URL")
                    .try_into_utf8()
                    .expect("URLs are UTF-8 encoded"),
            ),

            _ => None,
        }
    }

    pub fn is_git(self) -> bool {
        matches!(self.kind, SourceKind::Git(_))
    }

    /// Gets the [`GitReference`] if this is a [`SourceKind::Git`] source, otherwise `None`.
    pub fn git_reference(self) -> Option<GitReference> {
        match &self.kind {
            SourceKind::Git(GitSourceSpec { reference, .. }) => Some(reference.clone()),
            _ => None,
        }
    }

    pub fn is_std(self) -> bool {
        self.kind == SourceKind::Std
    }

    pub fn ident(self) -> String {
        let ident = self
            .url
            .host_str()
            .unwrap_or_else(|| self.kind.primary_field());
        let hash = short_hash(self);
        format!("{ident}-{hash}")
    }

    pub fn to_pretty_url(self) -> String {
        match &self.kind {
            SourceKind::Path => format!("{PATH_SOURCE_PROTOCOL}+{}", self.url),

            SourceKind::Git(GitSourceSpec { reference, precise }) => {
                let mut url = self.url.clone();
                match reference {
                    GitReference::Tag(tag) => {
                        url.query_pairs_mut().append_pair("tag", tag);
                    }
                    GitReference::Branch(branch) => {
                        url.query_pairs_mut().append_pair("branch", branch);
                    }
                    GitReference::Rev(rev) => {
                        url.query_pairs_mut().append_pair("rev", rev);
                    }
                    GitReference::DefaultBranch => {}
                }
                let precise = precise
                    .as_ref()
                    .map(|p| format!("#{p}"))
                    .unwrap_or_default();
                format!("{GIT_SOURCE_PROTOCOL}+{url}{precise}")
            }

            SourceKind::Registry => format!("{REGISTRY_SOURCE_PROTOCOL}+{}", self.url),

            SourceKind::Std => STD_SOURCE_PROTOCOL.to_string(),
        }
    }

    pub fn from_pretty_url(pretty_url: &str) -> Result<Self> {
        if pretty_url == STD_SOURCE_PROTOCOL {
            return Ok(Self::for_std());
        }

        let (kind, url_part) = {
            let mut parts = pretty_url.splitn(2, '+');
            (
                parts.next().expect("at least one part must be here"),
                parts
                    .next()
                    .ok_or_else(|| anyhow!("invalid source: {pretty_url}"))?,
            )
        };

        let parse_url = |value: &str| {
            Url::parse(value).with_context(|| format!("cannot parse source URL: {pretty_url}"))
        };

        let url = || parse_url(url_part);

        match kind {
            GIT_SOURCE_PROTOCOL => {
                let (mut url, precise) = url_part
                    .rsplit_once('#')
                    .map(|(url, precise)| -> Result<(_, _)> {
                        Ok((parse_url(url)?, Some(precise.to_string())))
                    })
                    .unwrap_or_else(|| Ok((url()?, None)))?;

                let mut reference = GitReference::DefaultBranch;
                for (k, v) in url.query_pairs() {
                    match &k[..] {
                        "branch" => reference = GitReference::Branch(v.into()),
                        "rev" => reference = GitReference::Rev(v.into()),
                        "tag" => reference = GitReference::Tag(v.into()),
                        _ => {}
                    }
                }

                url.set_query(None);

                let sid = SourceId::for_git(&url, &reference)?;
                precise.map(|p| sid.with_precise(p)).unwrap_or(Ok(sid))
            }

            PATH_SOURCE_PROTOCOL => SourceId::new(url()?, SourceKind::Path),

            REGISTRY_SOURCE_PROTOCOL => SourceId::for_registry(&(url()?)),

            kind => bail!("unsupported source protocol: {kind}"),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_display_str(string: &str) -> Result<Self> {
        Self::for_path(&Utf8PathBuf::from(string)).or_else(|_| Self::from_pretty_url(string))
    }

    /// Creates an implementation of `Source` corresponding to this ID.
    pub fn load<'c>(
        self,
        config: &'c Config,
        yanked_whitelist: &HashSet<PackageId>,
    ) -> Result<Arc<dyn Source + 'c>> {
        use crate::sources::*;
        match self.kind {
            SourceKind::Path => Ok(Arc::new(PathSource::new(self, config))),
            SourceKind::Git(_) => Ok(Arc::new(GitSource::new(self, config)?)),
            SourceKind::Registry => Ok(Arc::new(RegistrySource::new(
                self,
                config,
                yanked_whitelist,
            )?)),
            SourceKind::Std => Ok(Arc::new(StandardLibSource::new(config))),
        }
    }
}

#[cfg(test)]
impl SourceId {
    pub(crate) fn mock_git() -> SourceId {
        let url = Url::parse("https://github.com/starkware-libs/cairo.git").unwrap();
        let reference = GitReference::Tag("test".into());
        SourceId::for_git(&url, &reference).unwrap()
    }

    pub(crate) fn mock_path() -> SourceId {
        use crate::internal::fsx::PathUtf8Ext;
        let path = std::env::temp_dir();
        let path = path.try_as_utf8().unwrap();
        SourceId::for_path(path).unwrap()
    }
}

impl Deref for SourceId {
    type Target = SourceIdInner;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl Default for SourceId {
    fn default() -> Self {
        SourceId::default_registry()
    }
}

impl fmt::Debug for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SourceId")
            .field(&self.url.to_string())
            .field(&self.kind)
            .finish()
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.kind == SourceKind::Path {
            let path = self.url.to_file_path().expect("expected file:// URL here");
            write!(f, "{}", path.display())
        } else {
            write!(f, "{}", self.to_pretty_url())
        }
    }
}

impl Serialize for SourceId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.to_pretty_url())
    }
}

impl<'de> Deserialize<'de> for SourceId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<SourceId, D::Error> {
        use serde::de::Error;
        let string = String::deserialize(d)?;
        SourceId::from_pretty_url(&string).map_err(Error::custom)
    }
}

impl SourceKind {
    pub fn primary_field(&self) -> &str {
        match self {
            SourceKind::Path => "path",
            SourceKind::Git(_) => "git",
            SourceKind::Registry => "registry",
            SourceKind::Std => "std",
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;
    use url::Url;

    use crate::core::{GitReference, source::SourceId};

    #[test_case(SourceId::mock_git())]
    #[test_case(SourceId::mock_path())]
    #[test_case(SourceId::default_registry())]
    #[test_case(SourceId::for_std())]
    fn equality_after_pretty_url_conversion(source_id: SourceId) {
        assert_eq!(
            SourceId::from_pretty_url(&source_id.to_pretty_url()).unwrap(),
            source_id
        );
    }

    #[test]
    fn ignores_git_suffix() {
        fn mock_git(input: &str) -> SourceId {
            let url = Url::parse(input).unwrap();
            let reference = GitReference::Tag("test".into());
            SourceId::for_git(&url, &reference).unwrap()
        }

        assert_eq!(
            mock_git("https://github.com/starkware-libs/cairo"),
            mock_git("https://github.com/starkware-libs/cairo.git")
        );
    }

    #[test]
    fn includes_precise() {
        let sid = SourceId::mock_git();
        let original = sid.to_pretty_url();
        assert!(!original.contains("some_rev"));
        assert!(!original.contains('#'));
        let sid = sid.with_precise("some_rev".into()).unwrap();
        assert_eq!(sid.to_pretty_url(), format!("{original}#some_rev"));
    }

    #[test]
    fn parses_precise() {
        let sid = SourceId::mock_git();
        let sid = sid.with_precise("some_rev".into()).unwrap();
        assert_eq!(
            SourceId::from_pretty_url(&sid.to_pretty_url()).unwrap(),
            sid
        );
    }

    // NOTE: Path sources are deliberately not tested here, because paths have different form
    //   depending on running OS. We simply trust that this code works in that case.
    #[test_case(SourceId::mock_git() => "github.com-192sksn8g7p8c")]
    #[test_case(SourceId::default_registry() => "scarbs.xyz-9djtpev4jug5q")]
    #[test_case(SourceId::for_std() => "std-drqrn62cbjj5g")]
    fn ident(source_id: SourceId) -> String {
        source_id.ident()
    }
}
