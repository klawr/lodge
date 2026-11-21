use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ldg",
    version,
    about = "Inject your environment into any container",
    long_about = "Inject your environment into any container.\n\n\
        lodge copies your personal tools (editor, shell config, …) together with \
        their shared-library dependencies into a running container and drops \
        you into a shell. It also drives devcontainers: `ldg up` reads \
        devcontainer.json, starts the container, installs features, runs lifecycle \
        hooks, injects your environment on top, and forwards container ports to the host.\n\n\
        New here? `ldg init` writes a starting config, `ldg doctor` checks the host.",
    after_help = "Docs: https://github.com/klawr/lodge#readme"
)]
pub struct Cli {
    /// SSH remote to operate on; the docker CLI is run on the remote host over
    /// SSH (`ssh <dest> docker …`), so no local docker CLI is needed — only an
    /// ssh client. Accepts a Host alias from ~/.ssh/config, `user@host`,
    /// `user@host:port` (for a non-standard SSH port), `[v6addr]:port`, and any
    /// of those with an `ssh://` prefix.
    #[arg(long, short, global = true, value_name = "SSH_DEST")]
    pub remote: Option<String>,

    /// Use this config file instead of ~/.config/lodge/config.toml. A relative
    /// `bundle =` path in it is resolved against the file's folder, so a folder
    /// holding a bundle and its config works wherever it is put.
    #[arg(long, short, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Show what lodge is doing: print every docker/ssh command it runs and
    /// pass -v to lodge's own ssh connections (config probe, port tunnels).
    /// Repeat for more ssh verbosity (-vv, -vvv).
    #[arg(long, short, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

/// A project folder argument.
#[derive(Args)]
pub struct ProjectArg {
    /// Project path containing the devcontainer config (default: current directory)
    pub path: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum BundleCommand {
    /// Show what a bundle contains
    Inspect {
        /// The bundle file
        file: PathBuf,
    },
    /// Check that a bundle is intact (for example after copying it)
    Verify {
        /// The bundle file
        file: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum CacheCommand {
    /// List the cached bundles (the default)
    #[command(alias = "ls")]
    List,
    /// Remove every cached bundle
    Clear,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start a devcontainer and inject your environment
    #[command(
        long_about = "Read the project's devcontainer config — looked up under PATH (default: \
            current directory) as .devcontainer/devcontainer.json, .devcontainer.json, or \
            .devcontainer/<folder>/devcontainer.json — bring the container up, run lifecycle \
            hooks and features, inject your environment, then open an interactive shell.\n\n\
            Supports dockerComposeFile, image, build, and dockerFile backends. \
            Forwards ports declared in forwardPorts and any port that starts \
            listening at runtime."
    )]
    Up {
        #[command(flatten)]
        project: ProjectArg,
        /// Force-rebuild images and recreate the container from scratch
        #[arg(long)]
        build: bool,
        /// Recreate the container without asking when its config changed
        #[arg(long, short, conflicts_with = "no_recreate")]
        yes: bool,
        /// Keep the container without asking when its config changed
        #[arg(long)]
        no_recreate: bool,
        /// Set everything up, but don't open a shell (for scripts and CI)
        #[arg(long)]
        no_attach: bool,
        /// Inject from this bundle file instead of packing your config
        #[arg(long, value_name = "FILE")]
        bundle: Option<PathBuf>,
    },
    /// Stop a project's devcontainer
    #[command(
        long_about = "Stop the container that `ldg up` started for the project. The \
            port-forwarding daemon notices the container stopping and exits on its own. \
            The container is kept (with everything installed in it) and can be resumed \
            with `ldg up`."
    )]
    Down {
        #[command(flatten)]
        project: ProjectArg,
    },
    /// Show a project's container and whether its config changed
    Status {
        #[command(flatten)]
        project: ProjectArg,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Remove a project's container
    #[command(
        long_about = "Remove the project's container, discarding everything installed in it. \
            The next `ldg up` creates a fresh one. To rebuild and recreate in one step, \
            use `ldg up --build`."
    )]
    Rm {
        #[command(flatten)]
        project: ProjectArg,
    },
    /// List the containers lodge created, across projects
    #[command(alias = "list")]
    Ls {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Inject your environment into an already-running container
    #[command(
        long_about = "Inject your environment into a container that is already running, \
            then open an interactive shell. The container is identified by name or ID \
            as shown by `docker ps`."
    )]
    Exec {
        /// Container name or ID
        container: String,
        /// Open the shell as this user (default: the container's user)
        #[arg(long, short)]
        user: Option<String>,
        /// Open the shell in this directory (default: /)
        #[arg(long, short, value_name = "DIR", default_value = "/")]
        workdir: String,
        /// Inject, but don't open a shell (for scripts and CI)
        #[arg(long)]
        no_attach: bool,
        /// Inject from this bundle file instead of packing your config
        #[arg(long, value_name = "FILE")]
        bundle: Option<PathBuf>,
    },
    /// Show the ports forwarded for a project's container
    Ports {
        #[command(flatten)]
        project: ProjectArg,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Show the port-forwarding log for a project's container
    Logs {
        #[command(flatten)]
        project: ProjectArg,
        /// Keep printing new lines as they are written
        #[arg(long, short)]
        follow: bool,
    },
    /// Remove what lodge injected from a container
    #[command(
        long_about = "Remove what lodge injected: binaries, libraries, the wrappers it \
            installed, its shell setup and any placed secrets. Injected configs are left \
            in place (you may have changed them) and listed. The devcontainer's own state \
            (hook markers, recorded config) is kept. Acts on the project's container, or \
            on --container."
    )]
    Clean {
        #[command(flatten)]
        project: ProjectArg,
        /// A running container (name or ID) instead of the project's
        #[arg(long, value_name = "NAME", conflicts_with = "path")]
        container: Option<String>,
        /// Clean what was injected for this user (default: the container's user)
        #[arg(long, short, requires = "container")]
        user: Option<String>,
    },
    /// Pack your tools and configs into a portable bundle
    #[command(
        long_about = "Build a bundle from the [[inject]] entries and [git] settings \
            in your config: your binaries relinked to a bundled glibc, their libraries, \
            and your configs, in one tar. This is the only step that needs ldd and \
            patchelf, and it must run on a Linux machine of the architecture the bundle \
            is for. Credentials directories (~/.ssh, ~/.config/gh, …) trigger a warning: \
            a bundle is meant to be shared."
    )]
    Pack {
        /// Where to write the bundle (default: `lodge-<arch>.tar` here)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Work with bundle files
    Bundle {
        #[command(subcommand)]
        command: BundleCommand,
    },
    /// Show or clear the cache of prepared bundles
    Cache {
        #[command(subcommand)]
        command: Option<CacheCommand>,
    },
    /// Write a starting config, with the tools found on this machine filled in
    #[command(
        long_about = "Write a commented config.toml (at ~/.config/lodge/config.toml, or the \
            file given with -c). Editors and tools found on PATH (nvim, hx, rg, fzf, …) \
            are added as [[inject]] entries, commented out, so you pick what to bring. \
            Refuses to overwrite an existing file unless --force."
    )]
    Init {
        /// Overwrite an existing config
        #[arg(long)]
        force: bool,
        /// Print the config instead of writing it
        #[arg(long)]
        print: bool,
    },
    /// Check that everything lodge needs is available
    #[command(
        long_about = "Check the host: that the container engine and compose are reachable, \
            which helper tools are installed and what each is needed for, and that your \
            config and the binaries it lists are usable. Looks at the devcontainer in PATH \
            (default: current directory) to tell whether `oras` is needed. With --remote, \
            checks the remote engine. Exits non-zero if something required is missing."
    )]
    Doctor {
        #[command(flatten)]
        project: ProjectArg,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Print a shell completion script
    #[command(long_about = "Print a completion script for SHELL. For example:\n\n  \
            ldg completions bash > ~/.local/share/bash-completion/completions/ldg\n  \
            ldg completions zsh  > ~/.zfunc/_ldg\n  \
            ldg completions fish > ~/.config/fish/completions/ldg.fish")]
    Completions { shell: clap_complete::Shell },
    /// Print the man page (view it with `ldg man | man -l -`)
    Man,
    /// Internal: port-forwarding daemon spawned by `ldg up`
    #[command(name = "_forward", hide = true)]
    Forward {
        container: String,
        /// Forward spec as serialized by PortForward::encode
        /// (local:port or local:host:port)
        #[arg(long)]
        port: Vec<String>,
    },
}
