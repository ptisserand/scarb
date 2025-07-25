//! Various utility functions helpful for interacting with Cairo compiler.

use crate::compiler::{CairoCompilationUnit, CompilationUnitAttributes};
use crate::core::{InliningStrategy, Workspace};
use crate::flock::Filesystem;
use anyhow::{Context, Result};
use cairo_lang_compiler::CompilerConfig;
use cairo_lang_compiler::db::RootDatabase;
use cairo_lang_compiler::diagnostics::DiagnosticsReporter;
use cairo_lang_diagnostics::{FormattedDiagnosticEntry, Severity};
use cairo_lang_filesystem::db::FilesGroup;
use cairo_lang_filesystem::ids::CrateId;
use itertools::Itertools;
use serde::Serialize;
use std::collections::HashSet;
use std::io::{BufWriter, Write};

pub struct CountingWriter<W> {
    inner: W,
    pub byte_count: usize,
}

impl<W: Write> CountingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            byte_count: 0,
        }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.byte_count += n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub fn build_compiler_config<'c>(
    db: &RootDatabase,
    unit: &CairoCompilationUnit,
    main_crate_ids: &[CrateId],
    cached_crates: &[CrateId],
    ws: &Workspace<'c>,
) -> CompilerConfig<'c> {
    let ignore_warnings_crates = db
        .crates()
        .into_iter()
        .filter(|crate_id| !main_crate_ids.contains(crate_id))
        .collect_vec();
    let crates_to_check: HashSet<CrateId> = db
        .crates()
        .into_iter()
        .filter(|crate_id| !cached_crates.contains(crate_id))
        .chain(main_crate_ids.iter().cloned())
        .collect();
    let diagnostics_reporter = DiagnosticsReporter::callback({
        let config = ws.config();

        |entry: FormattedDiagnosticEntry| {
            let msg = entry
                .message()
                .strip_suffix('\n')
                .unwrap_or(entry.message());
            match entry.severity() {
                Severity::Error => {
                    if let Some(code) = entry.error_code() {
                        config.ui().error_with_code(code.as_str(), msg)
                    } else {
                        config.ui().error(msg)
                    }
                }
                Severity::Warning => {
                    if let Some(code) = entry.error_code() {
                        config.ui().warn_with_code(code.as_str(), msg)
                    } else {
                        config.ui().warn(msg)
                    }
                }
            };
        }
    })
    .with_ignore_warnings_crates(&ignore_warnings_crates)
    // If a crate is cached, we do not need to check it for diagnostics,
    // as the cache can only be produced if the crate is diagnostic-free.
    // So if there were any diagnotics here to show, it would mean that the cache is outdated - thus
    // we should not use it in the first place.
    // Note we still add the main crate, as we want it to be checked for warnings.
    .with_crates(&crates_to_check.into_iter().collect_vec());
    CompilerConfig {
        diagnostics_reporter: if unit.compiler_config.allow_warnings {
            diagnostics_reporter.allow_warnings()
        } else {
            diagnostics_reporter
        },
        replace_ids: unit.compiler_config.sierra_replace_ids,
        inlining_strategy: unit.compiler_config.inlining_strategy.clone().into(),
        add_statements_functions: unit
            .compiler_config
            .unstable_add_statements_functions_debug_info,
        add_statements_code_locations: unit
            .compiler_config
            .unstable_add_statements_code_locations_debug_info,
        ..CompilerConfig::default()
    }
}

impl From<InliningStrategy> for cairo_lang_lowering::utils::InliningStrategy {
    fn from(value: InliningStrategy) -> Self {
        match value {
            InliningStrategy::Default => cairo_lang_lowering::utils::InliningStrategy::Default,
            InliningStrategy::Avoid => cairo_lang_lowering::utils::InliningStrategy::Avoid,
            InliningStrategy::InlineSmallFunctions(weight) => {
                cairo_lang_lowering::utils::InliningStrategy::InlineSmallFunctions(weight)
            }
        }
    }
}

#[allow(unused)]
impl From<cairo_lang_lowering::utils::InliningStrategy> for InliningStrategy {
    fn from(value: cairo_lang_lowering::utils::InliningStrategy) -> Self {
        match value {
            cairo_lang_lowering::utils::InliningStrategy::Default => InliningStrategy::Default,
            cairo_lang_lowering::utils::InliningStrategy::Avoid => InliningStrategy::Avoid,
            cairo_lang_lowering::utils::InliningStrategy::InlineSmallFunctions(weight) => {
                InliningStrategy::InlineSmallFunctions(weight)
            }
        }
    }
}

pub fn collect_main_crate_ids(unit: &CairoCompilationUnit, db: &RootDatabase) -> Vec<CrateId> {
    let main_component = unit.main_component();
    vec![main_component.crate_id(db)]
}

pub fn write_json(
    file_name: &str,
    description: &str,
    target_dir: &Filesystem,
    ws: &Workspace<'_>,
    value: impl Serialize,
) -> Result<()> {
    let file = target_dir.create_rw(file_name, description, ws.config())?;
    let file = BufWriter::new(&*file);
    serde_json::to_writer(file, &value)
        .with_context(|| format!("failed to serialize {file_name}"))?;
    Ok(())
}

pub fn write_json_with_byte_count(
    file_name: &str,
    description: &str,
    target_dir: &Filesystem,
    ws: &Workspace<'_>,
    value: impl Serialize,
) -> Result<usize> {
    let file = target_dir.create_rw(file_name, description, ws.config())?;
    let file = BufWriter::new(&*file);
    let mut writer = CountingWriter::new(file);
    serde_json::to_writer(&mut writer, &value)
        .with_context(|| format!("failed to serialize {file_name}"))?;
    Ok(writer.byte_count)
}

pub fn write_string(
    file_name: &str,
    description: &str,
    target_dir: &Filesystem,
    ws: &Workspace<'_>,
    value: impl ToString,
) -> Result<()> {
    let mut file = target_dir.create_rw(file_name, description, ws.config())?;
    file.write_all(value.to_string().as_bytes())?;
    Ok(())
}
