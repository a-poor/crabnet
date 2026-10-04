use std::collections::BTreeSet;

use serde_json::Value;

use super::AppResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub version: String,
    pub workspace: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dependency {
    pub source: String,
    pub target: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub packages: Vec<Package>,
    pub dependencies: Vec<Dependency>,
}

impl Snapshot {
    /// Read the resolved graph from `cargo metadata --format-version 1`.
    /// Multiple aliases/kinds targeting the same package become one edge.
    pub fn from_metadata(value: &Value) -> AppResult<Self> {
        if value["version"].as_u64() != Some(1) {
            return Err("expected cargo metadata format version 1".into());
        }
        let members = array(value, "workspace_members")?
            .iter()
            .map(|id| id.as_str().ok_or("workspace member ID must be a string"))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let packages = array(value, "packages")?
            .iter()
            .map(|package| {
                let id = string(package, "id")?;
                Ok(Package {
                    workspace: members.contains(id),
                    id: id.to_owned(),
                    name: string(package, "name")?.to_owned(),
                    version: string(package, "version")?.to_owned(),
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        let ids: BTreeSet<_> = packages.iter().map(|package| package.id.as_str()).collect();
        if !members.is_subset(&ids) {
            return Err("workspace member is absent from packages".into());
        }
        if value["resolve"].is_null() {
            return Err("metadata has no resolved graph; omit cargo metadata --no-deps".into());
        }
        let mut resolved = BTreeSet::new();
        let mut dependencies = BTreeSet::new();
        for node in array(&value["resolve"], "nodes")? {
            let source = string(node, "id")?;
            if !ids.contains(source) || !resolved.insert(source) {
                return Err(format!("unknown or repeated resolve node: {source}").into());
            }
            for dependency in array(node, "dependencies")? {
                let target = dependency
                    .as_str()
                    .ok_or("dependency ID must be a string")?;
                dependencies.insert(Dependency {
                    source: source.into(),
                    target: target.into(),
                });
            }
        }
        let snapshot = Self {
            packages,
            dependencies: dependencies.into_iter().collect(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> AppResult<()> {
        let mut ids = BTreeSet::new();
        for package in &self.packages {
            if package.id.is_empty() || package.name.is_empty() || package.version.is_empty() {
                return Err("package ID, name, and version must be nonempty".into());
            }
            if !ids.insert(&package.id) {
                return Err(format!("repeated package ID: {}", package.id).into());
            }
        }
        let mut edges = BTreeSet::new();
        for edge in &self.dependencies {
            if !ids.contains(&edge.source) || !ids.contains(&edge.target) {
                return Err(format!("dependency has an unknown endpoint: {edge:?}").into());
            }
            if !edges.insert(edge) {
                return Err(format!("repeated dependency: {edge:?}").into());
            }
        }
        Ok(())
    }

    /// Deterministic components of up to 64 packages, with dependencies on
    /// the next one and two packages. Diamonds exercise path multiplicity.
    pub fn generated(count: usize) -> AppResult<Self> {
        if !(8..=100_000).contains(&count) {
            return Err("package count must be between 8 and 100000".into());
        }
        let packages: Vec<_> = (0..count)
            .map(|index| Package {
                id: format!("fixture:pkg-{index:08}"),
                name: format!("package-{index}"),
                version: "1.0.0".into(),
                workspace: index % 64 == 0,
            })
            .collect();
        let mut dependencies = Vec::new();
        for index in 0..count {
            for next in [index + 1, index + 2] {
                if next < count && next / 64 == index / 64 {
                    dependencies.push(Dependency {
                        source: packages[index].id.clone(),
                        target: packages[next].id.clone(),
                    });
                }
            }
        }
        Ok(Self {
            packages,
            dependencies,
        })
    }
}

fn array<'a>(value: &'a Value, key: &str) -> AppResult<&'a Vec<Value>> {
    value[key]
        .as_array()
        .ok_or_else(|| format!("metadata field {key:?} must be an array").into())
}

fn string<'a>(value: &'a Value, key: &str) -> AppResult<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("metadata field {key:?} must be a string").into())
}
