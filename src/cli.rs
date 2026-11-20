use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ldg",
    version,
    about = "Inject your environment into any container",
    long_about = "Inject your environment into any container.\n\n\
        lodge copies your personal tools (editor, shell config, …) together with \
        their shared-library dependencies into a running Docker container and drops \
        you into a shell. It also drives devcontainers: `ldg up` reads \
        devcontainer.json, starts the container, installs features, runs lifecycle \
        hooks, injects your environment on top, and forwards container ports to the host."
)]
pub struct Cli {
    /// SSH remote to operate on; the docker CLI is run on the remote host over
    /// SSH (`ssh <dest> docker …`), so no local docker CLI is needed — only an
    /// ssh client. Accepts any form the SSH client understands: a bare Host
    /// alias from ~/.ssh/config, user@host, or user@host:port (for a
    /// non-standard SSH port).
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
pub enum Command {
    /// Start a devcontainer and inject your environment
    #[command(
        long_about = "Read the project's devcontainer config — looked up under PATH (default: \
            current directory) as .devcontainer/devcontainer.json, .devcontainer.json, or \
            .devcontainer/<folder>/devcontainer.json — bring the container up, run lifecycle \
            hooks and features, inject your environment, then open an interactive shell.\n\n\
            Supports dockerComposeFile, image, build, and dockerFile backends. \
            Forwards ports declared in forwardPorts and any port that starts \
            listening at runtime. Config: ~/.config/lodge/config.toml."
    )]
    Up {
        /// Project path containing the devcontainer config
        /// (defaults to current directory)
        path: Option<PathBuf>,
        /// Force-rebuild images and recreate the container from scratch
        #[arg(long)]
        build: bool,
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
        /// Project path containing the devcontainer config
        /// (defaults to current directory)
        path: Option<PathBuf>,
    },
    /// Show a project's container and whether its config changed
    Status {
        /// Project path containing the devcontainer config
        /// (defaults to current directory)
        path: Option<PathBuf>,
    },
    /// Remove a project's container
    #[command(
        long_about = "Remove the project's container, discarding everything installed in it. \
            The next `ldg up` creates a fresh one. To rebuild and recreate in one step, \
            use `ldg up --build`."
    )]
    Rm {
        /// Project path containing the devcontainer config
        /// (defaults to current directory)
        path: Option<PathBuf>,
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
        /// Inject, but don't open a shell (for scripts and CI)
        #[arg(long)]
        no_attach: bool,
        /// Inject from this bundle file instead of packing your config
        #[arg(long, value_name = "FILE")]
        bundle: Option<PathBuf>,
    },
    /// Pack your tools and configs into a portable bundle
    #[command(
        long_about = "Build a bundle from the [[inject]] entries, [aliases] and [git] settings \
            in your config: your binaries relinked to a bundled glibc, their libraries, \
            and your configs, in one tar. This is the only step that needs ldd and \
            patchelf, and it must run on a Linux machine of the architecture the bundle \
            is for. Credentials directories (~/.ssh, ~/.config/gh, …) trigger a warning: \
            a bundle is meant to be shared."
    )]
    Pack {
        /// Where to write the bundle (default: lodge-<arch>.tar here)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Work with bundle files
    Bundle {
        #[command(subcommand)]
        command: BundleCommand,
    },
    /// Check that everything lodge needs is available
    #[command(
        long_about = "Check the host: that the container engine is reachable, which helper \
            tools are installed and what each is needed for, and that your config and the \
            binaries it lists are usable. With --remote, checks the remote engine. Exits \
            non-zero if a required tool is missing."
    )]
    Doctor,
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
