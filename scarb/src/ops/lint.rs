use std::vec;

use crate::{
    compiler::{
        CompilationUnit, CompilationUnitAttributes,
        db::{ScarbDatabase, build_scarb_root_database},
    },
    core::{PackageId, PackageName, TargetKind},
    ops,
};

use anyhow::anyhow;
use anyhow::{Context, Result};
use cairo_lang_compiler::db::RootDatabase;
use cairo_lang_defs::db::DefsGroup;
use cairo_lang_diagnostics::{DiagnosticEntry, Severity};
use cairo_lang_formatter::FormatterConfig;
use cairo_lang_semantic::{SemanticDiagnostic, db::SemanticGroup};
use cairo_lint::CAIRO_LINT_TOOL_NAME;
use cairo_lint::{
    CairoLintToolMetadata, apply_file_fixes, diagnostics::format_diagnostic, get_fixes,
    plugin::cairo_lint_plugin_suite,
};
use camino::Utf8PathBuf;
use itertools::Itertools;
use scarb_ui::components::Status;

use crate::core::{Package, Workspace};
use crate::internal::fsx::canonicalize;

use super::{
    CompilationUnitsOpts, FeaturesOpts, compile_unit, plugins_required_for_units, validate_features,
};

struct CompilationUnitDiagnostics {
    pub db: RootDatabase,
    pub diagnostics: Vec<SemanticDiagnostic>,
    pub formatter_config: FormatterConfig,
}

pub struct LintOptions {
    pub packages: Vec<Package>,
    pub target_names: Vec<String>,
    pub test: bool,
    pub fix: bool,
    pub ignore_cairo_version: bool,
    pub features: FeaturesOpts,
    pub deny_warnings: bool,
    pub path: Option<Utf8PathBuf>,
}

#[tracing::instrument(skip_all, level = "debug")]
pub fn lint(opts: LintOptions, ws: &Workspace<'_>) -> Result<()> {
    let resolve = ops::resolve_workspace(ws)?;

    validate_features(&opts.packages, &opts.features)?;

    let compilation_units = ops::generate_compilation_units(
        &resolve,
        &opts.features,
        ws,
        CompilationUnitsOpts {
            ignore_cairo_version: opts.ignore_cairo_version,
            load_prebuilt_macros: ws.config().load_prebuilt_proc_macros(),
        },
    )?;

    let absolute_path = opts.path.map(canonicalize).transpose()?;

    // Select proc macro units that need to be compiled for Cairo compilation units.
    let required_plugins = plugins_required_for_units(&compilation_units);

    // We process all proc-macro units that are required by Cairo compilation units beforehand.
    for compilation_unit in compilation_units.iter() {
        if let CompilationUnit::ProcMacro(_) = compilation_unit {
            if required_plugins.contains(&compilation_unit.main_package_id()) {
                compile_unit(compilation_unit.clone(), ws)?;
            }
        }
    }

    // We store the state of the workspace diagnostics, so we can decide upon throwing an error later on.
    // Also we want to apply fixes only if there were no previous errors.
    let mut packages_with_error: Vec<PackageName> = Default::default();
    let mut diagnostics_per_cu: Vec<CompilationUnitDiagnostics> = Default::default();

    for package in opts.packages {
        let package_name = &package.id.name;
        let formatter_config = package.fmt_config()?;
        let package_compilation_units = if opts.test {
            let mut result = vec![];
            let integration_test_compilation_unit =
                find_integration_test_package_id(&package).map(|id| {
                    compilation_units
                        .iter()
                        .find(|compilation_unit| compilation_unit.main_package_id() == id)
                        .unwrap()
                });

            // We also want to get the main compilation unit for the package.
            if let Some(cu) = compilation_units.iter().find(|compilation_unit| {
                compilation_unit.main_package_id() == package.id
                    && compilation_unit.main_component().target_kind() != TargetKind::TEST
            }) {
                result.push(cu)
            }

            // We get all the compilation units with target kind set to "test".
            result.extend(compilation_units.iter().filter(|compilation_unit| {
                compilation_unit.main_package_id() == package.id
                    && compilation_unit.main_component().target_kind() == TargetKind::TEST
            }));

            // If any integration test compilation unit was found, we add it to the result.
            if let Some(integration_test_compilation_unit) = integration_test_compilation_unit {
                result.push(integration_test_compilation_unit);
            }

            // If there is no compilation unit for the package, we skip it.
            if result.is_empty() {
                ws.config()
                    .ui()
                    .print(Status::new("Skipping package", package_name.as_str()));
                continue;
            }

            result
        } else {
            let found_compilation_unit =
                compilation_units
                    .iter()
                    .find(|compilation_unit| match compilation_unit {
                        CompilationUnit::Cairo(compilation_unit) => {
                            compilation_unit.main_package_id() == package.id
                                && compilation_unit.main_component().target_kind()
                                    != TargetKind::TEST
                        }
                        _ => false,
                    });

            // If there is no compilation unit for the package, we skip it.
            match found_compilation_unit {
                Some(cu) => vec![cu],
                None => {
                    ws.config()
                        .ui()
                        .print(Status::new("Skipping package", package_name.as_str()));
                    continue;
                }
            }
        };

        let filtered_by_target_names_package_compilation_units = if opts.target_names.is_empty() {
            package_compilation_units
        } else {
            package_compilation_units
                .into_iter()
                .filter(|compilation_unit| {
                    compilation_unit
                        .main_component()
                        .targets
                        .targets()
                        .iter()
                        .any(|t| opts.target_names.contains(&t.name.to_string()))
                })
                .collect::<Vec<_>>()
        };

        for compilation_unit in filtered_by_target_names_package_compilation_units {
            match compilation_unit {
                CompilationUnit::ProcMacro(_) => {
                    continue;
                }
                CompilationUnit::Cairo(compilation_unit) => {
                    ws.config()
                        .ui()
                        .print(Status::new("Linting", &compilation_unit.name()));

                    let additional_plugins = vec![cairo_lint_plugin_suite(
                        cairo_lint_tool_metadata(&package)?,
                    )?];
                    let ScarbDatabase { db, .. } =
                        build_scarb_root_database(compilation_unit, ws, additional_plugins)?;

                    let main_component = compilation_unit.main_component();
                    let crate_id = main_component.crate_id(&db);

                    // Diagnostics generated by the `cairo-lint` plugin.
                    // Only user-defined code is included, since virtual files are filtered by the `linter`.
                    let diags = db
                        .crate_modules(crate_id)
                        .iter()
                        .flat_map(|module_id| db.module_semantic_diagnostics(*module_id).ok())
                        .flat_map(|diags| diags.get_all())
                        .collect_vec();

                    // Filter diagnostics if `SCARB_ACTION_PATH` was provided.
                    let diagnostics = match &absolute_path {
                        Some(path) => diags
                            .into_iter()
                            .filter(|diag| {
                                let file_id = diag.stable_location.file_id(&db);

                                if let Ok(diag_path) = canonicalize(file_id.full_path(&db)) {
                                    (path.is_dir() && diag_path.starts_with(path))
                                        || (path.is_file() && diag_path == *path)
                                } else {
                                    false
                                }
                            })
                            .collect::<Vec<_>>(),
                        None => diags,
                    };

                    // Display diagnostics.
                    for diag in &diagnostics {
                        match diag.severity() {
                            Severity::Error => {
                                if let Some(code) = diag.error_code() {
                                    ws.config().ui().error_with_code(
                                        code.as_str(),
                                        format_diagnostic(diag, &db),
                                    )
                                } else {
                                    ws.config().ui().error(format_diagnostic(diag, &db))
                                }
                            }
                            Severity::Warning => {
                                if let Some(code) = diag.error_code() {
                                    ws.config()
                                        .ui()
                                        .warn_with_code(code.as_str(), format_diagnostic(diag, &db))
                                } else {
                                    ws.config().ui().warn(format_diagnostic(diag, &db))
                                }
                            }
                        }
                    }

                    let warnings_allowed =
                        compilation_unit.compiler_config.allow_warnings && !opts.deny_warnings;

                    if diagnostics.iter().any(|diag| {
                        matches!(diag.severity(), Severity::Error)
                            || (!warnings_allowed && matches!(diag.severity(), Severity::Warning))
                    }) {
                        packages_with_error.push(package_name.clone());
                    }
                    diagnostics_per_cu.push(CompilationUnitDiagnostics {
                        db,
                        diagnostics,
                        formatter_config: formatter_config.clone(),
                    });
                }
            }
        }
    }

    packages_with_error = packages_with_error
        .into_iter()
        .unique_by(|name| name.to_string())
        .collect();

    if !packages_with_error.is_empty() {
        if packages_with_error.len() == 1 {
            let package_name = packages_with_error[0].to_string();
            return Err(anyhow!(
                "lint checking `{package_name}` failed due to previous errors"
            ));
        } else {
            let package_names = packages_with_error
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!(
                "lint checking {package_names} packages failed due to previous errors"
            ));
        }
    }

    if opts.fix {
        for CompilationUnitDiagnostics {
            db,
            diagnostics,
            formatter_config,
        } in diagnostics_per_cu.into_iter()
        {
            let fixes = get_fixes(&db, diagnostics);
            for (file_id, fixes) in fixes.into_iter() {
                ws.config()
                    .ui()
                    .print(Status::new("Fixing", &file_id.file_name(&db)));
                apply_file_fixes(file_id, fixes, &db, formatter_config.clone())?;
            }
        }
    }

    Ok(())
}

fn cairo_lint_tool_metadata(package: &Package) -> Result<CairoLintToolMetadata> {
    Ok(package
        .tool_metadata(CAIRO_LINT_TOOL_NAME)
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .context("Failed to parse Cairo lint tool metadata")?
        .unwrap_or_default())
}

fn find_integration_test_package_id(package: &Package) -> Option<PackageId> {
    let integration_target = package.manifest.targets.iter().find(|target| {
        target.kind == TargetKind::TEST
            && target
                .params
                .get("test-type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                == "integration"
    });

    integration_target.map(|target| {
        package
            .id
            .for_test_target(target.group_id.clone().unwrap_or(target.name.clone()))
    })
}
