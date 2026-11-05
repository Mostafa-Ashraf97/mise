//! Expand runtime requirements before resolution, using ordinary tool requests.
use eyre::{Result, bail};

use crate::args::ToolArg;
use crate::install_context::backend_args_match;
use crate::toolset::{ToolRequest, ToolRequestSetBuilder, ToolVersionOptions, Toolset};

pub(crate) fn declarations(options: &ToolVersionOptions) -> Result<Vec<ToolArg>> {
    let Some(value) = options.opts.get("runtime_dependencies") else {
        return Ok(vec![]);
    };
    let values = super::tool_version_options::parse_string_or_array(value, "runtime_dependencies")
        .map_err(|message| eyre::eyre!(message))?;
    values
        .into_iter()
        .map(|value| {
            let arg: ToolArg = value.parse()?;
            if arg.tvr.is_none() {
                bail!("runtime dependency '{value}' must specify a version");
            }
            Ok(arg)
        })
        .collect()
}

/// Explicit selections override recipe defaults; differing defaults are an error.
/// Dependencies inherit their consumer's lockfile ownership, not the caller's cwd.
pub(crate) fn expand(
    roots: &[ToolRequest],
    configured: &[ToolRequest],
) -> Result<Vec<ToolRequest>> {
    expand_on(roots, configured, None)
}

pub fn expand_on(
    roots: &[ToolRequest],
    configured: &[ToolRequest],
    platform: Option<&crate::platform::Platform>,
) -> Result<Vec<ToolRequest>> {
    if !roots
        .iter()
        .any(|request| request.options().opts.contains_key("runtime_dependencies"))
    {
        return Ok(roots.to_vec());
    }
    let mut selected = configured
        .iter()
        .filter(|candidate| {
            !roots
                .iter()
                .any(|root| backend_args_match(root.ba(), candidate.ba()))
        })
        .cloned()
        .collect::<Vec<_>>();
    selected.extend_from_slice(roots);
    let mut result = roots.to_vec();
    let mut visited = Vec::new();
    let mut path = Vec::new();
    let mut defaults = Vec::new();
    for root in roots {
        if supported(root, platform) && !disabled(root, platform) {
            visit(
                root,
                &selected,
                &mut result,
                &mut visited,
                &mut path,
                &mut defaults,
                platform,
            )?;
        }
    }
    Ok(result)
}

fn supported(request: &ToolRequest, platform: Option<&crate::platform::Platform>) -> bool {
    platform.map_or_else(
        || request.is_os_supported(),
        |platform| request.is_os_supported_on(platform),
    )
}

fn disabled(request: &ToolRequest, platform: Option<&crate::platform::Platform>) -> bool {
    let filter = ToolRequestSetBuilder::new();
    if platform.is_some() {
        filter.is_disabled_for_lock(request.ba())
    } else {
        filter.is_disabled(request.ba())
    }
}

fn visit(
    request: &ToolRequest,
    configured: &[ToolRequest],
    result: &mut Vec<ToolRequest>,
    visited: &mut Vec<ToolRequest>,
    path: &mut Vec<ToolRequest>,
    defaults: &mut Vec<ToolRequest>,
    platform: Option<&crate::platform::Platform>,
) -> Result<()> {
    if path
        .iter()
        .any(|parent| backend_args_match(parent.ba(), request.ba()))
    {
        bail!(
            "runtime dependency cycle: {} -> {request}",
            path.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" -> ")
        );
    }
    if visited.contains(request) {
        return Ok(());
    }
    path.push(request.clone());
    for declaration in declarations(&request.options())? {
        let mut matches = configured
            .iter()
            .filter(|candidate| backend_args_match(candidate.ba(), &declaration.ba))
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            matches.retain(|candidate| supported(candidate, platform));
            if matches.is_empty() {
                bail!(
                    "runtime dependency {} required by {request} is disabled or unsupported",
                    declaration.ba
                );
            }
        }
        // Equal selectors in different scopes can resolve to different lockfile
        // pins. Only collapse identical requests, including their ownership.
        matches.dedup();
        let dependency = match matches.as_slice() {
            [selected] => (*selected).clone(),
            [] => {
                let mut dependency = declaration.tvr.expect("validated version");
                dependency.set_source(request.source().clone());
                dependency.set_lockfile_scope(request.lockfile_scope().clone());
                if defaults.iter().any(|existing| {
                    backend_args_match(existing.ba(), dependency.ba())
                        && (existing.version() != dependency.version()
                            || existing.options() != dependency.options())
                }) {
                    bail!(
                        "conflicting runtime dependency defaults for {}; configure an explicit version",
                        dependency.ba()
                    );
                }
                defaults.push(dependency.clone());
                dependency
            }
            _ => bail!(
                "ambiguous runtime dependency {}; configure one active version",
                declaration.ba
            ),
        };
        if !supported(&dependency, platform)
            || disabled(&dependency, platform)
            || dependency.ignored_in_safe_mode()
        {
            bail!(
                "runtime dependency {dependency} required by {request} is disabled or unsupported"
            );
        }
        visit(
            &dependency,
            configured,
            result,
            visited,
            path,
            defaults,
            platform,
        )?;
        if !result.contains(&dependency) {
            result.push(dependency);
        }
    }
    path.pop();
    visited.push(request.clone());
    Ok(())
}

impl Toolset {
    pub(crate) fn add_runtime_dependencies(&mut self) -> Result<()> {
        let requests = self
            .versions
            .values()
            .flat_map(|v| v.requests.clone())
            .collect::<Vec<_>>();
        for request in expand(&requests, &requests)? {
            if !requests.contains(&request) {
                let versions = self
                    .versions
                    .entry(request.ba().clone())
                    .or_insert_with(|| {
                        super::ToolVersionList::new(request.ba().clone(), request.source().clone())
                    });
                versions.requests.push(request);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_locks_expand_tools_unsupported_on_the_host() {
        let platform = crate::platform::Platform::parse("windows-x64").unwrap();
        for input in [
            "gsudo[runtime_dependencies='tiny@1.0.0']@2.6.1",
            "tiny[runtime_dependencies='gsudo@2.6.1']@1.0.0",
        ] {
            let root = input.parse::<ToolArg>().unwrap().tvr.unwrap();
            let expanded = expand_on(&[root], &[], Some(&platform)).unwrap();
            assert_eq!(expanded.len(), 2);
            assert!(expanded.iter().any(|request| request.ba().short == "gsudo"));
            assert!(expanded.iter().any(|request| request.ba().short == "tiny"));
        }
    }
}
