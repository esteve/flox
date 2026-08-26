use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use bpaf::Bpaf;
use flox_config::Config;
use flox_events::LifecycleFields;
use flox_manifest::lockfile::Lockfile;
use flox_rust_sdk::flox::Flox;
use flox_rust_sdk::models::environment::{ConcreteEnvironment, Environment};
use flox_rust_sdk::providers::build::{
    COMMON_NIXPKGS_URL,
    FloxBuildMk,
    ManifestBuilder,
    PackageTarget,
    PackageTargetKind,
    nix_expression_dir,
};
use flox_rust_sdk::providers::catalog_lock::BuildCatalogLock;
use flox_rust_sdk::providers::nix;
use indoc::formatdoc;
use nef_lock_catalog::NixFlakeref;
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::debug;

use super::build::{
    BaseCatalogUrlSelect,
    base_catalog_url_select,
    base_nixpkgs_url_from_url_select,
    check_git_tracking_for_expression_builds,
    packages_to_build,
    prefetch_expression_build_flake_ref,
    prefetch_flake_ref,
};
use super::{DirEnvironmentSelect, activate, dir_environment_select, needs_project_files_error};
use crate::subcommand_metric;
use crate::utils::detect_shell::INTERACTIVE_BASH_BIN;
use crate::utils::message;

/// The known divergences between this shell and the shell `flox build`
/// actually runs a package's build in. Printed in full on entry — see
/// [`Develop::print_disclosure`] — because the ones most likely to burn a
/// user are exactly the ones nobody opens a manpage to discover.
const DISCLOSURE: &str = "\
This shell approximates the build environment for '{name}'.
It is not the build. Known differences:
  - No build sandbox is applied here. 'flox build' runs the build under
    'nix build', which the Nix daemon may sandbox.
  - Your working tree is visible here, including files git does not
    track. A real build sees only tracked files.
  - '$src' is a snapshot in the Nix store, taken when you entered.
    'genericBuild' builds that snapshot, not your working tree; edits
    reach it only when you re-enter. See 'man flox-develop'.
  - '$out' and the other output variables point at placeholder paths
    under /tmp/outputs, not at store paths. Nothing installed there is
    a real build output.
  - The host PATH stays reachable after the build inputs, and if your
    '~/.bashrc' activates a Flox environment, that environment is on
    PATH here too. A real build sees only its own inputs.
  - This shell is interactive and sources '~/.bashrc'. The build shell
    does neither.";

#[derive(Bpaf, Clone)]
pub enum Develop {
    Package(#[bpaf(external(develop_options))] DevelopOptions),
    /// Deprecated: 'flox develop' as a synonym for 'flox activate'.
    #[bpaf(hide)]
    DeprecatedActivate(#[bpaf(external(activate::activate))] activate::Activate),
}

#[derive(Bpaf, Clone)]
pub struct DevelopOptions {
    #[bpaf(external(dir_environment_select), fallback(Default::default()))]
    environment: DirEnvironmentSelect,

    #[bpaf(external(base_catalog_url_select), optional)]
    base_catalog_url_select: Option<BaseCatalogUrlSelect>,

    /// The Nix expression package to develop.
    /// Corresponds to an expression file in '.flox/pkgs/'.
    #[bpaf(positional("package"), non_strict)]
    package: String,
}

impl Develop {
    /// Centrally-derived subcommand string for this invocation. The package
    /// form returns its own name; the deprecated form delegates to
    /// [`activate::Activate::subcommand_name`] so the legacy `activate`,
    /// `activate::allow` and `activate::deny` wire names survive unchanged —
    /// there is no `flox develop deprecated` behind a `develop::deprecated`
    /// key for a dashboard to find.
    pub fn subcommand_name(&self) -> &'static str {
        match self {
            Develop::Package(_) => "develop",
            Develop::DeprecatedActivate(activate) => activate.subcommand_name(),
        }
    }

    pub async fn handle(self, config: Config, flox: Flox) -> Result<()> {
        match self {
            Develop::Package(opts) => {
                subcommand_metric!("develop", "deprecated_alias" = false);
                Self::develop(flox, opts).await
            },
            Develop::DeprecatedActivate(activate) => {
                subcommand_metric!("develop", "deprecated_alias" = true);
                message::warning(formatdoc! {"
                    'flox develop' without a package is deprecated; it currently behaves as 'flox activate'.
                    Use 'flox develop <PACKAGE>' to enter a development shell for a Nix expression build.
                "});
                activate.handle(config, flox).await
            },
        }
    }

    async fn develop(mut flox: Flox, opts: DevelopOptions) -> Result<()> {
        let DevelopOptions {
            environment,
            base_catalog_url_select,
            package,
        } = opts;

        let mut env = environment.detect_concrete_environment(&mut flox, "Develop packages of")?;
        match &env {
            ConcreteEnvironment::Path(_) => (),
            ConcreteEnvironment::Managed(managed) => {
                bail!(needs_project_files_error(managed, "develop"))
            },
            ConcreteEnvironment::Remote(_) => {
                // guarded by DirEnvironmentSelect
                unreachable!("Cannot develop from a remote environment")
            },
        };

        let base_dir = env.parent_path()?;
        let cache_path = env.cache_path()?;
        let built_environments = env.build(&flox)?;
        let lockfile: Lockfile = env.lockfile(&flox)?.into();
        let lockfile_manifest = lockfile.migrated_manifest()?;

        let expression_parent_dir = env.dot_flox_path();
        let expression_path_ref = NixFlakeref::from_path(&expression_parent_dir)?;
        let mut targets = packages_to_build(&lockfile_manifest, &expression_path_ref, &[package])?;
        let target = targets.remove(0);

        Self::refuse_manifest_build(&target)?;

        let expression_git_ref =
            check_git_tracking_for_expression_builds([&target], &expression_parent_dir)?;
        let expression_ref = expression_git_ref.unwrap_or(expression_path_ref);

        // The catalog lock the NEF eval consumes, created by the CLI
        // exactly as `flox build` does: the committed .flox/catalog.lock as
        // found, or a fresh ephemeral lock scoped to this package (the
        // scanner follows its imports) living only as long as this command.
        let rel_file_path = match target.kind() {
            PackageTargetKind::ExpressionBuild(expression) => expression.rel_file_path.clone(),
            // Guarded by `refuse_manifest_build` above.
            PackageTargetKind::ManifestBuild { .. } => {
                unreachable!("manifest builds are refused before the eval")
            },
        };
        let catalog_lock = BuildCatalogLock::ensure(
            &flox.floxhub_client,
            env.dot_flox_path(),
            nix_expression_dir(&env),
            [&rel_file_path],
            &flox.temp_dir,
        )
        .await?;

        let base_nixpkgs_url =
            base_nixpkgs_url_from_url_select(&flox, base_catalog_url_select, Some(&lockfile))
                .await?
                .as_flake_ref()?;

        prefetch_flake_ref(&COMMON_NIXPKGS_URL)?;
        prefetch_expression_build_flake_ref([&target], &base_nixpkgs_url)?;

        let eval_results = FloxBuildMk::new(
            &flox,
            &base_dir,
            &expression_ref,
            &built_environments,
            &cache_path,
        )
        .eval(
            &base_nixpkgs_url,
            &[target.name()],
            catalog_lock.path(),
            None,
        )?;
        let drv_path = eval_results
            .first()
            .expect("eval() returns exactly one result for one requested package")
            .drv_path
            .clone();

        let env_script_path = Self::print_dev_env(&flox, &drv_path, target.name().as_ref())?;
        let rcfile_path = Self::render_rcfile(&flox, &env_script_path, target.name().as_ref())?;

        Self::print_disclosure(target.name().as_ref());

        // `exec` replaces this process, so the dispatcher's end-of-run
        // `command_completed` emit (main.rs) never runs; record it here
        // first, mirroring the in-place handoff `activate` performs before
        // its own `exec` (activate.rs:741-771).
        let hub = flox_events::EventsHub::global();
        if let Err(err) = hub.record_command_completed("develop".to_string(), LifecycleFields {
            exit_code: 0,
            duration_ms: None,
            error_kind: None,
        }) {
            debug!(error = %err, "Failed to record v2 cli.command_completed event before exec");
        }
        if let Err(err) = hub.flush(flox_events::force_flush_requested()) {
            debug!(error = %err, "Failed to flush v2 events before exec");
        }

        let mut command = Command::new(&*INTERACTIVE_BASH_BIN);
        command.arg("--rcfile").arg(&rcfile_path);
        debug!(command = ?command, "exec'ing development shell");

        // exec should never return
        Err(command.exec()).context("failed to exec development shell")
    }

    /// An unsandboxed manifest build already runs against the activated
    /// environment, which is most of what this command would give it, so
    /// naming one is refused with guidance rather than served by a second
    /// code path.
    fn refuse_manifest_build(target: &PackageTarget) -> Result<()> {
        if !target.kind().is_manifest_build() {
            return Ok(());
        }

        let name = target.name();
        bail!(formatdoc! {r#"
            Cannot develop '{name}': it is a manifest build, not a Nix expression build.
            An unsandboxed manifest build already runs against the activated environment,
            so the shell it would get is one you can enter today.
            If '{name}' declares any 'sandbox' mode other than "off", set 'sandbox = "off"' first.

            Next:
              $ flox activate                      <- Enter the environment
              $ <steps from build.{name}.command>  <- Run the build by hand
            "#, name = name});
    }

    /// Realise `drv_path`'s inputs and capture `nix print-dev-env`'s output
    /// to a file under `flox.temp_dir`, wrapping a raw nix failure per this
    /// repo's rule against surfacing internal tool output (`AGENTS.md`).
    ///
    /// `nix print-dev-env` realises the derivation's *inputs* and a wrapper
    /// derivation that dumps the environment; it does not run the package's
    /// build phases, which is what lets a package that fails to build still
    /// yield a shell.
    fn print_dev_env(flox: &Flox, drv_path: &Path, pname: &str) -> Result<PathBuf, DevelopError> {
        let mut cmd = nix::nix_base_command();
        cmd.arg("print-dev-env").arg(drv_path);

        let output = cmd.output().map_err(DevelopError::CallPrintDevEnv)?;

        if !output.status.success() {
            // The `drvPath` is not GC-rooted between the eval above and this
            // call, and this window is longer than the one the makefile's
            // own `build` goal guards internally. A pre-flight existence
            // check cannot close that race and only adds a stat to the
            // happy path, so the classification happens here, on the
            // failure path only.
            if !drv_path.exists() {
                return Err(DevelopError::DerivationGarbageCollected {
                    pname: pname.to_string(),
                });
            }
            return Err(DevelopError::PrintDevEnv {
                pname: pname.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let env_script_path = NamedTempFile::new_in(&flox.temp_dir)
            .map_err(DevelopError::CreateEnvScriptFile)?
            .into_temp_path();
        // SAFETY: according to the docs, this is fallible on _Windows_
        let env_script_path = env_script_path
            .keep()
            .expect("failed to keep env script file");
        std::fs::write(&env_script_path, &output.stdout)
            .map_err(DevelopError::CreateEnvScriptFile)?;

        Ok(env_script_path)
    }

    /// Render the rcfile a develop shell is exec'd with: `~/.bashrc` first
    /// (guarded by `_flox_sourcing_rc`), then the `print-dev-env` output,
    /// then the Flox prompt.
    ///
    /// The order is load-bearing: the env script overwrites `PATH` with the
    /// build inputs and then appends whatever `PATH` was live when it was
    /// sourced, so `~/.bashrc` must run first or the user's own `PATH`
    /// entries land in front of the build inputs and shadow the build's
    /// toolchain.
    fn render_rcfile(
        flox: &Flox,
        env_script_path: &Path,
        pname: &str,
    ) -> Result<PathBuf, DevelopError> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let bashrc_block = match &home {
            Some(home) => formatdoc! {r#"
                # 1. User config first — REQUIRED to be first. The env script
                #    sourced below overwrites PATH with the build inputs and then
                #    appends whatever PATH was live when it was sourced. Run
                #    ~/.bashrc afterwards instead and the user's own PATH entries
                #    land in front of the build inputs, shadowing the build's
                #    toolchain.
                #
                #    _flox_sourcing_rc is the guard flox's own activation rcfile
                #    sets (flox-activations/src/gen_rc/bash.rs), read back at
                #    attach.rs. It stops a subshell 'flox activate' inside
                #    ~/.bashrc from re-sourcing ~/.bashrc from inside this very
                #    sourcing. It does NOT suppress the activation itself — see
                #    the disclosure printed before this shell's prompt.
                if [ -n "${{PS1:-}}" ] && [ -f "{home}/.bashrc" ]; then
                  export _flox_sourcing_rc=true
                  source "{home}/.bashrc"
                  unset _flox_sourcing_rc
                fi
                "#, home = home.display()},
            None => String::new(),
        };

        let rcfile_content = formatdoc! {r#"
            {bashrc_block}
            # 2. The `nix print-dev-env` output: build inputs, stdenv
            #    functions, NIX_BUILD_TOP/TMPDIR fixups, shellHook eval. It
            #    sets nix_saved_PATH/nix_saved_XDG_DATA_DIRS itself, from the
            #    PATH live at this point — this rcfile must not pre-set or
            #    clobber them.
            source "{env_script_path}"

            # 3. Flox prompt: wrap the existing PS1, never replace it. This
            #    duplicates the wrap-not-replace logic in
            #    assets/environment-interpreter/activate/activate.d/set-prompt.bash
            #    rather than sourcing it: that asset depends on activation-time
            #    state (FLOX_PROMPT_ENVIRONMENTS, _activate_d) a develop shell
            #    never sets.
            if [ -n "${{PS1:-}}" ]; then
              if [ -z "${{FLOX_SAVE_BASH_PS1:-}}" ]; then
                export FLOX_SAVE_BASH_PS1="$PS1"
              fi
              if [ "${{NO_COLOR:-0}}" = "0" ]; then
                __flox_develop_marker="\[\e[1m\]flox [develop: {pname}]\[\e[0m\] "
              else
                __flox_develop_marker="flox [develop: {pname}] "
              fi
              case "$FLOX_SAVE_BASH_PS1" in
                *\\n*) PS1="${{FLOX_SAVE_BASH_PS1/\\n/\\n$__flox_develop_marker}}" ;;
                *\\012*) PS1="${{FLOX_SAVE_BASH_PS1/\\012/\\012$__flox_develop_marker}}" ;;
                *) PS1="$__flox_develop_marker$FLOX_SAVE_BASH_PS1" ;;
              esac
              unset __flox_develop_marker
            fi
            "#,
            bashrc_block = bashrc_block,
            env_script_path = env_script_path.display(),
            pname = pname,
        };

        let rcfile_path = NamedTempFile::new_in(&flox.temp_dir)
            .map_err(DevelopError::CreateRcFile)?
            .into_temp_path();
        // SAFETY: according to the docs, this is fallible on _Windows_
        let rcfile_path = rcfile_path.keep().expect("failed to keep rcfile");
        std::fs::write(&rcfile_path, rcfile_content).map_err(DevelopError::CreateRcFile)?;

        Ok(rcfile_path)
    }

    /// Print the fixed six-item disclosure list as a single `message::info`
    /// block, honoring the CLI's one-emoji-per-response rule.
    fn print_disclosure(pname: &str) {
        message::info(DISCLOSURE.replace("{name}", pname));
    }
}

#[derive(Debug, Error)]
pub(crate) enum DevelopError {
    #[error("Failed to call 'nix print-dev-env'")]
    CallPrintDevEnv(#[source] std::io::Error),

    #[error("failed to write the development shell's environment script")]
    CreateEnvScriptFile(#[source] std::io::Error),

    #[error("failed to write the development shell's rcfile")]
    CreateRcFile(#[source] std::io::Error),

    #[error("Failed to build the development environment for '{pname}'\n{stderr}")]
    PrintDevEnv { pname: String, stderr: String },

    #[error(
        "The derivation for '{pname}' was garbage collected between evaluation and use.\nPlease try again."
    )]
    DerivationGarbageCollected { pname: String },
}

#[cfg(test)]
mod tests {
    use flox_rust_sdk::flox::test_helpers::flox_instance;
    use flox_rust_sdk::providers::build::{ExpressionBuildMetadata, PackageTargetKind};

    use super::*;

    #[test]
    fn refuse_manifest_build_allows_expression_builds() {
        let target = PackageTarget::new_unchecked(
            "greet",
            PackageTargetKind::ExpressionBuild(ExpressionBuildMetadata {
                rel_file_path: Default::default(),
            }),
        );
        assert!(Develop::refuse_manifest_build(&target).is_ok());
    }

    #[test]
    fn refuse_manifest_build_names_activate_and_sandbox() {
        let target = PackageTarget::new_unchecked("greet", PackageTargetKind::ManifestBuild {
            sandbox: None,
        });
        let message = Develop::refuse_manifest_build(&target)
            .unwrap_err()
            .to_string();
        assert!(message.contains("flox activate"));
        assert!(message.contains("sandbox"));
        assert!(message.contains("greet"));
    }

    /// The `drvPath` a develop shell is built from is not GC-rooted between
    /// the eval that produced it and the `nix print-dev-env` call that
    /// consumes it. If the derivation is collected in that window, the
    /// failure is classified as `DerivationGarbageCollected` rather than
    /// surfacing nix's own "does not exist" error, per the flox repo's rule
    /// against surfacing internal tool output.
    #[test]
    fn print_dev_env_reports_a_collected_derivation() {
        let (flox, _temp_dir) = flox_instance();
        let missing_drv_path = flox.temp_dir.join("does-not-exist.drv");

        let err = Develop::print_dev_env(&flox, &missing_drv_path, "greet").unwrap_err();

        assert!(matches!(
            err,
            DevelopError::DerivationGarbageCollected { pname } if pname == "greet"
        ));
    }
}
