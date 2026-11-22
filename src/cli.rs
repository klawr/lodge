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
        Run `ldg` alone to pick up where you left off: it lists the projects and \
        containers you opened sessions in, most recent first, and takes you back \
        into the one you choose, the same way you got there. On a row, `r` opens it \
        and injects again, and `b` recreates a project's container (after asking); \
        `c` opens the config: browse what is injected, add programs and data from \
        a list, remove one.\n\n\
        New here? `ldg init` writes a starting config, `ldg doctor` checks the host.",
    after_help = "Docs: https://github.com/klawr/lodge#readme"
)]
pub struct Cli {
    /// SSH remote to operate on; the docker CLI is run on the remote host over
    /// SSH (`ssh <dest> docker …`), so no local container CLI is needed — only an
    /// ssh client. Accepts a Host alias from ~/.ssh/config, `user@host`,
    /// `user@host:port` (for a non-standard SSH port), `[v6addr]:port`, and any
    /// of those with an `ssh://` prefix.
    #[arg(long, short, global = true, value_name = "SSH_DEST")]
    pub remote: Option<String>,

    /// Where to work. It is on the host `--remote` names, or on this machine.
    /// Default: `target` in the config file, else the first one listed.
    #[arg(long, global = true, value_name = "TARGET")]
    pub target: Option<String>,

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

    /// Internal: the project folder whose `lodge.toml` the settings of a target
    /// were read from, handed to the forward daemon so it works on the same pod.
    #[arg(long, global = true, hide = true, value_name = "DIR")]
    pub project_dir: Option<PathBuf>,

    /// What to do. Without one, `ldg` offers the places you were in before.
    #[command(subcommand)]
    pub command: Option<Command>,
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
        /// The bundle: a file, or an `oci://` reference (fetched, not cached)
        #[arg(value_name = "FILE|oci://REF")]
        reference: String,
    },
    /// Check that a bundle is intact (for example after copying it)
    Verify {
        /// The bundle: a file, or an `oci://` reference (fetched, not cached)
        #[arg(value_name = "FILE|oci://REF")]
        reference: String,
    },
    /// Make a bundle the default, or choose one from those on offer
    #[command(
        long_about = "Set `bundle =` in the config, so that `ldg up` and `ldg attach` inject \
            from it. The bundle is fetched and checked first, so a reference that is wrong \
            or for another architecture is refused before the config is touched. The \
            bundle that was the default stays on offer (see `ldg bundle ls`). With \
            --live the default is the config's own [[binary]] and [[data]] entries, packed \
            on the spot (Linux only). With neither, you choose from the bundles on offer."
    )]
    Use {
        /// The bundle: a file, or an `oci://` reference
        #[arg(value_name = "FILE|oci://REF", conflicts_with = "live")]
        reference: Option<String>,
        /// Pack the config's entries instead of using a bundle
        #[arg(long)]
        live: bool,
    },
    /// Put a bundle on offer, without making it the default
    #[command(
        long_about = "Add a bundle to `bundles` in the config. When `ldg up` injects into a \
            project it has not opened before (or with --reinject) and more than one bundle is \
            on offer, it asks which. `ldg pack -o` adds what it writes."
    )]
    Add {
        /// The bundle: a file, or an `oci://` reference
        #[arg(value_name = "FILE|oci://REF")]
        reference: String,
    },
    /// Take a bundle off the ones on offer
    Rm {
        /// The bundle, as listed by `ldg bundle ls`
        #[arg(value_name = "FILE|oci://REF")]
        reference: String,
    },
    /// List the bundles on offer; the default is marked
    Ls,
}

#[derive(Subcommand)]
pub enum AddCommand {
    /// Add a program: its binary and the libraries it needs
    #[command(
        long_about = "Add a [[binary]] entry to the config file. Without a PATH, the program \
            called NAME on the PATH of this shell is used."
    )]
    Binary {
        /// Name of the entry, and of the binary in the bundle
        name: String,
        /// Host path of the binary (default: NAME, looked up on PATH)
        path: Option<PathBuf>,
        /// Also write a wrapper script that runs the binary here in the container
        #[arg(long, value_name = "PATH")]
        target: Option<String>,
        /// Do not pack the shared libraries of the binary
        #[arg(long)]
        no_deps: bool,
        /// Do not also add what sits beside the program (`../share/NAME`, `../lib/NAME`)
        #[arg(long)]
        no_data: bool,
    },
    /// Add a file or directory, copied as it is
    #[command(
        long_about = "Add a [[data]] entry to the config file: a file or directory, such as a \
            config, a plugin folder or the runtime files of a program. It goes to the same \
            place in the container as it is here (`~/…` stays under the user's home)."
    )]
    Data {
        /// Host path of the file or directory
        path: PathBuf,
        /// Where it goes in the container (default: the same place as here)
        #[arg(long, value_name = "PATH")]
        target: Option<String>,
        /// The program it is for, by its name (only says what it belongs to)
        #[arg(long = "for", value_name = "NAME")]
        binary: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RemoveCommand {
    /// Remove a program
    Binary {
        /// Name of the entry
        name: String,
    },
    /// Remove a file or directory
    Data {
        /// Host path of the file or directory
        path: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum InjectCommand {
    /// List the entries in the config (the default)
    #[command(alias = "list")]
    Ls,
    /// Add a program or data to what is injected
    Add {
        #[command(subcommand)]
        what: AddCommand,
    },
    /// Remove a program or data from what is injected
    #[command(alias = "remove")]
    Rm {
        #[command(subcommand)]
        what: RemoveCommand,
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
            hooks and features, then open an interactive shell. Your environment is injected \
            when the container is created; after that `ldg up` only attaches, and says when \
            what is injected is out of date (`--reinject` updates it, `--build` recreates \
            the container).\n\n\
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
        /// Inject again even though something is already injected
        #[arg(long)]
        reinject: bool,
        /// Set everything up, but don't open a shell (for scripts and CI)
        #[arg(long)]
        no_attach: bool,
        /// Inject from this bundle instead of packing your config
        #[arg(long, short, value_name = "FILE|oci://REF")]
        bundle: Option<String>,
    },
    /// Work in a container lodge did not make
    #[command(
        long_about = "Inject your tools into a running container that was made some other \
            way (a `docker run`, a compose service, someone else's), and open a shell in \
            it. Nothing is created, started or stopped: the container must be running, \
            and it is treated as somebody's workload, so a file that is already there \
            is left as it is (with a warning) and git's ownership check is not loosened. \
            It acts as the user the container runs as. Works on the engine of the target \
            (`--target docker|podman`, and `--remote`). `ldg clean --container NAME` takes \
            lodge out again."
    )]
    Attach {
        /// The container's name or id
        container: String,
        /// The directory a session starts in (default: the user's home)
        #[arg(long, value_name = "DIR")]
        workdir: Option<String>,
        /// Inject again even though something is already injected
        #[arg(long)]
        reinject: bool,
        /// Set everything up, but don't open a shell (for scripts and CI)
        #[arg(long)]
        no_attach: bool,
        /// Inject from this bundle instead of packing your config
        #[arg(long, short, value_name = "FILE|oci://REF")]
        bundle: Option<String>,
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
            installed, its shell setup and any placed secrets. Injected data is left \
            in place (you may have changed it) and listed. The devcontainer's own state \
            (hook markers, recorded config) is kept."
    )]
    Clean {
        #[command(flatten)]
        project: ProjectArg,
        /// Clean this container (one `ldg attach` worked in) instead of the project's
        #[arg(long, value_name = "NAME|ID", conflicts_with = "path")]
        container: Option<String>,
    },
    /// Pack your tools and their data into a portable bundle
    #[command(
        long_about = "Build a bundle from the [[binary]] and [[data]] entries and [git] settings \
            in your config: your binaries relinked to a bundled glibc, their libraries, \
            and your data, in one tar. This is the only step that needs ldd and \
            patchelf, and it must run on a Linux machine of the architecture the bundle \
            is for. Credentials directories (~/.ssh, ~/.config/gh, …) trigger a warning: \
            a bundle is meant to be shared. The output is a file, or a registry as \
            oci://ghcr.io/me/lodge:1, which publishes it there (the tag holds one image per \
            architecture: this machine's is replaced without asking, the others are kept; \
            `--no-clobber` refuses instead); `ldg up --bundle` takes the same reference. Publishing needs `docker login` for the registry."
    )]
    Pack {
        /// Where to write the bundle: a file, or an `oci://` reference to publish it
        /// to (default: `lodge-<arch>.tar` here)
        #[arg(short, long, value_name = "FILE|oci://REF")]
        output: Option<String>,
        /// Refuse to replace what is already there: a file that exists, or this
        /// machine's image under the tag (for CI that must not overwrite)
        #[arg(long)]
        no_clobber: bool,
    },
    /// Add or remove the programs and data that are injected
    #[command(
        long_about = "Edit the [[binary]] and [[data]] entries of the config file (the global \
            -c FILE, or ~/.config/lodge/config.toml) without opening it: \
            `ldg inject add binary rg`, `ldg inject add data ~/.config/nvim --for nvim`, \
            `ldg inject rm binary rg`. Comments in the file are kept. A bundle set with \
            `bundle =` replaces the entries, so changing them does nothing until you pack \
            again or drop that line."
    )]
    Inject {
        #[command(subcommand)]
        command: Option<InjectCommand>,
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
            are added as [[binary]] and [[data]] entries, commented out, so you pick what to bring. \
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
            (default: current directory) to tell whether compose is needed. With --remote, \
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
        /// How many polls in a row (2 s apart) may fail to reach the engine
        /// before the daemon exits, unless the host waits for a login
        #[arg(long, default_value_t = crate::forward::UNREACHABLE_POLLS)]
        unreachable_polls: u32,
    },
}
