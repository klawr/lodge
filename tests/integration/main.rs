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
fn a_binary_for_another_architecture_is_refused() {
    let sb = Sandbox::new();
    let p = sb.project("arch", &json(r#"{ "image": "@IMAGE@" }"#));
    p.up();

    // An ELF header for the architecture the container is *not*.
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

    assert_eq!(
        docker(&[
            "inspect",
            &c,
            "--format",
            "{{.HostConfig.CapAdd}} {{.HostConfig.Init}}"
        ]),
        "[CAP_SYS_PTRACE] true"
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
