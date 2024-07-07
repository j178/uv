use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use itertools::Itertools;
use tracing::debug;

use distribution_types::Name;
use pypi_types::Requirement;
use uv_cache::Cache;
use uv_client::{BaseClientBuilder, Connectivity};
use uv_configuration::{Concurrency, PreviewMode};
#[cfg(unix)]
use uv_fs::replace_symlink;
use uv_fs::Simplified;
use uv_installer::SitePackages;
use uv_normalize::PackageName;
use uv_python::{
    EnvironmentPreference, PythonFetch, PythonInstallation, PythonPreference, PythonRequest,
};
use uv_requirements::RequirementsSpecification;
use uv_tool::{entrypoint_paths, find_executable_directory, InstalledTools, Tool, ToolEntrypoint};
use uv_warnings::warn_user_once;

use crate::commands::project::{resolve_environment, sync_environment, update_environment};
use crate::commands::reporters::DownloadReporter;
use crate::commands::tool::common::resolve_requirements;
use crate::commands::{ExitStatus, SharedState};
use crate::printer::Printer;
use crate::settings::ResolverInstallerSettings;

/// Install a tool.
pub(crate) async fn install(
    package: String,
    from: Option<String>,
    python: Option<String>,
    with: Vec<String>,
    force: bool,
    settings: ResolverInstallerSettings,
    preview: PreviewMode,
    python_preference: PythonPreference,
    python_fetch: PythonFetch,
    connectivity: Connectivity,
    concurrency: Concurrency,
    native_tls: bool,
    cache: &Cache,
    printer: Printer,
) -> Result<ExitStatus> {
    if preview.is_disabled() {
        warn_user_once!("`uv tool install` is experimental and may change without warning.");
    }

    let client_builder = BaseClientBuilder::new()
        .connectivity(connectivity)
        .native_tls(native_tls);

    let reporter = DownloadReporter::new(printer, 1);

    let python_request = python.as_deref().map(PythonRequest::parse);

    // Pre-emptively identify a Python interpreter. We need an interpreter to resolve any unnamed
    // requirements, even if we end up using a different interpreter for the tool install itself.
    let interpreter = PythonInstallation::find_or_fetch(
        python_request.clone(),
        EnvironmentPreference::OnlySystem,
        python_preference,
        python_fetch,
        &client_builder,
        cache,
        Some(&reporter),
    )
    .await?
    .into_interpreter();

    // Initialize any shared state.
    let state = SharedState::default();

    // Resolve the `from` requirement.
    let from = if let Some(from) = from {
        // Parse the positional name. If the user provided more than a package name, it's an error
        // (e.g., `uv install foo==1.0 --from foo`).
        let Ok(package) = PackageName::from_str(&package) else {
            bail!("Package requirement `{from}` provided with `--from` conflicts with install request `{package}`")
        };

        let from_requirement = resolve_requirements(
            std::iter::once(from.as_str()),
            &interpreter,
            &settings,
            &state,
            preview,
            connectivity,
            concurrency,
            native_tls,
            cache,
            printer,
        )
        .await?
        .pop()
        .unwrap();

        // Check if the positional name conflicts with `--from`.
        if from_requirement.name != package {
            // Determine if it's an entirely different package (e.g., `uv install foo --from bar`).
            bail!(
                "Package name `{}` provided with `--from` does not match install request `{}`",
                from_requirement.name,
                package
            );
        }

        from_requirement
    } else {
        resolve_requirements(
            std::iter::once(package.as_str()),
            &interpreter,
            &settings,
            &state,
            preview,
            connectivity,
            concurrency,
            native_tls,
            cache,
            printer,
        )
        .await?
        .pop()
        .unwrap()
    };

    // Combine the `from` and `with` requirements.
    let requirements = {
        let mut requirements = Vec::with_capacity(1 + with.len());
        requirements.push(from.clone());
        requirements.extend(
            resolve_requirements(
                with.iter().map(String::as_str),
                &interpreter,
                &settings,
                &state,
                preview,
                connectivity,
                concurrency,
                native_tls,
                cache,
                printer,
            )
            .await?,
        );
        requirements
    };

    let installed_tools = InstalledTools::from_settings()?;
    let existing_tool_receipt = installed_tools.get_tool_receipt(&from.name)?;
    let existing_environment =
        installed_tools
            .get_environment(&from.name, cache)?
            .filter(|environment| {
                python_request.as_ref().map_or(true, |python_request| {
                    if python_request.satisfied(environment.interpreter(), cache) {
                        debug!("Found existing environment for tool `{}`", from.name);
                        true
                    } else {
                        let _ = writeln!(
                            printer.stderr(),
                            "Existing environment for `{}` does not satisfy the requested Python interpreter: `{}`",
                            from.name,
                            python_request
                        );
                        false
                    }
                })
            });

    // If the requested and receipt requirements are the same...
    if existing_environment.is_some() {
        if let Some(tool_receipt) = existing_tool_receipt.as_ref() {
            let receipt = tool_receipt
                .requirements()
                .iter()
                .cloned()
                .map(Requirement::from)
                .collect::<Vec<_>>();
            if requirements == receipt {
                // And the user didn't request a reinstall or upgrade...
                if !force && settings.reinstall.is_none() && settings.upgrade.is_none() {
                    // We're done.
                    writeln!(printer.stderr(), "Tool `{from}` is already installed")?;
                    return Ok(ExitStatus::Failure);
                }
            }
        }
    }

    // Replace entrypoints if the tool already exists (and we made it this far). If we find existing
    // entrypoints later on, and the tool _doesn't_ exist, we'll avoid removing the external tool's
    // entrypoints (without `--force`).
    let reinstall_entry_points = existing_tool_receipt.is_some();

    // Resolve the requirements.
    let state = SharedState::default();
    let spec = RequirementsSpecification::from_requirements(requirements.clone());

    // TODO(zanieb): Build the environment in the cache directory then copy into the tool directory.
    // This lets us confirm the environment is valid before removing an existing install. However,
    // entrypoints always contain an absolute path to the relevant Python interpreter, which would
    // be invalidated by moving the environment.
    let environment = if let Some(environment) = existing_environment {
        update_environment(
            environment,
            spec,
            &settings,
            &state,
            preview,
            connectivity,
            concurrency,
            native_tls,
            cache,
            printer,
        )
        .await?
    } else {
        // If we're creating a new environment, ensure that we can resolve the requirements prior
        // to removing any existing tools.
        let resolution = resolve_environment(
            &interpreter,
            spec,
            settings.as_ref().into(),
            &state,
            preview,
            connectivity,
            concurrency,
            native_tls,
            cache,
            printer,
        )
        .await?;

        let environment = installed_tools.create_environment(&from.name, interpreter)?;

        // Sync the environment with the resolved requirements.
        sync_environment(
            environment,
            &resolution.into(),
            settings.as_ref().into(),
            &state,
            preview,
            connectivity,
            concurrency,
            native_tls,
            cache,
            printer,
        )
        .await?
    };

    let site_packages = SitePackages::from_environment(&environment)?;
    let installed = site_packages.get_packages(&from.name);
    let Some(installed_dist) = installed.first().copied() else {
        bail!("Expected at least one requirement")
    };

    // Find a suitable path to install into
    // TODO(zanieb): Warn if this directory is not on the PATH
    let executable_directory = find_executable_directory()?;
    fs_err::create_dir_all(&executable_directory)
        .context("Failed to create executable directory")?;

    debug!(
        "Installing tool entry points into {}",
        executable_directory.user_display()
    );

    let entry_points = entrypoint_paths(
        &environment,
        installed_dist.name(),
        installed_dist.version(),
    )?;

    // Determine the entry points targets
    // Use a sorted collection for deterministic output
    let target_entry_points = entry_points
        .into_iter()
        .map(|(name, source_path)| {
            let target_path = executable_directory.join(
                source_path
                    .file_name()
                    .map(std::borrow::ToOwned::to_owned)
                    .unwrap_or_else(|| OsString::from(name.clone())),
            );
            (name, source_path, target_path)
        })
        .collect::<BTreeSet<_>>();

    if target_entry_points.is_empty() {
        // Clean up the environment we just created
        installed_tools.remove_environment(&from.name)?;

        bail!("No entry points found for tool `{}`", from.name);
    }

    // Check if they exist, before installing
    let mut existing_entry_points = target_entry_points
        .iter()
        .filter(|(_, _, target_path)| target_path.exists())
        .peekable();

    // Note we use `reinstall_entry_points` here instead of `reinstall`; requesting reinstall
    // will _not_ remove existing entry points when they are not managed by uv.
    if force || reinstall_entry_points {
        for (name, _, target) in existing_entry_points {
            debug!("Removing existing entry point `{name}`");
            fs_err::remove_file(target)?;
        }
    } else if existing_entry_points.peek().is_some() {
        // Clean up the environment we just created
        installed_tools.remove_environment(&from.name)?;

        let existing_entry_points = existing_entry_points
            // SAFETY: We know the target has a filename because we just constructed it above
            .map(|(_, _, target)| target.file_name().unwrap().to_string_lossy())
            .collect::<Vec<_>>();
        let (s, exists) = if existing_entry_points.len() == 1 {
            ("", "exists")
        } else {
            ("s", "exist")
        };
        bail!(
            "Entry point{s} for tool already {exists}: {} (use `--force` to overwrite)",
            existing_entry_points.iter().join(", ")
        )
    }

    for (name, source_path, target_path) in &target_entry_points {
        debug!("Installing `{name}`");
        #[cfg(unix)]
        replace_symlink(source_path, target_path).context("Failed to install entrypoint")?;
        #[cfg(windows)]
        fs_err::copy(source_path, target_path).context("Failed to install entrypoint")?;
    }

    writeln!(
        printer.stderr(),
        "Installed: {}",
        target_entry_points
            .iter()
            .map(|(name, _, _)| name)
            .join(", ")
    )?;

    debug!("Adding receipt for tool `{}`", from.name);
    let installed_tools = installed_tools.init()?;
    let tool = Tool::new(
        requirements
            .into_iter()
            .map(pep508_rs::Requirement::from)
            .collect(),
        python,
        target_entry_points
            .into_iter()
            .map(|(name, _, target_path)| ToolEntrypoint::new(name, target_path)),
    );
    installed_tools.add_tool_receipt(&from.name, tool)?;

    Ok(ExitStatus::Success)
}
