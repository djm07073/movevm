use crate::{docgen::DocgenPackage, extended_checks};
use anyhow::bail;
use codespan_reporting::{
    diagnostic::Severity,
    term::termcolor::{ColorChoice, StandardStream},
};
use initia_move_types::metadata::{
    self, RuntimeModuleMetadataV0, INITIA_METADATA_KEY_V0, METADATA_V0_MIN_FILE_FORMAT_VERSION,
};
use itertools::Itertools;
use legacy_move_compiler::compiled_unit::{CompiledUnit, NamedCompiledModule};
use move_binary_format::CompiledModule;
use move_command_line_common::files::MOVE_COMPILED_EXTENSION;
use move_core_types::{language_storage::ModuleId, metadata::Metadata};
use move_docgen::DocgenOptions;
use move_model::metadata::{CompilerVersion, LanguageVersion};
use move_package::{
    compilation::{compiled_package::CompiledPackage, package_layout::CompiledPackageLayout},
    resolution::resolution_graph::ResolvedGraph,
    BuildConfig,
};
use std::{
    collections::BTreeMap,
    io::stderr,
    path::{Path, PathBuf},
};

/// Represents a built package.  It allows to extract `PackageMetadata`. Can also be used to
/// just build Move code and related artifacts.
pub struct BuiltPackage {
    config: BuildConfig,
    package_path: PathBuf,
    package: CompiledPackage,
}

impl BuiltPackage {
    /// Builds the package and on success delivers a `BuiltPackage`.
    ///
    /// This function currently reports all Move compilation errors and warnings to stdout,
    /// and is not `Ok` if there was an error among those.
    pub fn build(
        package_path: PathBuf,
        config: BuildConfig,
        docgen_options: Option<DocgenOptions>,
    ) -> anyhow::Result<Self> {
        eprintln!("Compiling, may take a little while to download git dependencies...");
        let generate_docs = config.generate_docs || docgen_options.is_some();
        let bytecode_version = config.compiler_config.bytecode_version;

        // customize config
        let mut new_config = config.clone();
        new_config.generate_docs = false;
        new_config.generate_move_model = true;
        new_config
            .compiler_config
            .known_attributes
            .clone_from(metadata::get_all_attribute_names());

        // use v2 as default
        if new_config.compiler_config.compiler_version.is_none() {
            new_config.compiler_config.compiler_version = Some(CompilerVersion::V2_0);
        }
        if new_config.compiler_config.language_version.is_none() {
            new_config.compiler_config.language_version = Some(LanguageVersion::V2_0);
        }

        let resolved_graph = Self::prepare_resolution_graph(&package_path, new_config.clone())?;
        let (mut package, model_opt) =
            new_config.compile_package_no_exit(resolved_graph, vec![], &mut stderr())?;

        // Run extended checks as well as derive runtime metadata
        let model = &model_opt.expect("move model");
        let runtime_metadata = extended_checks::run_extended_checks(model);
        if model.diag_count(Severity::Warning) > 0 {
            let mut error_writer = StandardStream::stderr(ColorChoice::Auto);
            model.report_diag(&mut error_writer, Severity::Warning);
            if model.has_errors() {
                bail!("extended checks failed")
            }
        }

        let compiled_pkg_path = package
            .compiled_package_info
            .build_flags
            .install_dir
            .as_ref()
            .unwrap_or(&package_path)
            .join(CompiledPackageLayout::Root.path())
            .join(package.compiled_package_info.package_name.as_str());

        inject_runtime_metadata(
            compiled_pkg_path,
            &mut package,
            runtime_metadata,
            bytecode_version,
        )?;

        // If enabled generate docs.
        if generate_docs {
            let docgen = docgen_options.unwrap_or_default();
            let dep_paths = package
                .deps_compiled_units
                .iter()
                .map(|(_, u)| {
                    u.source_path
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .join("doc")
                        .display()
                        .to_string()
                })
                .unique()
                .collect::<Vec<_>>();

            DocgenPackage {
                docgen_options: docgen,
                package_path: package_path.clone(),
            }
            .generate_docs(dep_paths, model)?
        }

        Ok(Self {
            config,
            package_path,
            package,
        })
    }

    pub fn prepare_resolution_graph(
        package_path: &Path,
        build_config: BuildConfig,
    ) -> anyhow::Result<ResolvedGraph> {
        eprintln!("Compiling, may take a little while to download git dependencies...");
        build_config.resolution_graph_for_package(package_path, &mut stderr())
    }

    /// Returns the name of this package.
    pub fn name(&self) -> &str {
        self.package.compiled_package_info.package_name.as_str()
    }

    pub fn package_path(&self) -> &Path {
        self.package_path.as_path()
    }

    pub fn package_artifacts_path(&self) -> PathBuf {
        self.package_path
            .join(CompiledPackageLayout::Root.path())
            .join(self.name())
    }

    /// Extracts the bytecode for the modules of the built package.
    pub fn extract_code(&self) -> Vec<Vec<u8>> {
        self.package
            .root_modules()
            .map(|unit_with_source| {
                unit_with_source
                    .unit
                    .serialize(self.config.compiler_config.bytecode_version)
            })
            .collect()
    }

    /// Returns an iterator for all compiled proper (non-script) modules.
    pub fn modules(&self) -> impl Iterator<Item = &CompiledModule> {
        self.package
            .root_modules()
            .filter_map(|unit| match &unit.unit {
                CompiledUnit::Module(NamedCompiledModule { module, .. }) => Some(module),
                CompiledUnit::Script(_) => None,
            })
    }

    /// Returns the number of scripts in the package.
    pub fn script_count(&self) -> usize {
        self.package.scripts().count()
    }

    /// Returns the serialized bytecode of the scripts in the package.
    pub fn extract_script_code(&self) -> Vec<Vec<u8>> {
        self.package
            .scripts()
            .map(|unit_with_source| {
                unit_with_source
                    .unit
                    .serialize(self.config.compiler_config.bytecode_version)
            })
            .collect()
    }
}

fn inject_runtime_metadata(
    package_path: PathBuf,
    pack: &mut CompiledPackage,
    metadata: BTreeMap<ModuleId, RuntimeModuleMetadataV0>,
    bytecode_version: Option<u32>,
) -> anyhow::Result<()> {
    for unit_with_source in pack.root_compiled_units.iter_mut() {
        match &mut unit_with_source.unit {
            CompiledUnit::Module(named_module) => {
                if let Some(module_metadata) = metadata.get(&named_module.module.self_id()) {
                    if !module_metadata.is_empty() {
                        if bytecode_version.unwrap_or(METADATA_V0_MIN_FILE_FORMAT_VERSION)
                            >= METADATA_V0_MIN_FILE_FORMAT_VERSION
                        {
                            let serialized_metadata = bcs::to_bytes(&module_metadata)
                                .expect("BCS for RuntimeModuleMetadata");
                            named_module.module.metadata.push(Metadata {
                                key: INITIA_METADATA_KEY_V0.to_vec(),
                                value: serialized_metadata,
                            });
                        } else {
                            bail!("not supported bytecode version")
                        };

                        // Also need to update the .mv file on disk.
                        let path = package_path
                            .join(CompiledPackageLayout::CompiledModules.path())
                            .join(named_module.name.as_str())
                            .with_extension(MOVE_COMPILED_EXTENSION);
                        if path.is_file() {
                            let bytes = unit_with_source.unit.serialize(bytecode_version);
                            std::fs::write(path, bytes)?;
                        }
                    }
                }
            }
            CompiledUnit::Script(_) => {}
        }
    }
    Ok(())
}
