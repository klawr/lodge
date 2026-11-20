//! Integration tests: the real `ldg` binary against a real container engine.
//!
//! They are `#[ignore]`d so a plain `cargo test` stays Docker-free. Run them
//! with `make integration` (quiet: a pass is a few lines, a failure shows the
//! tail of the command that failed). Each test uses its own sandbox and project,
//! so they run in parallel, and a project removes its container when dropped.
//!
//! Needs: a running docker daemon with the compose plugin, and `patchelf`.

mod support;

use support::*;

/// `.devcontainer/devcontainer.json` text with `@IMAGE@` filled in.
fn json(template: &str) -> String {
    template.replace("@IMAGE@", IMAGE)
}

fn inject_sh_config(skip_if_exists: bool) -> String {
    format!(
        "[[inject]]\nname = \"hostsh\"\nbinary = \"/bin/sh\"\nskip_if_exists = {skip_if_exists}\n"
    )
}

#[test]
#[ignore = "needs docker"]
fn up_creates_the_container_and_runs_create_hooks_once() {
    let sb = Sandbox::new();
    let p = sb.project(
        "hooks",
        &json(
            r#"{ "image": "@IMAGE@",
                 "containerEnv": { "FOO": "bar baz" },
                 "postCreateCommand": "echo ran >> /tmp/created" }"#,
        ),
    );
    p.up();
    p.up(); // reuse: the hook has succeeded once, so it must not run again

    assert_eq!(
        p.sh("wc -l < /tmp/created").trim(),
        "1",
        "postCreate ran more than once"
    );
    assert_eq!(
        p.sh("cat /etc/profile.d/lodge-env.sh"),
        "export FOO='bar baz'"
    );
    assert!(p.ldg("status", &[]).has("running"));
}

#[test]
#[ignore = "needs docker"]
fn drift_ignores_comments_and_notices_real_changes() {
    let sb = Sandbox::new();
    let p = sb.project(
        "drift",
        &json(
            r#"{ // v1
        "image": "@IMAGE@", "containerEnv": { "A": "1" } }"#,
        ),
    );
    p.up();
    assert!(p.ldg("status", &[]).has("up to date"));

    p.write(
        ".devcontainer/devcontainer.json",
        &json(
            r#"{ // a much longer comment than before
        "image": "@IMAGE@", "containerEnv": { "A": "1" } }"#,
        ),
    );
    let out = p.ldg("status", &[]);
    assert!(
        out.has("up to date"),
        "a comment-only edit counted as drift:\n{}",
        out.tail(5)
    );

    p.write(
        ".devcontainer/devcontainer.json",
        &json(r#"{ "image": "@IMAGE@", "containerEnv": { "A": "2" } }"#),
    );
    let out = p.ldg("status", &[]);
    assert!(
        out.has("changed since"),
        "a real edit went unnoticed:\n{}",
        out.tail(5)
    );
}

#[test]
#[ignore = "needs docker"]
fn status_down_and_rm_follow_the_container_lifecycle() {
    let sb = Sandbox::new();
    let p = sb.project("life", &json(r#"{ "image": "@IMAGE@" }"#));
    assert!(p.ldg("status", &[]).has("none"));
    p.up();
    assert!(p.ldg("status", &[]).has("running"));
    assert!(p.ldg("down", &[]).ok);
    assert!(p.ldg("status", &[]).has("stopped"));
    assert!(p.ldg("rm", &[]).ok);
    assert!(p.containers().is_empty(), "rm left a container behind");
    assert!(p.ldg("status", &[]).has("none"));
}

#[test]
#[ignore = "needs docker"]
fn an_injected_binary_runs_against_the_injected_glibc() {
    require_tool("patchelf", "apt install patchelf / pacman -S patchelf");
    let sb = Sandbox::new();
    sb.config(&inject_sh_config(true));
    let p = sb.project("inject", &json(r#"{ "image": "@IMAGE@" }"#));
    p.up();

    assert_eq!(p.exec(&["/usr/local/bin/hostsh", "-c", "echo ok"]), "ok");
    assert_eq!(p.sh("cat /opt/lodge/version").trim(), "1");
    assert!(
        p.sh("wc -l < /opt/lodge/lib/.manifest")
            .trim()
            .parse::<u32>()
            .unwrap()
            > 0
    );
    assert!(
        p.up().has("is up to date"),
        "a second run should find nothing to do"
    );
}

#[test]
#[ignore = "needs docker"]
fn a_library_that_changed_on_the_host_is_refreshed() {
    require_tool("patchelf", "apt install patchelf / pacman -S patchelf");
    let sb = Sandbox::new();
    sb.config(&inject_sh_config(false));
    let p = sb.project("refresh", &json(r#"{ "image": "@IMAGE@" }"#));
    p.up();

    // Simulate a host upgrade: the container's libc is stale and the manifest
    // remembers a different hash for it.
    let libc = p.sh("ls /opt/lodge/lib | grep '^libc.so'");
    p.sh(&format!(
        "echo STALE > /opt/lodge/lib/{libc} && \
         sed -i 's/^[0-9a-f]* {libc}$/0000000000000000 {libc}/' /opt/lodge/lib/.manifest"
    ));

    let container = p.container();
    let out = sb.run(&["exec", "--no-attach", &container]);
    assert!(out.ok, "re-injecting failed:\n{}", out.tail(10));
    assert_eq!(
        p.sh(&format!("head -c 4 /opt/lodge/lib/{libc} | tail -c 3")),
        "ELF",
        "libc was not restored"
    );
    assert_eq!(p.exec(&["/usr/local/bin/hostsh", "-c", "echo ok"]), "ok");
}

#[test]
#[ignore = "needs docker"]
fn packing_a_binary_for_another_architecture_than_this_machine_is_refused() {
    let sb = Sandbox::new();
    let p = sb.project("arch", &json(r#"{ "image": "@IMAGE@" }"#));
    p.up();

    // An ELF header for the architecture this machine (and so the local
    // container) is *not*. The live pack refuses it before injecting; the
    // container-side check is `a_bundle_for_another_architecture_is_refused`.
    let machine: [u8; 2] = if p.exec(&["uname", "-m"]) == "x86_64" {
        [0xB7, 0]
    } else {
        [0x3E, 0]
    };
    let mut elf = vec![0u8; 64];
    elf[..4].copy_from_slice(b"\x7fELF");
    elf[5] = 1;
    elf[18..20].copy_from_slice(&machine);
    let fake = sb.root().join("foreign-binary");
    std::fs::write(&fake, elf).unwrap();
    sb.config(&format!(
        "[[inject]]\nname = \"foreign\"\nbinary = \"{}\"\n",
        fake.display()
    ));

    let container = p.container();
    let out = sb.run(&["exec", "--no-attach", &container]);
    assert!(
        !out.ok && out.has("built for"),
        "expected an architecture refusal:\n{}",
        out.tail(6)
    );
}

/// The uid ldg should have given a user that the image created as `image_uid`.
fn uid_project<'a>(sb: &'a Sandbox, name: &str, image_uid: u32, extra: &str) -> Project<'a> {
    let p = sb.project(
        name,
        &format!(r#"{{ "build": {{ "dockerfile": "Dockerfile" }}, "remoteUser": "dev"{extra} }}"#),
    );
    p.write(
        ".devcontainer/Dockerfile",
        &format!("FROM {IMAGE}\nRUN useradd -m -u {image_uid} dev\n"),
    );
    p
}

#[test]
#[ignore = "needs docker"]
fn the_container_users_uid_is_aligned_with_the_host_user() {
    let host = host_uid();
    if host == 0 {
        eprintln!("skipped: uid alignment does nothing when the host user is root");
        return;
    }
    let sb = Sandbox::new();
    let image_uid = if host == 1500 { 1501 } else { 1500 };
    let p = uid_project(&sb, "uidsync", image_uid, "");
    p.up();
    assert_eq!(p.exec(&["id", "-u", "dev"]), host.to_string());
    assert_eq!(
        p.sh("stat -c %u /home/dev"),
        host.to_string(),
        "home was not re-owned"
    );
}

#[test]
#[ignore = "needs docker"]
fn update_remote_user_uid_false_leaves_the_uid_alone() {
    let host = host_uid();
    let sb = Sandbox::new();
    let image_uid = if host == 1500 { 1501 } else { 1500 };
    let p = uid_project(
        &sb,
        "uidoff",
        image_uid,
        r#", "updateRemoteUserUID": false"#,
    );
    p.up();
    assert_eq!(p.exec(&["id", "-u", "dev"]), image_uid.to_string());
}

#[test]
#[ignore = "needs docker"]
fn container_settings_and_variables_reach_the_container() {
    let sb = Sandbox::new();
    let port = free_port();
    let p = sb.project(
        "settings",
        &json(&format!(
            r#"{{ "image": "@IMAGE@", "workspaceFolder": "/workspaces/w",
                  "capAdd": ["SYS_PTRACE"], "init": true, "appPort": [{port}],
                  "containerEnv": {{ "X": "${{localEnv:LODGE_IT_SURELY_UNSET:dflt}}|${{containerWorkspaceFolder}}" }} }}"#
        )),
    );
    p.up();
    let c = p.container();

    // Newer docker CLIs normalise the capability to `CAP_SYS_PTRACE`, older
    // ones (27) send it as written.
    let settings = docker(&[
        "inspect",
        &c,
        "--format",
        "{{.HostConfig.CapAdd}} {{.HostConfig.Init}}",
    ]);
    assert!(
        settings == "[CAP_SYS_PTRACE] true" || settings == "[SYS_PTRACE] true",
        "{settings}"
    );
    let bindings = docker(&[
        "inspect",
        &c,
        "--format",
        "{{json .HostConfig.PortBindings}}",
    ]);
    assert!(
        bindings.contains("\"HostIp\":\"127.0.0.1\"") && bindings.contains(&port.to_string()),
        "appPort was not published on loopback only: {bindings}"
    );
    assert_eq!(
        p.sh("cat /etc/profile.d/lodge-env.sh"),
        "export X='dflt|/workspaces/w'"
    );
}

#[test]
#[ignore = "needs docker"]
fn compose_run_services_starts_only_the_named_service() {
    let sb = Sandbox::new();
    let p = sb.project(
        "compose",
        r#"{ "dockerComposeFile": "docker-compose.yml", "service": "app",
             "runServices": ["app"], "workspaceFolder": "/" }"#,
    );
    p.write(
        ".devcontainer/docker-compose.yml",
        &format!(
            "services:\n  app:\n    image: {IMAGE}\n    command: sleep infinity\n  db:\n    image: {IMAGE}\n    command: sleep infinity\n"
        ),
    );
    p.up();
    let names = p.containers();
    assert!(
        names.iter().any(|n| n.contains("-app-")),
        "app did not start: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("-db-")),
        "db should not have started: {names:?}"
    );
    assert!(p.ldg("rm", &[]).ok);
    assert!(
        p.containers().is_empty(),
        "rm left compose containers behind"
    );
}

#[test]
#[ignore = "needs docker"]
fn forwarded_ports_are_local_only_and_served_by_one_daemon() {
    let sb = Sandbox::new();
    let port = free_port();
    let p = sb.project(
        "forward",
        &json(&format!(
            r#"{{ "image": "@IMAGE@", "forwardPorts": [{port}] }}"#
        )),
    );
    p.up();
    let container = p.container();

    // The daemon starts asynchronously; wait for its listener.
    let mut waited = 0;
    while listeners(port).is_empty() && waited < 50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        waited += 1;
    }
    let addrs = listeners(port);
    assert!(
        !addrs.is_empty(),
        "nothing is listening on the forwarded port"
    );
    assert!(
        addrs.iter().all(|a| a == "0100007F"),
        "forwarded port is exposed beyond loopback: {addrs:?}"
    );

    p.up(); // every `up` starts a daemon; only one may survive
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(
        forward_daemons(&container),
        1,
        "expected exactly one forward daemon"
    );
}

#[test]
#[ignore = "needs docker"]
fn remote_mode_works_through_ssh_without_a_local_tar() {
    let mut sb = Sandbox::new();
    sb.use_fake_ssh();

    // A config directory with a symlink, copied by streaming a tar over ssh.
    let cfg = sb.root().join("cfg");
    std::fs::create_dir_all(cfg.join("sub/deep")).unwrap();
    std::fs::write(cfg.join("sub/deep/file.txt"), "hello").unwrap();
    std::os::unix::fs::symlink("sub/deep/file.txt", cfg.join("link")).unwrap();
    sb.config(&format!(
        "[[inject]]\nname = \"cfg\"\nconfig = \"{}\"\ntarget_config = \"/root/.config/cfg\"\n",
        cfg.display()
    ));

    let p = sb
        .project(
            "remote",
            &json(r#"{ "image": "@IMAGE@", "initializeCommand": "touch ../init-ran" }"#),
        )
        .remote();
    p.up();

    assert_eq!(p.sh("cat /root/.config/cfg/sub/deep/file.txt"), "hello");
    assert_eq!(p.sh("readlink /root/.config/cfg/link"), "sub/deep/file.txt");
    assert!(
        p.path().join("init-ran").exists(),
        "initializeCommand did not run on the project host"
    );
    assert!(
        p.ldg("status", &[]).has("up to date"),
        "the stored hash was not read back"
    );

    // With -v every spawned command is traced; none may be a local `tar`.
    let out = sb.run(&[
        "-v",
        "--remote",
        "fakehost",
        "up",
        "--no-attach",
        &p.path().to_string_lossy(),
    ]);
    assert!(out.ok, "{}", out.tail(10));
    let tar = out
        .text
        .lines()
        .filter(|l| l.starts_with("[lodge] $ tar "))
        .count();
    assert_eq!(tar, 0, "a local tar was spawned");
}

#[test]
#[ignore = "needs docker"]
fn doctor_reports_a_reachable_engine() {
    require_tool("patchelf", "apt install patchelf / pacman -S patchelf");
    let sb = Sandbox::new();
    let out = sb.run(&["doctor"]);
    assert!(out.ok && out.has("container engine"), "{}", out.tail(12));
}

#[test]
#[ignore = "needs docker"]
fn a_bundle_can_be_packed_inspected_verified_and_damage_is_caught() {
    require_tool("patchelf", "apt install patchelf / pacman -S patchelf");
    let sb = Sandbox::new();
    sb.config(&format!(
        "{}\n[aliases]\nll = \"ls -la\"\n",
        inject_sh_config(true)
    ));
    let bundle = sb.root().join("b.tar");
    let bundle_arg = bundle.to_string_lossy().into_owned();

    let out = sb.run(&["pack", "-o", &bundle_arg]);
    assert!(out.ok, "pack failed:\n{}", out.tail(10));
    let id = out
        .text
        .split("id ")
        .nth(1)
        .map(|s| s.trim().trim_end_matches(')').to_string());
    assert!(id.is_some(), "pack did not report an id:\n{}", out.tail(5));

    let inspect = sb.run(&["bundle", "inspect", &bundle_arg]);
    assert!(
        inspect.ok && inspect.has("hostsh") && inspect.has("relinked") && inspect.has("1 alias"),
        "unexpected inspect output:\n{}",
        inspect.text
    );
    assert!(sb.run(&["bundle", "verify", &bundle_arg]).ok);

    // Packing the same inputs again gives the same bundle id.
    let again = sb.root().join("again.tar");
    let out2 = sb.run(&["pack", "-o", &again.to_string_lossy()]);
    assert_eq!(
        out.text.split("id ").nth(1),
        out2.text.split("id ").nth(1),
        "the id changed although nothing did"
    );

    // Flip one byte of file data: the tar is still a valid tar, but a library no
    // longer matches its recorded hash.
    let mut bytes = std::fs::read(&bundle).unwrap();
    let at = bytes.len() * 3 / 4;
    bytes[at] ^= 0xFF;
    std::fs::write(&bundle, bytes).unwrap();
    let damaged = sb.run(&["bundle", "verify", &bundle_arg]);
    assert!(
        !damaged.ok && damaged.has("damaged"),
        "damage went unnoticed:\n{}",
        damaged.tail(5)
    );
}

/// Rewrites a bundle's manifest so it claims `arch`, leaving everything else as is.
fn with_arch(src: &std::path::Path, dst: &std::path::Path, arch: &str) {
    use std::io::Read;
    let mut input = tar::Archive::new(std::fs::File::open(src).unwrap());
    let mut out = tar::Builder::new(std::fs::File::create(dst).unwrap());
    for entry in input.entries().unwrap() {
        let mut entry = entry.unwrap();
        let mut header = entry.header().clone();
        let path = entry.path().unwrap().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if path == std::path::Path::new("manifest.json") {
            let mut manifest: serde_json::Value = serde_json::from_slice(&data).unwrap();
            manifest["arch"] = arch.into();
            data = serde_json::to_vec(&manifest).unwrap();
            header.set_size(data.len() as u64);
        }
        header.set_cksum();
        out.append_data(&mut header, &path, &data[..]).unwrap();
    }
    out.finish().unwrap();
}

/// A config with one script tool (no libraries to resolve, so no ldd or patchelf).
fn script_tool_config(sb: &Sandbox, name: &str, body: &str) -> std::path::PathBuf {
    let script = sb.root().join(format!("{name}-src"));
    std::fs::write(&script, body).unwrap();
    sb.config(&format!(
        "[[inject]]\nname = \"{name}\"\nbinary = \"{}\"\ncopy_deps = false\n",
        script.display()
    ));
    script
}

#[test]
#[ignore = "needs docker"]
fn a_bundle_for_another_architecture_is_refused() {
    let sb = Sandbox::new();
    script_tool_config(&sb, "hello", "#!/bin/sh\necho hi\n");
    let p = sb.project("bundlearch", &json(r#"{ "image": "@IMAGE@" }"#));
    p.up();

    let bundle = sb.root().join("b.tar");
    assert!(sb.run(&["pack", "-o", &bundle.to_string_lossy()]).ok);
    let foreign = sb.root().join("foreign.tar");
    let other = if p.exec(&["uname", "-m"]) == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    with_arch(&bundle, &foreign, other);

    let container = p.container();
    let out = sb.run(&[
        "exec",
        "--no-attach",
        "--bundle",
        &foreign.to_string_lossy(),
        &container,
    ]);
    assert!(
        !out.ok && out.has("the bundle is for"),
        "expected an architecture refusal:\n{}",
        out.tail(6)
    );
}

#[test]
#[ignore = "needs docker"]
fn a_repeated_up_reuses_the_cached_bundle_and_a_change_updates_the_container() {
    let sb = Sandbox::new();
    let script = script_tool_config(&sb, "hello", "#!/bin/sh\necho one\n");
    let p = sb.project("cached", &json(r#"{ "image": "@IMAGE@" }"#));

    assert!(p.up().has("(packed)"), "the first run should pack");
    assert_eq!(p.exec(&["/usr/local/bin/hello"]), "one");
    let again = p.up();
    assert!(
        again.has("(cached)"),
        "unchanged inputs should reuse the bundle:\n{}",
        again.tail(8)
    );
    assert!(again.has("is up to date"));

    // A different script: the bundle is repacked and the container updated.
    std::fs::write(&script, "#!/bin/sh\necho two, and longer\n").unwrap();
    let changed = p.up();
    assert!(
        changed.has("(packed)") && changed.has("injecting hello"),
        "{}",
        changed.tail(8)
    );
    assert_eq!(p.exec(&["/usr/local/bin/hello"]), "two, and longer");
}

#[test]
#[ignore = "needs docker"]
fn injecting_from_a_bundle_needs_neither_ldd_nor_patchelf() {
    require_tool("patchelf", "apt install patchelf / pacman -S patchelf");
    let sb = Sandbox::new();
    sb.config(&inject_sh_config(false));
    let bundle = sb.root().join("b.tar");
    let bundle_arg = bundle.to_string_lossy().into_owned();
    assert!(
        sb.run(&["pack", "-o", &bundle_arg]).ok,
        "packing needs the full PATH"
    );

    let p = sb.project("nodeps", &json(r#"{ "image": "@IMAGE@" }"#));
    let path_arg = p.path().to_string_lossy().into_owned();
    // `ldg` itself needs docker, and a shell and `id` for the uid check; nothing else.
    let minimal = sb.restricted_path(&["docker", "sh", "id"]);

    // Control: without a bundle ldg must pack, which needs ldd, so it cannot work here.
    let live = sb.run_with_path(&minimal, &["up", "--no-attach", &path_arg]);
    assert!(
        !live.ok && (live.has("ldd") || live.has("patchelf")),
        "the restricted PATH did not take away ldd and patchelf:\n{}",
        live.tail(6)
    );

    // With the bundle it needs neither.
    let out = sb.run_with_path(
        &minimal,
        &["up", "--no-attach", "--bundle", &bundle_arg, &path_arg],
    );
    assert!(
        out.ok,
        "injecting from a bundle failed without ldd/patchelf:\n{}",
        out.tail(10)
    );
    assert_eq!(p.exec(&["/usr/local/bin/hostsh", "-c", "echo ok"]), "ok");
}

const SECRET: &str = "s3cr3t-VALUE-9f2a";
const FILE_SECRET: &str = "f1le-VALUE-77c1";

/// A config declaring an env secret (from the process environment), a file secret
/// (from a host file, linked into the container), and one that is for another
/// target.
fn secrets_config(sb: &Sandbox) {
    let host_file = sb.root().join("gh-token");
    std::fs::write(&host_file, format!("{FILE_SECRET}\n")).unwrap();
    sb.config(&format!(
        r#"
[[secret]]
name = "API_KEY"
from = {{ env = "LODGE_IT_API_KEY" }}
as = "env"
targets = ["local"]

[[secret]]
name = "GH"
from = {{ file = "{}" }}
as = {{ file = "/root/.cfg/gh-token", mode = "0640" }}

[[secret]]
name = "WORK_KEY"
from = {{ command = "echo work-VALUE-31b8" }}
as = "env"
targets = ["no-such-host-*"]
"#,
        host_file.display()
    ));
}

#[test]
#[ignore = "needs docker"]
fn secrets_reach_the_session_and_never_the_image_the_trace_or_the_disk() {
    let sb = Sandbox::new();
    secrets_config(&sb);
    let p = sb.project("secrets", &json(r#"{ "image": "@IMAGE@" }"#));
    let host_env = [("LODGE_IT_API_KEY", SECRET)];

    // A traced run whose session prints nothing: the value must not appear.
    let traced = p.attach(&["-v"], &host_env, "exit\n");
    assert!(traced.ok, "ldg up failed:\n{}", traced.tail(15));
    for value in [SECRET, FILE_SECRET] {
        assert!(!traced.has(value), "the -v run printed a secret value");
    }
    assert!(traced.has("secrets: API_KEY, GH"), "{}", traced.tail(15));
    assert!(
        traced.has("not for this target: WORK_KEY"),
        "{}",
        traced.tail(15)
    );

    // The session has the variable and the credential file.
    let session = p.attach(
        &[],
        &host_env,
        "printf 'GOT=%s\\n' \"$API_KEY\"; printf 'FILE=%s\\n' \"$(cat /root/.cfg/gh-token)\"; \
         printf 'WORK=[%s]\\n' \"${WORK_KEY:-}\"; exit\n",
    );
    assert!(
        session.has(&format!("GOT={SECRET}")),
        "{}",
        session.tail(15)
    );
    assert!(
        session.has(&format!("FILE={FILE_SECRET}")),
        "{}",
        session.tail(15)
    );
    assert!(
        session.has("WORK=[]"),
        "a secret for another target arrived"
    );

    // In memory only, owner-only.
    let c = p.container();
    assert_eq!(p.sh("stat -f -c %T /dev/shm").trim(), "tmpfs");
    assert_eq!(p.sh("stat -c %a /dev/shm/lodge-secrets-0").trim(), "700");
    assert_eq!(
        p.sh("stat -c %a /dev/shm/lodge-secrets-0/API_KEY").trim(),
        "600"
    );
    assert_eq!(p.sh("stat -c %a /dev/shm/lodge-secrets-0/GH").trim(), "640");
    assert_eq!(p.sh("ls /dev/shm/lodge-secrets-0").lines().count(), 2);

    // Not in the container's configuration, nor in any layer or file on disk.
    let inspect = docker(&["inspect", &c]);
    let diff = docker(&["diff", &c]);
    assert!(
        !diff.contains("lodge-secrets"),
        "a secret reached the filesystem layer:\n{diff}"
    );
    let export = std::process::Command::new("sh")
        .args([
            "-c",
            &format!("docker export {c} | grep -a -c -e {SECRET} -e {FILE_SECRET} -e work-VALUE"),
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&export.stdout).trim(),
        "0",
        "a secret value was found in the container's filesystem"
    );
    for value in [SECRET, FILE_SECRET, "work-VALUE"] {
        assert!(!inspect.contains(value), "{value} is in docker inspect");
    }

    // status and doctor show names and states, never values.
    let status = p.ldg("status", &[]);
    assert!(
        status.has("API_KEY (placed)") && status.has("GH (placed)"),
        "{}",
        status.tail(8)
    );
    assert!(status.has("WORK_KEY (not placed)"), "{}", status.tail(8));
    let doctor = {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_ldg"));
        cmd.arg("doctor")
            .env("XDG_CONFIG_HOME", sb.root().join("config"))
            .env("LODGE_IT_API_KEY", SECRET);
        let o = cmd.output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
    };
    assert!(
        doctor.contains("secret API_KEY") && doctor.contains("resolves"),
        "{doctor}"
    );
    for value in [SECRET, FILE_SECRET, "work-VALUE"] {
        assert!(!doctor.contains(value), "doctor printed a secret value");
    }

    // Dropping a declaration removes its value on the next attach.
    sb.config("");
    let again = p.attach(&[], &host_env, "exit\n");
    assert!(again.ok, "{}", again.tail(10));
    // (no secrets declared at all: nothing runs, the old files stay until stop)
    secrets_config(&sb);
    let one = std::fs::read_to_string(sb.root().join("config/lodge/config.toml"))
        .unwrap()
        .replace("name = \"GH\"", "name = \"GH_RENAMED\"");
    std::fs::write(sb.root().join("config/lodge/config.toml"), one).unwrap();
    p.attach(&[], &host_env, "exit\n");
    let left = p.sh("ls /dev/shm/lodge-secrets-0");
    assert!(
        left.contains("GH_RENAMED") && !left.lines().any(|l| l == "GH"),
        "{left}"
    );
}

#[test]
#[ignore = "needs docker"]
fn a_secret_that_cannot_be_resolved_warns_and_the_session_still_opens() {
    let sb = Sandbox::new();
    sb.config(
        r#"
[[secret]]
name = "MISSING"
from = { env = "LODGE_IT_SURELY_UNSET" }
as = "env"

[[secret]]
name = "PRESENT"
from = { command = "echo here" }
as = "env"
"#,
    );
    let p = sb.project("secret-warn", &json(r#"{ "image": "@IMAGE@" }"#));
    let out = p.attach(
        &[],
        &[],
        "printf 'P=%s M=[%s]\\n' \"$PRESENT\" \"${MISSING:-}\"; exit\n",
    );
    assert!(out.ok, "{}", out.tail(15));
    assert!(
        out.has("secret not delivered") && out.has("MISSING"),
        "{}",
        out.tail(15)
    );
    assert!(out.has("P=here M=[]"), "{}", out.tail(15));
}

#[test]
#[ignore = "needs docker"]
fn secrets_are_added_on_the_machine_that_uses_a_tarred_bundle() {
    let sb = Sandbox::new();
    // The packing machine declares a secret too; it must stay out of the bundle.
    let script = script_tool_config(&sb, "hello", "#!/bin/sh\necho tool-ran-from-bundle\n");
    sb.config(&format!(
        "[[inject]]\nname = \"hello\"\nbinary = \"{}\"\ncopy_deps = false\n\n[[secret]]\nname = \"API_KEY\"\nfrom = {{ env = \"LODGE_IT_API_KEY\" }}\nas = \"env\"\n",
        script.display()
    ));
    let bundle = sb.root().join("b.tar");
    assert!(sb.run(&["pack", "-o", &bundle.to_string_lossy()]).ok);
    let tar = std::process::Command::new("tar")
        .args(["-xOf", &bundle.to_string_lossy()])
        .output()
        .unwrap();
    let contents = String::from_utf8_lossy(&tar.stdout);
    assert!(
        !contents.contains("API_KEY") && !contents.contains("LODGE_IT_API_KEY"),
        "the bundle carries a secret declaration"
    );

    // The consuming machine: a folder with the tar and a config that refers to it
    // by a relative path, used through `-c`. The default config is empty.
    let deploy = sb.root().join("deploy");
    std::fs::create_dir_all(&deploy).unwrap();
    std::fs::copy(&bundle, deploy.join("bundle.tar")).unwrap();
    let deploy_config = deploy.join("config.toml");
    std::fs::write(
        &deploy_config,
        "bundle = \"bundle.tar\"\n\n[session]\nmultiplexer = \"none\"\n\n\
         [[secret]]\nname = \"API_KEY\"\nfrom = { env = \"LODGE_IT_API_KEY\" }\nas = \"env\"\n",
    )
    .unwrap();
    sb.config("");
    let config_arg = deploy_config.to_string_lossy().into_owned();
    let missing = sb.run(&["-c", "/no/such/config.toml", "doctor"]);
    assert!(
        !missing.ok && missing.has("does not exist"),
        "{}",
        missing.tail(5)
    );
    let p = sb.project("bundle-secret", &json(r#"{ "image": "@IMAGE@" }"#));
    let out = p.attach(
        &["-c", &config_arg],
        &[("LODGE_IT_API_KEY", SECRET)],
        "printf 'GOT=%s\\n' \"$API_KEY\"; /usr/local/bin/hello; exit\n",
    );
    assert!(out.ok, "{}", out.tail(15));
    assert!(out.has(&format!("GOT={SECRET}")), "{}", out.tail(15));
    assert!(
        out.has("tool-ran-from-bundle"),
        "the bundle's tool was not injected:\n{}",
        out.tail(15)
    );
}

#[test]
#[ignore = "needs docker"]
fn hooks_and_feature_scripts_run_as_the_remote_user() {
    let sb = Sandbox::new();
    let p = sb.project(
        "remoteuser",
        &json(
            r#"{ "image": "@IMAGE@",
                 "remoteUser": "nobody",
                 "updateRemoteUserUID": false,
                 "features": { "./who": {} },
                 "postCreateCommand": "id -un > /tmp/hook-user" }"#,
        ),
    );
    p.write(
        ".devcontainer/who/devcontainer-feature.json",
        r#"{ "id": "who" }"#,
    );
    p.write(
        ".devcontainer/who/install.sh",
        "#!/bin/sh\necho \"$(id -un) $_REMOTE_USER $_REMOTE_USER_HOME\" > /tmp/feature-users\n",
    );
    p.up();

    assert_eq!(
        p.sh("cat /tmp/hook-user"),
        "nobody",
        "the hook ran as the wrong user"
    );
    // The script itself runs as root, and is told who the remote user is.
    assert_eq!(p.sh("cat /tmp/feature-users"), "root nobody /nonexistent");
}

#[test]
#[ignore = "needs docker"]
fn the_same_folder_is_the_same_container_however_it_is_spelled() {
    let sb = Sandbox::new();
    let p = sb.project("spelling", &json(r#"{ "image": "@IMAGE@" }"#));
    p.up();
    // The same folder through `..`: canonicalised, so no second container.
    let dir = p.path();
    let roundabout = dir.join("..").join(dir.file_name().expect("a folder name"));
    let out = sb.run(&["up", "--no-attach", &roundabout.to_string_lossy()]);
    assert!(out.ok, "{}", out.tail(10));
    assert_eq!(p.containers().len(), 1, "a second container was created");
}

#[test]
#[ignore = "needs docker"]
fn local_features_are_read_from_the_project_host_over_ssh() {
    let mut sb = Sandbox::new();
    sb.use_fake_ssh();
    let p = sb
        .project(
            "remotefeature",
            &json(r#"{ "image": "@IMAGE@", "features": { "./mine": {} } }"#),
        )
        .remote();
    p.write(
        ".devcontainer/mine/devcontainer-feature.json",
        r#"{ "id": "mine" }"#,
    );
    p.write(
        ".devcontainer/mine/install.sh",
        "#!/bin/sh\necho installed > /tmp/feature-ran\n",
    );
    let out = p.ldg("-v", &["up", "--no-attach"]);
    assert!(out.ok, "{}", out.tail(15));
    assert_eq!(p.sh("cat /tmp/feature-ran"), "installed");
    // Read through ssh (a tar stream from the project host), not from local disk.
    assert!(
        out.text
            .lines()
            .any(|l| l.starts_with("[lodge] $ ssh") && l.contains("tar -C")),
        "the feature was not fetched over ssh:\n{}",
        out.tail(30)
    );
}

#[test]
#[ignore = "needs docker"]
fn ls_status_ports_and_clean_describe_and_undo_what_up_did() {
    require_tool("patchelf", "apt install patchelf / pacman -S patchelf");
    let sb = Sandbox::new();
    sb.config(&inject_sh_config(false));
    let port = free_port();
    let p = sb.project(
        "manage",
        &json(&format!(
            r#"{{ "image": "@IMAGE@", "forwardPorts": [{port}],
                  "postCreateCommand": "true" }}"#
        )),
    );
    p.up();
    let path = p.path().to_string_lossy().into_owned();

    // ls: labelled with the project, across projects.
    let ls = sb.run(&["ls", "--json"]);
    assert!(ls.ok, "{}", ls.tail(10));
    let listed: serde_json::Value = serde_json::from_str(&ls.stdout).expect("ls --json");
    assert!(
        listed
            .as_array()
            .expect("an array")
            .iter()
            .any(|c| c["project"] == path.as_str() && c["running"] == true),
        "{}",
        ls.stdout
    );

    // status --json.
    let status = p.ldg("status", &["--json"]);
    let status: serde_json::Value = serde_json::from_str(&status.stdout).expect("status --json");
    assert_eq!(status["container"]["running"], true);
    assert_eq!(status["container"]["config"], "up_to_date");

    // ports: the daemon reports the declared forward once it is up.
    let mut shown = String::new();
    for _ in 0..50 {
        shown = p.ldg("ports", &[]).text;
        if shown.contains(&format!("127.0.0.1:{port}")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(shown.contains(&format!("127.0.0.1:{port}")), "{shown}");
    assert!(p.ldg("logs", &[]).has("forwarding port"));

    // clean: lodge's files and wrapper go, the devcontainer's state stays.
    assert!(p.sh("test -x /usr/local/bin/hostsh && echo yes") == "yes");
    // As an older ldg left it: the record has the hash but no path, so the
    // wrapper must be found by looking.
    p.sh("sed -i 2d /opt/lodge/installed/hostsh.bin");
    let out = p.ldg("clean", &[]);
    assert!(
        out.ok && out.has("/usr/local/bin/hostsh"),
        "{}",
        out.tail(10)
    );
    assert_eq!(
        p.sh("ls -d /opt/lodge/bin /lodge /usr/local/bin/hostsh 2>/dev/null | wc -l"),
        "0"
    );
    assert_eq!(p.sh("ls /opt/lodge/state"), "postCreate");
    assert!(p.ldg("status", &[]).has("up to date"));

    // And the next `up` puts it back.
    let again = p.up();
    assert!(again.has("injecting hostsh binary"), "{}", again.tail(10));
}

#[test]
#[ignore = "needs docker"]
fn a_typo_in_the_config_stops_before_anything_starts() {
    let sb = Sandbox::new();
    sb.config("[[inject]]\nname = \"x\"\nbinary = \"/bin/sh\"\nskip_if_exist = true\n");
    let p = sb.project("typo", &json(r#"{ "image": "@IMAGE@" }"#));
    let out = p.ldg("up", &["--no-attach"]);
    assert!(!out.ok);
    assert!(out.has("did you mean `skip_if_exists`?"), "{}", out.tail(5));
    assert!(p.containers().is_empty(), "a container was created anyway");
    let doctor = sb.run(&["doctor"]);
    assert!(
        !doctor.ok && doctor.has("MISSING   config"),
        "{}",
        doctor.tail(12)
    );
}

#[test]
#[ignore = "needs docker"]
fn init_writes_a_config_doctor_accepts_and_completions_and_man_render() {
    let sb = Sandbox::new();
    std::fs::remove_file(sb.root().join("config/lodge/config.toml")).expect("no config");
    let out = sb.run(&["init"]);
    assert!(out.ok, "{}", out.tail(5));
    let written = std::fs::read_to_string(sb.root().join("config/lodge/config.toml")).unwrap();
    assert!(written.starts_with("#:schema "));
    assert!(!sb.run(&["init"]).ok, "init overwrote an existing config");
    assert!(sb.run(&["init", "--force"]).ok);
    let doctor = sb.run(&["doctor"]);
    assert!(doctor.has("ok        config"), "{}", doctor.tail(12));

    assert!(sb.run(&["completions", "bash"]).stdout.contains("_ldg"));
    assert!(sb.run(&["man"]).stdout.contains(".TH ldg"));
}

#[test]
#[ignore = "needs docker"]
fn a_tmux_session_runs_the_shell_in_the_container() {
    require_tool("tmux", "apt install tmux / pacman -S tmux");
    let sb = Sandbox::new();
    sb.config_raw("[session]\nmultiplexer = \"tmux\"\n");
    let p = sb.project("tmux", &json(r#"{ "image": "@IMAGE@" }"#));
    let out = p.attach(&[], &[], "echo in-tmux > /tmp/tmux-proof; exit\n");
    assert!(!out.has("tmux not found"), "the session did not use tmux");
    assert!(out.ok, "ldg up failed:\n{}", out.tail(15));
    assert_eq!(p.sh("cat /tmp/tmux-proof"), "in-tmux");
}

#[test]
#[ignore = "needs docker"]
fn a_glibc_tool_runs_in_a_musl_container() {
    require_tool("patchelf", "apt install patchelf / pacman -S patchelf");
    let sb = Sandbox::new();
    sb.config(&inject_sh_config(false));
    let p = sb.project(
        "alpine",
        r#"{ "image": "alpine:3", "remoteUser": "nobody", "updateRemoteUserUID": false,
             "postCreateCommand": "id -un > /tmp/hook-user" }"#,
    );
    p.up();
    // The host's (glibc) shell, with its bundled libraries, on busybox/musl.
    assert_eq!(
        p.exec(&["/usr/local/bin/hostsh", "-c", "echo glibc-on-musl"]),
        "glibc-on-musl"
    );
    assert_eq!(p.sh("cat /tmp/hook-user"), "nobody");
    assert!(p.ldg("status", &[]).has("up to date"));
}

#[test]
#[ignore = "needs docker"]
fn remote_mode_works_against_a_real_ssh_server() {
    let mut sb = Sandbox::new();
    let host = sb.use_real_sshd();
    let cfg = sb.root().join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("rc"), "hello over ssh").unwrap();
    sb.config(&format!(
        "[[inject]]\nname = \"cfg\"\nconfig = \"{}\"\ntarget_config = \"/root/.config/cfg\"\n",
        cfg.display()
    ));
    let port = free_port();
    let p = sb
        .project(
            "realssh",
            &json(&format!(
                r#"{{ // comments must not count as a change
                      "image": "@IMAGE@", "forwardPorts": [{port}],
                      "initializeCommand": "touch ../init-ran" }}"#
            )),
        )
        .remote_via(host.dest());
    p.up();

    assert_eq!(p.sh("cat /root/.config/cfg/rc"), "hello over ssh");
    assert!(
        p.path().join("init-ran").exists(),
        "initializeCommand did not run"
    );
    assert!(
        p.ldg("status", &[]).has("up to date"),
        "the hash differs over ssh"
    );

    // A comment-only edit is not a change, over ssh as locally.
    let dc = p.path().join(".devcontainer/devcontainer.json");
    let text = std::fs::read_to_string(&dc).unwrap();
    std::fs::write(&dc, text.replace("comments must", "comments still must")).unwrap();
    assert!(p.ldg("status", &[]).has("up to date"));

    // The forward goes through an ssh -L tunnel.
    let mut shown = String::new();
    for _ in 0..80 {
        shown = p.ldg("ports", &[]).text;
        if shown.contains(&format!("127.0.0.1:{port}")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(shown.contains(&format!("127.0.0.1:{port}")), "{shown}");
    assert!(
        !listeners(port).is_empty(),
        "the tunnel isn't listening here"
    );
}
