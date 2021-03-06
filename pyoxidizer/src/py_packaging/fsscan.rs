// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/*!
Scanning the filesystem for Python resources.
*/

use {
    super::distribution::PythonModuleSuffixes,
    super::package_metadata::PythonPackageMetadata,
    super::resource::{
        BytecodeOptimizationLevel, DataLocation, PythonEggFile, PythonExtensionModule,
        PythonModuleBytecode, PythonModuleSource, PythonPackageDistributionResource,
        PythonPackageDistributionResourceFlavor, PythonPackageResource, PythonPathExtension,
        PythonResource,
    },
    anyhow::Result,
    itertools::Itertools,
    std::collections::HashSet,
    std::ffi::OsStr,
    std::path::{Path, PathBuf},
};

pub fn is_package_from_path(path: &Path) -> bool {
    let file_name = path.file_name().unwrap().to_str().unwrap();
    file_name.starts_with("__init__.")
}

pub fn walk_tree_files(path: &Path) -> Box<dyn Iterator<Item = walkdir::DirEntry>> {
    let res = walkdir::WalkDir::new(path).sort_by(|a, b| a.file_name().cmp(b.file_name()));

    let filtered = res.into_iter().filter_map(|entry| {
        let entry = entry.expect("unable to get directory entry");

        let path = entry.path();

        if path.is_dir() {
            None
        } else {
            Some(entry)
        }
    });

    Box::new(filtered)
}

#[derive(Debug, PartialEq)]
struct ResourceFile {
    /// Filesystem path of this resource.
    pub full_path: PathBuf,

    /// Relative path of this resource.
    pub relative_path: PathBuf,
}

#[derive(Debug, PartialEq)]
enum DirEntryItem {
    PythonResource(PythonResource),
    ResourceFile(ResourceFile),
}

pub struct PythonResourceIterator {
    root_path: PathBuf,
    suffixes: PythonModuleSuffixes,
    walkdir_result: Box<dyn Iterator<Item = walkdir::DirEntry>>,
    seen_packages: HashSet<String>,
    resources: Vec<ResourceFile>,
}

impl PythonResourceIterator {
    fn new(path: &Path, suffixes: &PythonModuleSuffixes) -> PythonResourceIterator {
        let res = walkdir::WalkDir::new(path).sort_by(|a, b| a.file_name().cmp(b.file_name()));

        let filtered = res.into_iter().filter_map(|entry| {
            let entry = entry.expect("unable to get directory entry");

            let path = entry.path();

            if path.is_dir() {
                None
            } else {
                Some(entry)
            }
        });

        PythonResourceIterator {
            root_path: path.to_path_buf(),
            suffixes: suffixes.clone(),
            walkdir_result: Box::new(filtered),
            seen_packages: HashSet::new(),
            resources: Vec::new(),
        }
    }

    fn resolve_dir_entry(&mut self, entry: walkdir::DirEntry) -> Option<DirEntryItem> {
        let path = entry.path();

        let mut rel_path = path
            .strip_prefix(&self.root_path)
            .expect("unable to strip path prefix");
        let mut rel_str = rel_path.to_str().expect("could not convert path to str");
        let mut components = rel_path
            .iter()
            .map(|p| p.to_str().expect("unable to get path as str"))
            .collect::<Vec<_>>();

        // Files in .dist-info and .egg-info directories are distribution metadata files.
        // Parsing the package name out of the directory name can be a bit wonky, as
        // case sensitivity and other normalization can come into play. So our strategy
        // is to parse the well-known metadata record inside the directory to extract
        // the package info. If the file doesn't exist or can't be parsed, we ignore this
        // distribution entirely.

        let distribution_info = if components[0].ends_with(".dist-info") {
            Some((
                self.root_path.join(components[0]).join("METADATA"),
                PythonPackageDistributionResourceFlavor::DistInfo,
            ))
        } else if components[0].ends_with(".egg-info") {
            Some((
                self.root_path.join(components[0]).join("PKG-INFO"),
                PythonPackageDistributionResourceFlavor::EggInfo,
            ))
        } else {
            None
        };

        if let Some((metadata_path, location)) = distribution_info {
            let metadata = if let Ok(data) = std::fs::read(&metadata_path) {
                if let Ok(metadata) = PythonPackageMetadata::from_metadata(&data) {
                    metadata
                } else {
                    return None;
                }
            } else {
                return None;
            };

            let package = if let Some(name) = metadata.name() {
                name
            } else {
                return None;
            };

            let version = if let Some(version) = metadata.version() {
                version
            } else {
                return None;
            };

            // Name of resource is file path after the initial directory.
            let name = components[1..components.len()].join("/");

            return Some(DirEntryItem::PythonResource(
                PythonResource::DistributionResource(PythonPackageDistributionResource {
                    location,
                    package: package.to_string(),
                    version: version.to_string(),
                    name,
                    data: DataLocation::Path(path.to_path_buf()),
                }),
            ));
        }

        // site-packages directories are package roots within package roots. Treat them as
        // such.
        let in_site_packages = if components[0] == "site-packages" {
            let sp_path = self.root_path.join("site-packages");
            rel_path = path
                .strip_prefix(sp_path)
                .expect("unable to strip site-packages prefix");

            rel_str = rel_path.to_str().expect("could not convert path to str");
            components = rel_path
                .iter()
                .map(|p| p.to_str().expect("unable to get path as str"))
                .collect::<Vec<_>>();

            true
        } else {
            false
        };

        // It looks like we're in an unpacked egg. This is similar to the site-packages
        // scenario: we essentially have a new package root that corresponds to the
        // egg's extraction directory.
        if (&components[0..components.len() - 1])
            .iter()
            .any(|p| p.ends_with(".egg"))
        {
            let mut egg_root_path = self.root_path.clone();

            if in_site_packages {
                egg_root_path = egg_root_path.join("site-packages");
            }

            for p in &components[0..components.len() - 1] {
                egg_root_path = egg_root_path.join(p);

                if p.ends_with(".egg") {
                    break;
                }
            }

            rel_path = path
                .strip_prefix(egg_root_path)
                .expect("unable to strip egg prefix");
            components = rel_path
                .iter()
                .map(|p| p.to_str().expect("unable to get path as str"))
                .collect::<Vec<_>>();

            // Ignore EGG-INFO directory, as it is just packaging metadata.
            if components[0] == "EGG-INFO" {
                return None;
            }
        }

        let file_name = rel_path.file_name().unwrap().to_string_lossy();

        for ext_suffix in &self.suffixes.extension {
            if file_name.ends_with(ext_suffix) {
                let package_parts = &components[0..components.len() - 1];
                let mut package = itertools::join(package_parts, ".");

                let module_name = &file_name[0..file_name.len() - ext_suffix.len()];

                let mut full_module_name: Vec<&str> = package_parts.to_vec();

                if module_name != "__init__" {
                    full_module_name.push(module_name);
                }

                let full_module_name = itertools::join(full_module_name, ".");

                if package.is_empty() {
                    package = full_module_name.clone();
                }

                self.seen_packages.insert(package.clone());

                let module_components = full_module_name.split('.').collect::<Vec<_>>();
                let final_name = module_components[module_components.len() - 1];
                let init_fn = Some(format!("PyInit_{}", final_name));

                return Some(DirEntryItem::PythonResource(
                    PythonResource::ExtensionModuleDynamicLibrary(PythonExtensionModule {
                        name: full_module_name,
                        init_fn,
                        extension_file_suffix: ext_suffix.clone(),
                        extension_data: Some(DataLocation::Path(path.to_path_buf())),
                        object_file_data: vec![],
                        is_package: is_package_from_path(path),
                        libraries: vec![],
                        library_dirs: vec![],
                    }),
                ));
            }
        }

        // TODO use registered suffixes for source and bytecode detection.
        let resource = match rel_path.extension().and_then(OsStr::to_str) {
            Some("py") => {
                let package_parts = &components[0..components.len() - 1];
                let mut package = itertools::join(package_parts, ".");

                let module_name = rel_path
                    .file_stem()
                    .expect("unable to get file stem")
                    .to_str()
                    .expect("unable to convert path to str");

                let mut full_module_name: Vec<&str> = package_parts.to_vec();

                if module_name != "__init__" {
                    full_module_name.push(module_name);
                }

                let full_module_name = itertools::join(full_module_name, ".");

                if package.is_empty() {
                    package = full_module_name.clone();
                }

                self.seen_packages.insert(package.clone());

                DirEntryItem::PythonResource(PythonResource::ModuleSource(PythonModuleSource {
                    name: full_module_name,
                    source: DataLocation::Path(path.to_path_buf()),
                    is_package: is_package_from_path(&path),
                }))
            }
            Some("pyc") => {
                // .pyc files should be in a __pycache__ directory.
                if components.len() < 2 {
                    panic!("encountered .pyc file with invalid path: {}", rel_str);
                }

                // Possibly from Python 2?
                if components[components.len() - 2] != "__pycache__" {
                    return None;
                }

                let package_parts = &components[0..components.len() - 2];
                let mut package = itertools::join(package_parts, ".");

                // Files have format <package>/__pycache__/<module>.cpython-37.opt-1.pyc
                let module_name = rel_path
                    .file_stem()
                    .expect("unable to get file stem")
                    .to_str()
                    .expect("unable to convert file stem to str");
                let module_name_parts = module_name.split('.').collect_vec();
                let module_name =
                    itertools::join(&module_name_parts[0..module_name_parts.len() - 1], ".");

                let mut full_module_name: Vec<&str> = package_parts.to_vec();

                if module_name != "__init__" {
                    full_module_name.push(&module_name);
                }

                let full_module_name = itertools::join(full_module_name, ".");

                if package.is_empty() {
                    package = full_module_name.clone();
                }

                self.seen_packages.insert(package.clone());

                DirEntryItem::PythonResource(if rel_str.ends_with(".opt-1.pyc") {
                    PythonResource::ModuleBytecode(PythonModuleBytecode::from_path(
                        &full_module_name,
                        BytecodeOptimizationLevel::One,
                        path,
                    ))
                } else if rel_str.ends_with(".opt-2.pyc") {
                    PythonResource::ModuleBytecode(PythonModuleBytecode::from_path(
                        &full_module_name,
                        BytecodeOptimizationLevel::Two,
                        path,
                    ))
                } else {
                    PythonResource::ModuleBytecode(PythonModuleBytecode::from_path(
                        &full_module_name,
                        BytecodeOptimizationLevel::Zero,
                        path,
                    ))
                })
            }
            Some("egg") => DirEntryItem::PythonResource(PythonResource::EggFile(PythonEggFile {
                data: DataLocation::Path(path.to_path_buf()),
            })),
            Some("pth") => {
                DirEntryItem::PythonResource(PythonResource::PathExtension(PythonPathExtension {
                    data: DataLocation::Path(path.to_path_buf()),
                }))
            }
            _ => {
                // If it is some other file type, we categorize it as a resource
                // file. The package name and resource name are resolved later,
                // by the iterator.
                DirEntryItem::ResourceFile(ResourceFile {
                    full_path: path.to_path_buf(),
                    relative_path: rel_path.to_path_buf(),
                })
            }
        };

        Some(resource)
    }
}

impl Iterator for PythonResourceIterator {
    type Item = Result<PythonResource>;

    fn next(&mut self) -> Option<Result<PythonResource>> {
        // Our strategy is to walk directory entries and buffer resource files locally.
        // We then emit those at the end, perhaps doing some post-processing along the
        // way.
        loop {
            let res = self.walkdir_result.next();

            // We're out of directory entries;
            if res.is_none() {
                break;
            }

            let entry = res.unwrap();
            let entry = self.resolve_dir_entry(entry);

            // Try the next directory entry.
            if entry.is_none() {
                continue;
            }

            let entry = entry?;

            // Buffer Resource entries until later.
            match entry {
                DirEntryItem::ResourceFile(resource) => {
                    self.resources.push(resource);
                }
                DirEntryItem::PythonResource(resource) => {
                    return Some(Ok(resource));
                }
            }
        }

        loop {
            if self.resources.is_empty() {
                return None;
            }

            // This isn't efficient. But we shouldn't care.
            let resource = self.resources.remove(0);

            // Resource addressing in Python is a bit wonky. This is because the resource
            // reading APIs allow loading resources across package and directory boundaries.
            // For example, let's say we have a resource defined at the relative path
            // `foo/bar/resource.txt`. This resource could be accessed via the following
            // mechanisms:
            //
            // * Via the `resource.txt` resource on package `bar`'s resource reader.
            // * Via the `bar/resource.txt` resource on package `foo`'s resource reader.
            // * Via the `foo/bar/resource.txt` resource on the root resource reader.
            //
            // Furthermore, there could be resources in subdirectories that don't have
            // Python packages, forcing directory separators in resource names. e.g.
            // `foo/bar/resources/baz.txt`, where there isn't a `foo.bar.resources` Python
            // package.
            //
            // Our strategy for handling this is to initially resolve the relative path to
            // the resource. Then when we get to this code, we have awareness of all Python
            // packages and can supplement the relative path (which is the one true resource
            // identifier) with annotations, such as the leaf-most Python package.

            // Resources should always have a filename component. Otherwise how did we get here?
            let basename = resource
                .relative_path
                .file_name()
                .unwrap()
                .to_string_lossy();

            // The full name of the resource is its relative path with path separators normalized to
            // POSIX conventions.
            let full_name = resource.relative_path.to_string_lossy().replace("\\", "/");

            // We also resolve the leaf-most Python package that this resource is within and
            // the relative path within that package.
            let (leaf_package, relative_name) =
                if let Some(relative_directory) = resource.relative_path.parent() {
                    // We walk relative directory components until we find a Python package.
                    let mut components = relative_directory
                        .iter()
                        .map(|p| p.to_string_lossy())
                        .collect::<Vec<_>>();

                    let mut relative_components = vec![basename];
                    let mut package = None;
                    let mut relative_name = None;

                    while !components.is_empty() {
                        let candidate_package = itertools::join(&components, ".");

                        if self.seen_packages.contains(&candidate_package) {
                            package = Some(candidate_package);
                            relative_components.reverse();
                            relative_name = Some(itertools::join(&relative_components, "/"));
                            break;
                        }

                        let popped = components.pop().unwrap();
                        relative_components.push(popped);
                    }

                    (package, relative_name)
                } else {
                    (None, None)
                };

            // Resources without a resolved package are not legal.
            if leaf_package.is_none() {
                continue;
            }

            let leaf_package = leaf_package.unwrap();
            let relative_name = relative_name.unwrap();

            return Some(Ok(PythonResource::Resource(PythonPackageResource {
                full_name,
                leaf_package,
                relative_name,
                data: DataLocation::Path(resource.full_path),
            })));
        }
    }
}

/// Find Python resources in a directory.
///
/// Given a root directory path, walk the directory and find all Python
/// resources in it.
///
/// A resource is a Python source file, bytecode file, or resource file which
/// can be addressed via the ``A.B.C`` naming convention.
///
/// Returns an iterator of ``PythonResource`` instances.
pub fn find_python_resources(
    root_path: &Path,
    suffixes: &PythonModuleSuffixes,
) -> PythonResourceIterator {
    PythonResourceIterator::new(root_path, suffixes)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        lazy_static::lazy_static,
        std::fs::{create_dir_all, write},
    };

    lazy_static! {
        static ref EMPTY_SUFFIXES: PythonModuleSuffixes = PythonModuleSuffixes {
            source: vec![],
            bytecode: vec![],
            debug_bytecode: vec![],
            optimized_bytecode: vec![],
            extension: vec![],
        };
    }

    #[test]
    fn test_source_resolution() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let acme_path = tp.join("acme");
        let acme_a_path = acme_path.join("a");
        let acme_bar_path = acme_path.join("bar");

        create_dir_all(&acme_a_path).unwrap();
        create_dir_all(&acme_bar_path).unwrap();

        write(acme_path.join("__init__.py"), "")?;
        write(acme_a_path.join("__init__.py"), "")?;
        write(acme_bar_path.join("__init__.py"), "")?;

        write(acme_a_path.join("foo.py"), "# acme.foo")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 4);

        assert_eq!(
            resources[0],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "acme".to_string(),
                source: DataLocation::Path(acme_path.join("__init__.py")),
                is_package: true,
            })
        );
        assert_eq!(
            resources[1],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "acme.a".to_string(),
                source: DataLocation::Path(acme_a_path.join("__init__.py")),
                is_package: true,
            })
        );
        assert_eq!(
            resources[2],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "acme.a.foo".to_string(),
                source: DataLocation::Path(acme_a_path.join("foo.py")),
                is_package: false,
            })
        );
        assert_eq!(
            resources[3],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "acme.bar".to_string(),
                source: DataLocation::Path(acme_bar_path.join("__init__.py")),
                is_package: true,
            })
        );

        Ok(())
    }

    #[test]
    fn test_site_packages() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let sp_path = tp.join("site-packages");
        let acme_path = sp_path.join("acme");

        create_dir_all(&acme_path).unwrap();

        write(acme_path.join("__init__.py"), "")?;
        write(acme_path.join("bar.py"), "")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 2);

        assert_eq!(
            resources[0],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "acme".to_string(),
                source: DataLocation::Path(acme_path.join("__init__.py")),
                is_package: true,
            })
        );
        assert_eq!(
            resources[1],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "acme.bar".to_string(),
                source: DataLocation::Path(acme_path.join("bar.py")),
                is_package: false,
            })
        );

        Ok(())
    }

    #[test]
    fn test_extension_module() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        create_dir_all(&tp.join("markupsafe"))?;

        let pyd_path = tp.join("foo.pyd");
        let so_path = tp.join("bar.so");
        let cffi_path = tp.join("_cffi_backend.cp37-win_amd64.pyd");
        let markupsafe_speedups_path = tp
            .join("markupsafe")
            .join("_speedups.cpython-37m-x86_64-linux-gnu.so");
        let zstd_path = tp.join("zstd.cpython-37m-x86_64-linux-gnu.so");

        write(&pyd_path, "")?;
        write(&so_path, "")?;
        write(&cffi_path, "")?;
        write(&markupsafe_speedups_path, "")?;
        write(&zstd_path, "")?;

        let suffixes = PythonModuleSuffixes {
            source: vec![],
            bytecode: vec![],
            debug_bytecode: vec![],
            optimized_bytecode: vec![],
            extension: vec![
                ".cp37-win_amd64.pyd".to_string(),
                ".cp37-win32.pyd".to_string(),
                ".cpython-37m-x86_64-linux-gnu.so".to_string(),
                ".pyd".to_string(),
                ".so".to_string(),
            ],
        };

        let resources = PythonResourceIterator::new(tp, &suffixes).collect::<Result<Vec<_>>>()?;

        assert_eq!(resources.len(), 5);

        assert_eq!(
            resources[0],
            PythonResource::ExtensionModuleDynamicLibrary(PythonExtensionModule {
                name: "_cffi_backend".to_string(),
                init_fn: Some("PyInit__cffi_backend".to_string()),
                extension_file_suffix: ".cp37-win_amd64.pyd".to_string(),
                extension_data: Some(DataLocation::Path(cffi_path)),
                object_file_data: vec![],
                is_package: false,
                libraries: vec![],
                library_dirs: vec![],
            })
        );
        assert_eq!(
            resources[1],
            PythonResource::ExtensionModuleDynamicLibrary(PythonExtensionModule {
                name: "bar".to_string(),
                init_fn: Some("PyInit_bar".to_string()),
                extension_file_suffix: ".so".to_string(),
                extension_data: Some(DataLocation::Path(so_path)),
                object_file_data: vec![],
                is_package: false,
                libraries: vec![],
                library_dirs: vec![],
            }),
        );
        assert_eq!(
            resources[2],
            PythonResource::ExtensionModuleDynamicLibrary(PythonExtensionModule {
                name: "foo".to_string(),
                init_fn: Some("PyInit_foo".to_string()),
                extension_file_suffix: ".pyd".to_string(),
                extension_data: Some(DataLocation::Path(pyd_path)),
                object_file_data: vec![],
                is_package: false,
                libraries: vec![],
                library_dirs: vec![],
            }),
        );
        assert_eq!(
            resources[3],
            PythonResource::ExtensionModuleDynamicLibrary(PythonExtensionModule {
                name: "markupsafe._speedups".to_string(),
                init_fn: Some("PyInit__speedups".to_string()),
                extension_file_suffix: ".cpython-37m-x86_64-linux-gnu.so".to_string(),
                extension_data: Some(DataLocation::Path(markupsafe_speedups_path)),
                object_file_data: vec![],
                is_package: false,
                libraries: vec![],
                library_dirs: vec![],
            }),
        );
        assert_eq!(
            resources[4],
            PythonResource::ExtensionModuleDynamicLibrary(PythonExtensionModule {
                name: "zstd".to_string(),
                init_fn: Some("PyInit_zstd".to_string()),
                extension_file_suffix: ".cpython-37m-x86_64-linux-gnu.so".to_string(),
                extension_data: Some(DataLocation::Path(zstd_path)),
                object_file_data: vec![],
                is_package: false,
                libraries: vec![],
                library_dirs: vec![],
            }),
        );

        Ok(())
    }

    #[test]
    fn test_egg_file() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        create_dir_all(&tp)?;

        let egg_path = tp.join("foo-1.0-py3.7.egg");
        write(&egg_path, "")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 1);

        assert_eq!(
            resources[0],
            PythonResource::EggFile(PythonEggFile {
                data: DataLocation::Path(egg_path)
            })
        );

        Ok(())
    }

    #[test]
    fn test_egg_dir() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        create_dir_all(&tp)?;

        let egg_path = tp.join("site-packages").join("foo-1.0-py3.7.egg");
        let egg_info_path = egg_path.join("EGG-INFO");
        let package_path = egg_path.join("foo");

        create_dir_all(&egg_info_path)?;
        create_dir_all(&package_path)?;

        write(egg_info_path.join("PKG-INFO"), "")?;
        write(package_path.join("__init__.py"), "")?;
        write(package_path.join("bar.py"), "")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 2);

        assert_eq!(
            resources[0],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "foo".to_string(),
                source: DataLocation::Path(package_path.join("__init__.py")),
                is_package: true,
            })
        );
        assert_eq!(
            resources[1],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "foo.bar".to_string(),
                source: DataLocation::Path(package_path.join("bar.py")),
                is_package: false,
            })
        );

        Ok(())
    }

    #[test]
    fn test_pth_file() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        create_dir_all(&tp)?;

        let pth_path = tp.join("foo.pth");
        write(&pth_path, "")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 1);

        assert_eq!(
            resources[0],
            PythonResource::PathExtension(PythonPathExtension {
                data: DataLocation::Path(pth_path)
            })
        );

        Ok(())
    }

    /// Resource files without a package are not valid.
    #[test]
    fn test_root_resource_file() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let resource_path = tp.join("resource.txt");
        write(&resource_path, "content")?;

        let resources = PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Vec<_>>();
        assert!(resources.is_empty());

        Ok(())
    }

    /// Resource files in a relative directory without a package are not valid.
    #[test]
    fn test_relative_resource_no_package() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        write(&tp.join("foo.py"), "")?;
        let resource_dir = tp.join("resources");
        create_dir_all(&resource_dir)?;

        let resource_path = resource_dir.join("resource.txt");
        write(&resource_path, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 1);

        assert_eq!(
            resources[0],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "foo".to_string(),
                source: DataLocation::Path(tp.join("foo.py")),
                is_package: false,
            })
        );

        Ok(())
    }

    /// Resource files next to a package are detected.
    #[test]
    fn test_relative_package_resource() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let package_dir = tp.join("foo");
        create_dir_all(&package_dir)?;

        let module_path = package_dir.join("__init__.py");
        write(&module_path, "")?;
        let resource_path = package_dir.join("resource.txt");
        write(&resource_path, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;

        assert_eq!(resources.len(), 2);
        assert_eq!(
            resources[0],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "foo".to_string(),
                source: DataLocation::Path(module_path),
                is_package: true,
            })
        );
        assert_eq!(
            resources[1],
            PythonResource::Resource(PythonPackageResource {
                full_name: "foo/resource.txt".to_string(),
                leaf_package: "foo".to_string(),
                relative_name: "resource.txt".to_string(),
                data: DataLocation::Path(resource_path),
            })
        );

        Ok(())
    }

    /// Resource files in sub-directory are detected.
    #[test]
    fn test_subdirectory_resource() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let package_dir = tp.join("foo");
        let subdir = package_dir.join("resources");
        create_dir_all(&subdir)?;

        let module_path = package_dir.join("__init__.py");
        write(&module_path, "")?;
        let resource_path = subdir.join("resource.txt");
        write(&resource_path, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;

        assert_eq!(resources.len(), 2);
        assert_eq!(
            resources[0],
            PythonResource::ModuleSource(PythonModuleSource {
                name: "foo".to_string(),
                source: DataLocation::Path(module_path),
                is_package: true,
            }),
        );
        assert_eq!(
            resources[1],
            PythonResource::Resource(PythonPackageResource {
                full_name: "foo/resources/resource.txt".to_string(),
                leaf_package: "foo".to_string(),
                relative_name: "resources/resource.txt".to_string(),
                data: DataLocation::Path(resource_path),
            })
        );

        Ok(())
    }

    /// .dist-info directory ignored if METADATA file not present.
    #[test]
    fn test_distinfo_missing_metadata() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let dist_path = tp.join("foo-1.2.dist-info");
        create_dir_all(&dist_path)?;
        let resource = dist_path.join("file.txt");
        write(&resource, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert!(resources.is_empty());

        Ok(())
    }

    /// .dist-info with invalid METADATA file has no content emitted.
    #[test]
    fn test_distinfo_bad_metadata() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let dist_path = tp.join("foo-1.2.dist-info");
        create_dir_all(&dist_path)?;
        let metadata = dist_path.join("METADATA");
        write(&metadata, "bad content")?;
        let resource = dist_path.join("file.txt");
        write(&resource, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert!(resources.is_empty());

        Ok(())
    }

    /// .dist-info with partial METADATA content has no content emitted.
    #[test]
    fn test_distinfo_partial_metadata() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let dist_path = tp.join("black-1.2.3.dist-info");
        create_dir_all(&dist_path)?;
        let metadata = dist_path.join("METADATA");
        write(&metadata, "Name: black\n")?;
        let resource = dist_path.join("file.txt");
        write(&resource, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert!(resources.is_empty());

        Ok(())
    }

    /// .dist-info with partial METADATA content has no content emitted.
    #[test]
    fn test_distinfo_valid_metadata() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let dist_path = tp.join("black-1.2.3.dist-info");
        create_dir_all(&dist_path)?;
        let metadata_path = dist_path.join("METADATA");
        write(&metadata_path, "Name: black\nVersion: 1.2.3\n")?;
        let resource_path = dist_path.join("file.txt");
        write(&resource_path, "content")?;

        let subdir = dist_path.join("subdir");
        create_dir_all(&subdir)?;
        let subdir_resource_path = subdir.join("sub.txt");
        write(&subdir_resource_path, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 3);

        assert_eq!(
            resources[0],
            PythonResource::DistributionResource(PythonPackageDistributionResource {
                location: PythonPackageDistributionResourceFlavor::DistInfo,
                package: "black".to_string(),
                version: "1.2.3".to_string(),
                name: "METADATA".to_string(),
                data: DataLocation::Path(metadata_path),
            })
        );
        assert_eq!(
            resources[1],
            PythonResource::DistributionResource(PythonPackageDistributionResource {
                location: PythonPackageDistributionResourceFlavor::DistInfo,
                package: "black".to_string(),
                version: "1.2.3".to_string(),
                name: "file.txt".to_string(),
                data: DataLocation::Path(resource_path),
            })
        );
        assert_eq!(
            resources[2],
            PythonResource::DistributionResource(PythonPackageDistributionResource {
                location: PythonPackageDistributionResourceFlavor::DistInfo,
                package: "black".to_string(),
                version: "1.2.3".to_string(),
                name: "subdir/sub.txt".to_string(),
                data: DataLocation::Path(subdir_resource_path),
            })
        );

        Ok(())
    }

    /// .dist-info with partial METADATA content has no content emitted.
    #[test]
    fn test_egginfo_valid_metadata() -> Result<()> {
        let td = tempdir::TempDir::new("pyoxidizer-test")?;
        let tp = td.path();

        let egg_path = tp.join("black-1.2.3.egg-info");
        create_dir_all(&egg_path)?;
        let metadata_path = egg_path.join("PKG-INFO");
        write(&metadata_path, "Name: black\nVersion: 1.2.3\n")?;
        let resource_path = egg_path.join("file.txt");
        write(&resource_path, "content")?;

        let subdir = egg_path.join("subdir");
        create_dir_all(&subdir)?;
        let subdir_resource_path = subdir.join("sub.txt");
        write(&subdir_resource_path, "content")?;

        let resources =
            PythonResourceIterator::new(tp, &EMPTY_SUFFIXES).collect::<Result<Vec<_>>>()?;
        assert_eq!(resources.len(), 3);

        assert_eq!(
            resources[0],
            PythonResource::DistributionResource(PythonPackageDistributionResource {
                location: PythonPackageDistributionResourceFlavor::EggInfo,
                package: "black".to_string(),
                version: "1.2.3".to_string(),
                name: "PKG-INFO".to_string(),
                data: DataLocation::Path(metadata_path),
            })
        );
        assert_eq!(
            resources[1],
            PythonResource::DistributionResource(PythonPackageDistributionResource {
                location: PythonPackageDistributionResourceFlavor::EggInfo,
                package: "black".to_string(),
                version: "1.2.3".to_string(),
                name: "file.txt".to_string(),
                data: DataLocation::Path(resource_path),
            })
        );
        assert_eq!(
            resources[2],
            PythonResource::DistributionResource(PythonPackageDistributionResource {
                location: PythonPackageDistributionResourceFlavor::EggInfo,
                package: "black".to_string(),
                version: "1.2.3".to_string(),
                name: "subdir/sub.txt".to_string(),
                data: DataLocation::Path(subdir_resource_path),
            })
        );

        Ok(())
    }
}
