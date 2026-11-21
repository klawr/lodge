use super::credentials::fake::FakeCredentials;
use super::digest::Digest;
use super::http::fake::FakeHttp;
use super::*;
use crate::tools::OciFetcher;
use std::sync::Arc;
use std::time::Duration;

const REPO: &str = "https://ghcr.io/v2/x/feat";
/// What a pull of `REPO` asks a token service for, when the registry names no scope.
const SCOPE: &str = "repository%3Ax%2Ffeat%3Apull";

fn layer() -> (Digest, Vec<u8>) {
    let body = b"the layer".to_vec();
    (Digest::of(&body), body)
}

fn image(layer: &Digest, size: usize) -> Vec<u8> {
    format!(r#"{{"layers": [{{"digest": "{layer}", "size": {size}}}]}}"#).into_bytes()
}

/// A registry serving one image under the tag `1`.
fn serving(layer: &(Digest, Vec<u8>)) -> FakeHttp {
    FakeHttp::new()
        .answer(
            &format!("{REPO}/manifests/1"),
            200,
            &[],
            &image(&layer.0, layer.1.len()),
        )
        .answer(&format!("{REPO}/blobs/{}", layer.0), 200, &[], &layer.1)
}

fn oci(http: &Arc<FakeHttp>, creds: FakeCredentials) -> NativeOci {
    NativeOci::new(Box::new(http.clone()), Box::new(creds)).retrying(&[Duration::ZERO; 3])
}

fn pull(
    http: &Arc<FakeHttp>,
    creds: FakeCredentials,
    arch: Option<Arch>,
) -> (Result<()>, Vec<String>) {
    let dir = crate::tmpfile::TempDir::new("oci-test").unwrap();
    let result = oci(http, creds).pull("ghcr.io/x/feat:1", arch, dir.path());
    let mut left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    (result, left)
}

#[test]
fn an_image_is_pulled_as_a_file_named_by_its_digest() {
    let l = layer();
    let http = Arc::new(serving(&l));
    let dir = crate::tmpfile::TempDir::new("oci-test").unwrap();
    oci(&http, FakeCredentials::new())
        .pull("ghcr.io/x/feat:1", None, dir.path())
        .unwrap();
    let written = std::fs::read(dir.path().join(l.0.file_name())).unwrap();
    assert_eq!(written, l.1);
    let accept = http
        .sent(&format!("{REPO}/manifests/1"), 0, "accept")
        .unwrap();
    assert!(
        accept.contains("application/vnd.oci.image.index.v1+json"),
        "{accept}"
    );
}

#[test]
fn an_index_gives_the_image_for_the_architecture() {
    let l = layer();
    let amd = image(&l.0, l.1.len());
    let amd_digest = Digest::of(&amd);
    let index = format!(
        r#"{{"manifests": [
            {{"digest": "{amd_digest}", "size": {}, "platform": {{"architecture": "amd64", "os": "linux"}}}},
            {{"digest": "sha256:{}", "size": 1, "platform": {{"architecture": "arm64", "os": "linux"}}}}
        ]}}"#,
        amd.len(),
        "0".repeat(64)
    );
    let http = Arc::new(
        FakeHttp::new()
            .answer(&format!("{REPO}/manifests/1"), 200, &[], index.as_bytes())
            .answer(&format!("{REPO}/manifests/{amd_digest}"), 200, &[], &amd)
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], &l.1),
    );
    let (result, files) = pull(&http, FakeCredentials::new(), Some(Arch::X86_64));
    result.unwrap();
    assert_eq!(files, vec![l.0.file_name()]);
}

#[test]
fn an_index_with_two_images_needs_an_architecture() {
    let index = format!(
        r#"{{"manifests": [
            {{"digest": "sha256:{0}", "size": 1, "platform": {{"architecture": "amd64", "os": "linux"}}}},
            {{"digest": "sha256:{0}", "size": 1, "platform": {{"architecture": "arm64", "os": "linux"}}}}
        ]}}"#,
        "0".repeat(64)
    );
    let http = Arc::new(FakeHttp::new().answer(
        &format!("{REPO}/manifests/1"),
        200,
        &[],
        index.as_bytes(),
    ));
    let (result, _) = pull(&http, FakeCredentials::new(), None);
    assert!(format!("{:#}", result.unwrap_err()).contains("no architecture"));
}

#[test]
fn a_bearer_challenge_is_answered_with_a_token_and_the_request_repeated() {
    let l = layer();
    let token = "https://ghcr.io/token?service=ghcr.io&scope=repository%3Ax%2Ffeat%3Apull";
    let challenge =
        r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:x/feat:pull""#;
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                401,
                &[("WWW-Authenticate", challenge)],
                b"",
            )
            .answer(token, 200, &[], br#"{"token": "T0K"}"#)
            .answer(
                &format!("{REPO}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], &l.1),
    );
    let (result, files) = pull(
        &http,
        FakeCredentials::new().with("ghcr.io", "kai", "pw"),
        None,
    );
    result.unwrap();
    assert_eq!(files.len(), 1);
    // The token service is asked as the user, the registry with the token.
    assert!(
        http.sent(token, 0, "authorization")
            .unwrap()
            .starts_with("Basic ")
    );
    assert_eq!(
        http.sent(&format!("{REPO}/manifests/1"), 0, "authorization"),
        None
    );
    assert_eq!(
        http.sent(&format!("{REPO}/manifests/1"), 1, "authorization")
            .as_deref(),
        Some("Bearer T0K")
    );
    // and the blob, which the token also covers.
    assert_eq!(
        http.sent(&format!("{REPO}/blobs/{}", l.0), 0, "authorization")
            .as_deref(),
        Some("Bearer T0K")
    );
}

#[test]
fn without_a_login_the_token_service_is_asked_anonymously() {
    let l = layer();
    let challenge = r#"Bearer realm="https://ghcr.io/token""#;
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                401,
                &[("www-authenticate", challenge)],
                b"",
            )
            .answer(
                &format!("https://ghcr.io/token?scope={SCOPE}"),
                200,
                &[],
                br#"{"access_token": "A"}"#,
            )
            .answer(
                &format!("{REPO}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], &l.1),
    );
    pull(&http, FakeCredentials::new(), None).0.unwrap();
    assert_eq!(
        http.sent(
            &format!("https://ghcr.io/token?scope={SCOPE}"),
            0,
            "authorization"
        ),
        None
    );
}

#[test]
fn a_broken_login_source_falls_back_to_anonymous() {
    let l = layer();
    let challenge = r#"Bearer realm="https://ghcr.io/token""#;
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                401,
                &[("www-authenticate", challenge)],
                b"",
            )
            .answer(
                &format!("https://ghcr.io/token?scope={SCOPE}"),
                200,
                &[],
                br#"{"token": "A"}"#,
            )
            .answer(
                &format!("{REPO}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], &l.1),
    );
    pull(
        &http,
        FakeCredentials::new().broken("ghcr.io", "bad config"),
        None,
    )
    .0
    .unwrap();
}

#[test]
fn a_login_is_never_sent_to_a_token_service_without_tls() {
    let challenge = r#"Bearer realm="http://evil.example/token""#;
    let http = Arc::new(FakeHttp::new().answer(
        &format!("{REPO}/manifests/1"),
        401,
        &[("www-authenticate", challenge)],
        b"",
    ));
    let (result, _) = pull(
        &http,
        FakeCredentials::new().with("ghcr.io", "kai", "pw"),
        None,
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("not https"));
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}

#[test]
fn a_challenge_without_a_scope_asks_for_a_pull_of_the_repository() {
    let l = layer();
    let challenge = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#;
    let token = "https://ghcr.io/token?service=ghcr.io&scope=repository%3Ax%2Ffeat%3Apull";
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                401,
                &[("www-authenticate", challenge)],
                b"",
            )
            .answer(token, 200, &[], br#"{"token": "A"}"#)
            .answer(
                &format!("{REPO}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], &l.1),
    );
    pull(&http, FakeCredentials::new(), None).0.unwrap();
}

#[test]
fn a_token_that_goes_stale_mid_pull_is_renewed_once() {
    let l = layer();
    let challenge = r#"Bearer realm="https://ghcr.io/token",scope="repository:x/feat:pull""#;
    let token = "https://ghcr.io/token?scope=repository%3Ax%2Ffeat%3Apull";
    let blob = format!("{REPO}/blobs/{}", l.0);
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                401,
                &[("www-authenticate", challenge)],
                b"",
            )
            .answer(token, 200, &[], br#"{"token": "OLD"}"#)
            .answer(
                &format!("{REPO}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(&blob, 401, &[("www-authenticate", challenge)], b"")
            .answer(token, 200, &[], br#"{"token": "NEW"}"#)
            .answer(&blob, 200, &[], &l.1),
    );
    pull(&http, FakeCredentials::new(), None).0.unwrap();
    assert_eq!(
        http.sent(&blob, 1, "authorization").as_deref(),
        Some("Bearer NEW")
    );
}

#[test]
fn an_ipv6_loopback_registry_is_spoken_to_without_tls() {
    let l = layer();
    for (registry, base) in [
        ("[::1]", "http://[::1]/v2"),
        ("[::1]:5000", "http://[::1]:5000/v2"),
    ] {
        let http = Arc::new(
            FakeHttp::new()
                .answer(
                    &format!("{base}/x/feat/manifests/1"),
                    200,
                    &[],
                    &image(&l.0, l.1.len()),
                )
                .answer(&format!("{base}/x/feat/blobs/{}", l.0), 200, &[], &l.1),
        );
        let dir = crate::tmpfile::TempDir::new("oci-test").unwrap();
        oci(&http, FakeCredentials::new())
            .pull(&format!("{registry}/x/feat:1"), None, dir.path())
            .unwrap();
    }
}

#[test]
fn a_refusal_after_the_handshake_says_how_a_login_is_found() {
    let challenge = r#"Bearer realm="https://ghcr.io/token""#;
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                401,
                &[("www-authenticate", challenge)],
                b"",
            )
            .answer(
                &format!("https://ghcr.io/token?scope={SCOPE}"),
                200,
                &[],
                br#"{"token": "A"}"#,
            )
            .answer(
                &format!("{REPO}/manifests/1"),
                401,
                &[("www-authenticate", challenge)],
                b"",
            ),
    );
    let err = format!(
        "{:#}",
        pull(&http, FakeCredentials::new(), None).0.unwrap_err()
    );
    assert!(
        err.contains("docker login") && err.contains("credential helper"),
        "{err}"
    );
}

#[test]
fn a_blob_that_does_not_match_its_digest_leaves_nothing_behind() {
    let l = layer();
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], b"the LAYER"),
    );
    let (result, files) = pull(&http, FakeCredentials::new(), None);
    assert!(format!("{:#}", result.unwrap_err()).contains("expected"));
    assert!(files.is_empty(), "{files:?}");
}

#[test]
fn a_blob_longer_than_declared_is_refused() {
    let l = layer();
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{REPO}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(
                &format!("{REPO}/blobs/{}", l.0),
                200,
                &[],
                b"the layer and more",
            ),
    );
    let (result, files) = pull(&http, FakeCredentials::new(), None);
    assert!(result.is_err());
    assert!(files.is_empty(), "{files:?}");
}

#[test]
fn a_manifest_asked_for_by_digest_must_hash_to_it() {
    let l = layer();
    let real = image(&l.0, l.1.len());
    let want = Digest::of(&real);
    let http = Arc::new(FakeHttp::new().answer(
        &format!("{REPO}/manifests/{want}"),
        200,
        &[],
        &image(&l.0, 999),
    ));
    let dir = crate::tmpfile::TempDir::new("oci-test").unwrap();
    let err = oci(&http, FakeCredentials::new())
        .pull(&format!("ghcr.io/x/feat@{want}"), None, dir.path())
        .unwrap_err();
    assert!(format!("{err:#}").contains("expected"), "{err:#}");
}

#[test]
fn a_missing_artifact_and_a_server_error_are_told_apart() {
    let http = Arc::new(FakeHttp::new().answer(&format!("{REPO}/manifests/1"), 404, &[], b""));
    let err = pull(&http, FakeCredentials::new(), None).0.unwrap_err();
    assert!(
        err.to_string().contains("not found: ghcr.io/x/feat"),
        "{err}"
    );
    let http = Arc::new(FakeHttp::new().answer(&format!("{REPO}/manifests/1"), 501, &[], b""));
    let err = pull(&http, FakeCredentials::new(), None).0.unwrap_err();
    assert!(err.to_string().contains("501"), "{err}");
}

#[test]
fn a_registry_on_this_machine_is_spoken_to_without_tls() {
    let l = layer();
    let base = "http://localhost:5000/v2/x/feat";
    let http = Arc::new(
        FakeHttp::new()
            .answer(
                &format!("{base}/manifests/1"),
                200,
                &[],
                &image(&l.0, l.1.len()),
            )
            .answer(&format!("{base}/blobs/{}", l.0), 200, &[], &l.1),
    );
    let dir = crate::tmpfile::TempDir::new("oci-test").unwrap();
    oci(&http, FakeCredentials::new())
        .pull("localhost:5000/x/feat:1", None, dir.path())
        .unwrap();
}

#[test]
fn an_unusable_reference_never_reaches_the_network() {
    let http = Arc::new(FakeHttp::new());
    let dir = crate::tmpfile::TempDir::new("oci-test").unwrap();
    assert!(
        oci(&http, FakeCredentials::new())
            .pull("ghcr.io/a/../b:1", None, dir.path())
            .is_err()
    );
    assert!(http.requests.lock().unwrap().is_empty());
}

/// Against the real thing: `cargo test live_ -- --ignored`. Needs the network.
#[test]
#[ignore = "needs the network"]
fn live_a_public_feature_is_pulled_from_ghcr_anonymously() {
    let oci = NativeOci::new(
        Box::new(UreqHttp::new()),
        Box::new(DockerConfig::new(Arc::new(
            crate::local::fake::FakeLocal::new(),
        ))),
    );
    let dir = crate::tmpfile::TempDir::new("oci-live").unwrap();
    oci.pull(
        "ghcr.io/devcontainers/features/common-utils:2",
        Some(Arch::X86_64),
        dir.path(),
    )
    .unwrap();
    let layers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(layers.len(), 1);
    let path = layers[0].as_ref().unwrap().path();
    let mut archive = tar::Archive::new(std::fs::File::open(path).unwrap());
    let names: Vec<String> = archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n.ends_with("install.sh")), "{names:?}");
}

#[test]
fn only_a_network_that_fails_is_marked_unreachable_not_a_registry_that_answers() {
    use crate::tools::Unreachable;
    let url = format!("{REPO}/manifests/1");
    let pin = |http: FakeHttp| {
        oci(&Arc::new(http), FakeCredentials::new())
            .pin("ghcr.io/x/feat:1", None)
            .unwrap_err()
    };
    let down = pin(FakeHttp::new().break_on(http::Method::Get, &url));
    assert!(down.downcast_ref::<Unreachable>().is_some(), "{down:#}");
    // It said no, or had nothing, or failed: all answers, so all the registry's own.
    for status in [401, 403, 404, 501] {
        let said = pin(FakeHttp::new().answer(&url, status, &[], b""));
        assert!(
            said.downcast_ref::<Unreachable>().is_none(),
            "{status}: {said:#}"
        );
    }
}

#[test]
fn a_tag_is_pinned_to_the_digest_of_its_image() {
    let l = layer();
    let manifest = image(&l.0, l.1.len());
    let http =
        Arc::new(FakeHttp::new().answer(&format!("{REPO}/manifests/1"), 200, &[], &manifest));
    let pinned = oci(&http, FakeCredentials::new())
        .pin("ghcr.io/x/feat:1", None)
        .unwrap();
    assert_eq!(pinned.digest, Digest::of(&manifest).to_string());
    assert_eq!(
        pinned.reference,
        format!("ghcr.io/x/feat@{}", pinned.digest)
    );
    // Nothing but the manifest was asked for.
    assert_eq!(http.requests.lock().unwrap().len(), 1);
}

#[test]
fn an_index_is_pinned_to_the_image_for_the_architecture() {
    let amd = Digest::of(b"amd");
    let index = format!(
        r#"{{"manifests": [
            {{"digest": "{amd}", "size": 1, "platform": {{"architecture": "amd64", "os": "linux"}}}},
            {{"digest": "sha256:{}", "size": 1, "platform": {{"architecture": "arm64", "os": "linux"}}}}
        ]}}"#,
        "0".repeat(64)
    );
    let http = Arc::new(FakeHttp::new().answer(
        &format!("{REPO}/manifests/1"),
        200,
        &[],
        index.as_bytes(),
    ));
    let pinned = oci(&http, FakeCredentials::new())
        .pin("ghcr.io/x/feat:1", Some(Arch::X86_64))
        .unwrap();
    assert_eq!(pinned.digest, amd.to_string());
}

#[test]
fn a_pinned_reference_pulls_what_it_was_pinned_to() {
    let l = layer();
    let manifest = image(&l.0, l.1.len());
    let digest = Digest::of(&manifest);
    let http = Arc::new(
        FakeHttp::new()
            .answer(&format!("{REPO}/manifests/1"), 200, &[], &manifest)
            .answer(&format!("{REPO}/manifests/{digest}"), 200, &[], &manifest)
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], &l.1),
    );
    let fetcher = oci(&http, FakeCredentials::new());
    let pinned = fetcher.pin("ghcr.io/x/feat:1", None).unwrap();
    let dir = crate::tmpfile::TempDir::new("oci-test").unwrap();
    fetcher.pull(&pinned.reference, None, dir.path()).unwrap();
    assert!(dir.path().join(l.0.file_name()).is_file());
}

// ---- pushing ----

mod push {
    use super::*;
    use crate::tools::{Artifact, OciPusher, Replace};
    use http::Method;

    const UPLOADS: &str = "https://ghcr.io/v2/x/feat/blobs/uploads/";
    const TYPE: &str = "application/vnd.test.artifact";
    const LAYER_TYPE: &str = "application/vnd.test.layer";

    fn config_digest() -> Digest {
        Digest::of(b"{}")
    }

    /// A file holding the layer, and its digest.
    fn layer_file(dir: &crate::tmpfile::TempDir) -> (std::path::PathBuf, Digest) {
        let path = dir.path().join("layer.tar");
        std::fs::write(&path, b"the layer").unwrap();
        (path, Digest::of(b"the layer"))
    }

    fn artifact(path: &std::path::Path) -> Artifact<'_> {
        Artifact {
            artifact_type: TYPE,
            layer_media_type: LAYER_TYPE,
            layer: path,
            arch: Arch::X86_64,
            replace: Replace::Yes,
        }
    }

    /// The image manifest that is written for the layer `layer`.
    fn image_bytes(layer: &Digest) -> Vec<u8> {
        manifest::image(
            TYPE,
            LAYER_TYPE,
            (&config_digest(), 2),
            (layer, b"the layer".len() as u64),
        )
        .unwrap()
    }

    fn image_url(layer: &Digest) -> String {
        format!("{REPO}/manifests/{}", Digest::of(&image_bytes(layer)))
    }

    /// What a registry says once the blobs are there: it takes the image by its
    /// digest and the index under the tag, and holds nothing under the tag yet.
    trait Publishes {
        fn takes_image(self, layer: &Digest) -> Self;
        fn publishes(self, layer: &Digest) -> Self;
    }

    impl Publishes for FakeHttp {
        fn takes_image(self, layer: &Digest) -> Self {
            self.answer_to(Method::Put, &image_url(layer), 201, &[], b"")
        }

        fn publishes(self, layer: &Digest) -> Self {
            self.takes_image(layer)
                .answer(&format!("{REPO}/manifests/1"), 404, &[], b"")
                .answer_to(Method::Put, &format!("{REPO}/manifests/1"), 201, &[], b"")
        }
    }

    /// A registry that has neither blob, and takes both and the manifest.
    fn empty_registry(layer: &Digest) -> FakeHttp {
        let mut http = FakeHttp::new();
        for (n, blob) in [(1, config_digest()), (2, layer.clone())] {
            let location = format!("/v2/x/feat/blobs/uploads/u{n}");
            http = http
                .answer_to(Method::Head, &format!("{REPO}/blobs/{blob}"), 404, &[], b"")
                .answer_to(Method::Post, UPLOADS, 202, &[("Location", &location)], b"")
                .answer_to(
                    Method::Put,
                    &format!(
                        "https://ghcr.io{location}?digest={}",
                        auth_encode(blob.as_str())
                    ),
                    201,
                    &[],
                    b"",
                );
        }
        http.publishes(layer)
    }

    fn auth_encode(s: &str) -> String {
        s.replace(':', "%3A")
    }

    #[test]
    fn blobs_go_up_first_and_the_manifest_last() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let http = Arc::new(empty_registry(&layer));
        let pinned = oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap();

        let asked: Vec<(Method, String)> = http.asked();
        let methods: Vec<Method> = asked.iter().map(|(m, _)| *m).collect();
        assert_eq!(
            methods,
            vec![
                Method::Head,
                Method::Post,
                Method::Put, // the config
                Method::Head,
                Method::Post,
                Method::Put, // the layer
                Method::Put, // the image, by its digest
                Method::Get, // what the tag holds now
                Method::Put, // the index, under the tag
            ]
        );
        // What was sent: the empty config, the layer, and a manifest naming both.
        let put = |url: String| http.body_of(Method::Put, &url, 0).unwrap();
        assert_eq!(
            put(format!(
                "https://ghcr.io/v2/x/feat/blobs/uploads/u1?digest={}",
                auth_encode(config_digest().as_str())
            )),
            b"{}"
        );
        assert_eq!(
            put(format!(
                "https://ghcr.io/v2/x/feat/blobs/uploads/u2?digest={}",
                auth_encode(layer.as_str())
            )),
            b"the layer"
        );
        let manifest = put(format!(
            "{REPO}/manifests/{}",
            Digest::of(&image_bytes(&layer))
        ));
        let text = String::from_utf8(manifest.clone()).unwrap();
        assert!(text.contains(TYPE) && text.contains(LAYER_TYPE), "{text}");
        assert!(text.contains(layer.as_str()), "{text}");
        // The tag holds an index naming that image for this machine.
        let index = put(format!("{REPO}/manifests/1"));
        let text = String::from_utf8(index.clone()).unwrap();
        assert!(text.contains(&Digest::of(&manifest).to_string()), "{text}");
        assert!(text.contains("\"architecture\":\"amd64\""), "{text}");
        // And what comes back is the image, which pulls.
        assert_eq!(pinned.digest, Digest::of(&manifest).to_string());
        assert_eq!(
            pinned.reference,
            format!("ghcr.io/x/feat@{}", pinned.digest)
        );
    }

    #[test]
    fn a_blob_the_registry_already_has_is_not_sent_again() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    200,
                    &[],
                    b"",
                )
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{layer}"),
                    200,
                    &[],
                    b"",
                )
                .publishes(&layer),
        );
        oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap();
        assert!(http.asked().iter().all(|(m, _)| *m != Method::Post));
    }

    #[test]
    fn a_push_asks_for_the_right_to_write_whatever_the_challenge_names() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        // The registry only asks for a pull on the first look at a blob.
        let challenge = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:x/feat:pull""#;
        let token =
            "https://ghcr.io/token?service=ghcr.io&scope=repository%3Ax%2Ffeat%3Apull%2Cpush";
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    401,
                    &[("www-authenticate", challenge)],
                    b"",
                )
                .answer(token, 200, &[], br#"{"token": "W"}"#)
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    200,
                    &[],
                    b"",
                )
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{layer}"),
                    200,
                    &[],
                    b"",
                )
                .publishes(&layer),
        );
        oci(&http, FakeCredentials::new().with("ghcr.io", "kai", "pw"))
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap();
        assert!(
            http.sent(token, 0, "authorization")
                .unwrap()
                .starts_with("Basic ")
        );
        assert_eq!(
            http.sent(&format!("{REPO}/manifests/1"), 0, "authorization")
                .as_deref(),
            Some("Bearer W")
        );
    }

    #[test]
    fn a_refused_write_says_how_a_login_is_found() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, _) = layer_file(&dir);
        let blob = format!("{REPO}/blobs/{}", config_digest());
        let http = Arc::new(FakeHttp::new().answer_to(Method::Head, &blob, 403, &[], b""));
        let err = format!(
            "{:#}",
            oci(&http, FakeCredentials::new())
                .push("ghcr.io/x/feat:1", &artifact(&path))
                .unwrap_err()
        );
        assert!(
            err.contains("refused to write x/feat") && err.contains("docker login ghcr.io"),
            "{err}"
        );
    }

    #[test]
    fn a_registry_that_stored_something_else_is_an_error() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    200,
                    &[],
                    b"",
                )
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{layer}"),
                    200,
                    &[],
                    b"",
                )
                .takes_image(&layer)
                .answer(&format!("{REPO}/manifests/1"), 404, &[], b"")
                .answer_to(
                    Method::Put,
                    &format!("{REPO}/manifests/1"),
                    201,
                    &[(
                        "Docker-Content-Digest",
                        &format!("sha256:{}", "f".repeat(64)),
                    )],
                    b"",
                ),
        );
        let err = format!(
            "{:#}",
            oci(&http, FakeCredentials::new())
                .push("ghcr.io/x/feat:1", &artifact(&path))
                .unwrap_err()
        );
        assert!(
            err.contains("stored") && err.contains("publishing ghcr.io/x/feat:1"),
            "{err}"
        );
    }

    #[test]
    fn uploads_are_not_sent_to_another_host() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, _) = layer_file(&dir);
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    404,
                    &[],
                    b"",
                )
                .answer_to(
                    Method::Post,
                    UPLOADS,
                    202,
                    &[("Location", "https://evil.example/u/1")],
                    b"",
                ),
        );
        let err = format!(
            "{:#}",
            oci(&http, FakeCredentials::new())
                .push("ghcr.io/x/feat:1", &artifact(&path))
                .unwrap_err()
        );
        assert!(err.contains("not ghcr.io"), "{err}");
        // Nothing was sent there.
        assert!(http.asked().iter().all(|(_, url)| !url.contains("evil")));
    }

    /// A registry that has the layer and takes the empty config at `location`,
    /// which is where the upload is sent; `put_url` is where it must arrive.
    fn config_uploaded_to(location: &str, put_url: &str, layer: &Digest) -> FakeHttp {
        FakeHttp::new()
            .answer_to(
                Method::Head,
                &format!("{REPO}/blobs/{}", config_digest()),
                404,
                &[],
                b"",
            )
            .answer_to(Method::Post, UPLOADS, 202, &[("Location", location)], b"")
            .answer_to(
                Method::Put,
                &format!("{put_url}?digest={}", auth_encode(config_digest().as_str())),
                201,
                &[],
                b"",
            )
            .answer_to(
                Method::Head,
                &format!("{REPO}/blobs/{layer}"),
                200,
                &[],
                b"",
            )
            .publishes(layer)
    }

    #[test]
    fn an_upload_location_may_be_relative_or_name_another_scheme_for_the_same_host() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let put_url = "https://ghcr.io/v2/x/feat/blobs/uploads/u1";
        for location in [
            "/v2/x/feat/blobs/uploads/u1",
            "u1",
            // A proxy that terminates TLS may write its own scheme in the answer;
            // the upload is still sent the way lodge chose.
            "http://ghcr.io/v2/x/feat/blobs/uploads/u1",
            "https://ghcr.io/v2/x/feat/blobs/uploads/u1",
        ] {
            let http = Arc::new(config_uploaded_to(location, put_url, &layer));
            oci(&http, FakeCredentials::new())
                .push("ghcr.io/x/feat:1", &artifact(&path))
                .unwrap_or_else(|e| panic!("{location}: {e:#}"));
        }
    }

    #[test]
    fn an_upload_location_on_another_host_or_port_is_refused() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        for location in [
            "https://ghcr.io.evil.example/v2/x/feat/blobs/uploads/u1",
            "https://ghcr.io:444/v2/x/feat/blobs/uploads/u1",
            "https://evil.example/ghcr.io/u1",
        ] {
            let http = Arc::new(config_uploaded_to(location, "unused", &layer));
            let err = format!(
                "{:#}",
                oci(&http, FakeCredentials::new())
                    .push("ghcr.io/x/feat:1", &artifact(&path))
                    .unwrap_err()
            );
            assert!(err.contains("not ghcr.io"), "{location}: {err}");
            assert!(
                http.asked().iter().all(|(m, _)| *m != Method::Put),
                "{location}"
            );
        }
    }

    #[test]
    fn an_upload_the_registry_hangs_up_on_is_tried_once_more_with_a_new_handshake() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let challenge = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#;
        let token =
            "https://ghcr.io/token?service=ghcr.io&scope=repository%3Ax%2Ffeat%3Apull%2Cpush";
        let config = format!("{REPO}/blobs/{}", config_digest());
        let put = |n: u8| {
            format!(
                "https://ghcr.io/v2/x/feat/blobs/uploads/u{n}?digest={}",
                auth_encode(config_digest().as_str())
            )
        };
        let http = Arc::new(
            FakeHttp::new()
                // The first try: the connection breaks during the upload.
                .answer_to(Method::Head, &config, 404, &[], b"")
                .answer_to(
                    Method::Post,
                    UPLOADS,
                    202,
                    &[("Location", "/v2/x/feat/blobs/uploads/u1")],
                    b"",
                )
                .break_on(Method::Put, &put(1))
                // The second: a new handshake first, since the token may be why.
                .answer_to(
                    Method::Head,
                    &config,
                    401,
                    &[("www-authenticate", challenge)],
                    b"",
                )
                .answer(token, 200, &[], br#"{"token": "FRESH"}"#)
                .answer_to(Method::Head, &config, 404, &[], b"")
                .answer_to(
                    Method::Post,
                    UPLOADS,
                    202,
                    &[("Location", "/v2/x/feat/blobs/uploads/u2")],
                    b"",
                )
                .answer_to(Method::Put, &put(2), 201, &[], b"")
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{layer}"),
                    200,
                    &[],
                    b"",
                )
                .publishes(&layer),
        );
        oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap();
        assert_eq!(
            http.sent(&put(2), 0, "authorization").as_deref(),
            Some("Bearer FRESH")
        );
    }

    #[test]
    fn a_second_hang_up_is_an_error() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, _) = layer_file(&dir);
        let config = format!("{REPO}/blobs/{}", config_digest());
        let put = format!(
            "https://ghcr.io/v2/x/feat/blobs/uploads/u1?digest={}",
            auth_encode(config_digest().as_str())
        );
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(Method::Head, &config, 404, &[], b"")
                .answer_to(Method::Head, &config, 404, &[], b"")
                .answer_to(
                    Method::Post,
                    UPLOADS,
                    202,
                    &[("Location", "/v2/x/feat/blobs/uploads/u1")],
                    b"",
                )
                .answer_to(
                    Method::Post,
                    UPLOADS,
                    202,
                    &[("Location", "/v2/x/feat/blobs/uploads/u1")],
                    b"",
                )
                .break_on(Method::Put, &put)
                .break_on(Method::Put, &put),
        );
        let err = oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap_err();
        assert!(format!("{err:#}").contains("connection broke"), "{err:#}");
        // Once more, and no more.
        let puts = http
            .asked()
            .iter()
            .filter(|(m, _)| *m == Method::Put)
            .count();
        assert_eq!(puts, 2);
    }

    #[test]
    fn an_architecture_published_before_stays_in_the_index() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let before =
            manifest::merged_index(None, Arch::Aarch64, TYPE, (&Digest::of(b"arm image"), 9))
                .unwrap();
        let tag = format!("{REPO}/manifests/1");
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    200,
                    &[],
                    b"",
                )
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{layer}"),
                    200,
                    &[],
                    b"",
                )
                .takes_image(&layer)
                .answer(&tag, 200, &[], &before)
                .answer_to(Method::Put, &tag, 201, &[], b""),
        );
        oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap();
        let index = http.body_of(Method::Put, &tag, 0).unwrap();
        let Manifest::Index(entries) = Manifest::parse(&index).unwrap() else {
            panic!("not an index");
        };
        let found: Vec<(String, String)> = entries
            .into_iter()
            .map(|d| (d.platform.unwrap().architecture, d.digest))
            .collect();
        assert_eq!(
            found,
            vec![
                ("arm64".to_string(), Digest::of(b"arm image").to_string()),
                (
                    "amd64".to_string(),
                    Digest::of(&image_bytes(&layer)).to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_tag_that_cannot_be_read_is_not_overwritten() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let tag = format!("{REPO}/manifests/1");
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    200,
                    &[],
                    b"",
                )
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{layer}"),
                    200,
                    &[],
                    b"",
                )
                .takes_image(&layer)
                // A read is tried again after a 500, and then the answer stands.
                .answer(&tag, 500, &[], b"")
                .answer(&tag, 500, &[], b"")
                .answer(&tag, 500, &[], b"")
                .answer(&tag, 500, &[], b""),
        );
        let err = oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap_err();
        assert!(format!("{err:#}").contains("answered 500"), "{err:#}");
        assert!(http.body_of(Method::Put, &tag, 0).is_none());
    }

    #[test]
    fn a_method_is_named_as_it_is_written_on_the_wire() {
        for (method, name) in [
            (Method::Get, "GET"),
            (Method::Head, "HEAD"),
            (Method::Post, "POST"),
            (Method::Put, "PUT"),
            (Method::Patch, "PATCH"),
        ] {
            assert_eq!(method.name(), name);
        }
    }

    #[test]
    fn a_digest_is_not_a_place_to_publish() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, _) = layer_file(&dir);
        let http = Arc::new(FakeHttp::new());
        let err = oci(&http, FakeCredentials::new())
            .push(
                &format!("ghcr.io/x/feat@{}", Digest::of(b"x")),
                &artifact(&path),
            )
            .unwrap_err();
        assert!(err.to_string().contains("names a digest"), "{err}");
        assert!(http.asked().is_empty());
    }

    #[test]
    fn a_blob_refused_in_one_piece_is_sent_in_pieces() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let blob = format!("{REPO}/blobs/{layer}");
        let location = |n: &str| format!("/v2/x/feat/blobs/uploads/{n}");
        let at = |n: &str, digest: &Digest| {
            format!(
                "https://ghcr.io{}?digest={}",
                location(n),
                auth_encode(digest.as_str())
            )
        };
        let config = config_digest();
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{config}"),
                    200,
                    &[],
                    b"",
                )
                .answer_to(Method::Head, &blob, 404, &[], b"")
                .answer_to(
                    Method::Post,
                    UPLOADS,
                    202,
                    &[("Location", &location("a"))],
                    b"",
                )
                .answer_to(Method::Put, &at("a", &layer), 413, &[], b"")
                .answer_to(
                    Method::Post,
                    UPLOADS,
                    202,
                    &[("Location", &location("b"))],
                    b"",
                )
                .answer_to(
                    Method::Patch,
                    &format!("https://ghcr.io{}", location("b")),
                    202,
                    &[("Location", &location("c"))],
                    b"",
                )
                .answer_to(Method::Put, &at("c", &layer), 201, &[], b"")
                .publishes(&layer),
        );
        oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap();
        let piece = format!("https://ghcr.io{}", location("b"));
        assert_eq!(
            http.body_of(Method::Patch, &piece, 0).unwrap(),
            b"the layer"
        );
        assert_eq!(http.sent(&piece, 0, "content-range").unwrap(), "0-8");
        // The closing request carries no bytes: they went in the piece.
        assert_eq!(http.body_of(Method::Put, &at("c", &layer), 0).unwrap(), b"");
    }

    #[test]
    fn a_request_answered_with_not_now_is_tried_again() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let blob = format!("{REPO}/blobs/{layer}");
        let config = format!("{REPO}/blobs/{}", config_digest());
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(Method::Head, &config, 429, &[("Retry-After", "0")], b"")
                .answer_to(Method::Head, &config, 503, &[], b"")
                .answer_to(Method::Head, &config, 200, &[], b"")
                .answer_to(Method::Head, &blob, 200, &[], b"")
                .publishes(&layer),
        );
        oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap();
        let heads = http
            .asked()
            .iter()
            .filter(|(m, u)| *m == Method::Head && *u == config)
            .count();
        assert_eq!(heads, 3);
    }

    #[test]
    fn a_registry_that_stays_busy_is_given_up_on_after_a_few_tries() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, _) = layer_file(&dir);
        let config = format!("{REPO}/blobs/{}", config_digest());
        let mut http = FakeHttp::new();
        for _ in 0..4 {
            http = http.answer_to(Method::Head, &config, 503, &[], b"");
        }
        let http = Arc::new(http);
        let err = oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap_err();
        assert!(format!("{err:#}").contains("answered 503"), "{err:#}");
        assert_eq!(http.asked().len(), 4);
    }

    #[test]
    fn a_wait_the_registry_asks_for_that_is_too_long_is_not_obeyed() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, _) = layer_file(&dir);
        let config = format!("{REPO}/blobs/{}", config_digest());
        let http = Arc::new(FakeHttp::new().answer_to(
            Method::Head,
            &config,
            429,
            &[("Retry-After", "3600")],
            b"",
        ));
        let err = oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap_err();
        assert!(format!("{err:#}").contains("answered 429"), "{err:#}");
        assert_eq!(http.asked().len(), 1);
    }

    #[test]
    fn a_write_that_failed_with_a_plain_500_is_not_sent_again() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let http = Arc::new(
            FakeHttp::new()
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{}", config_digest()),
                    200,
                    &[],
                    b"",
                )
                .answer_to(
                    Method::Head,
                    &format!("{REPO}/blobs/{layer}"),
                    200,
                    &[],
                    b"",
                )
                .answer_to(Method::Put, &image_url(&layer), 500, &[], b""),
        );
        let err = oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &artifact(&path))
            .unwrap_err();
        assert!(format!("{err:#}").contains("answered 500"), "{err:#}");
        let puts = http
            .asked()
            .iter()
            .filter(|(m, _)| *m == Method::Put)
            .count();
        assert_eq!(puts, 1);
    }

    #[test]
    fn publishing_that_must_not_replace_refuses_before_anything_is_sent() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let image = image_bytes(&layer);
        let held = Digest::of(&image);
        let index = format!(
            r#"{{"manifests": [{{"digest": "{held}", "size": {}, "platform": {{"architecture": "amd64", "os": "linux"}}}}]}}"#,
            image.len()
        );
        let tag = format!("{REPO}/manifests/1");
        let http = Arc::new(FakeHttp::new().answer(&tag, 200, &[], index.as_bytes()));
        let mut keep = artifact(&path);
        keep.replace = Replace::No;
        let err = oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &keep)
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("already holds an image for x86_64") && text.contains(&held.to_string()),
            "{text}"
        );
        // Only the tag was looked at: nothing was uploaded or written.
        assert_eq!(http.asked(), vec![(Method::Get, tag)]);
    }

    #[test]
    fn publishing_that_must_not_replace_goes_ahead_when_nothing_is_there_for_this_machine() {
        let dir = crate::tmpfile::TempDir::new("push").unwrap();
        let (path, layer) = layer_file(&dir);
        let arm = format!(
            r#"{{"manifests": [{{"digest": "sha256:{}", "size": 1, "platform": {{"architecture": "arm64", "os": "linux"}}}}]}}"#,
            "a".repeat(64)
        );
        let tag = format!("{REPO}/manifests/1");
        let mut keep = artifact(&path);
        keep.replace = Replace::No;
        // The tag is read once to decide, and once more to merge into.
        let blob = |d: &Digest| format!("{REPO}/blobs/{d}");
        let http = Arc::new(
            FakeHttp::new()
                .answer(&tag, 200, &[], arm.as_bytes())
                .answer_to(Method::Head, &blob(&config_digest()), 200, &[], b"")
                .answer_to(Method::Head, &blob(&layer), 200, &[], b"")
                .takes_image(&layer)
                .answer(&tag, 200, &[], arm.as_bytes())
                .answer_to(Method::Put, &tag, 201, &[], b""),
        );
        oci(&http, FakeCredentials::new())
            .push("ghcr.io/x/feat:1", &keep)
            .unwrap();
        assert!(http.body_of(Method::Put, &tag, 0).is_some());
    }
}

#[test]
fn a_registry_that_serves_only_the_older_docker_types_is_pulled_from() {
    let l = layer();
    let amd = format!(
        r#"{{"schemaVersion": 2, "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "layers": [{{"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "digest": "{}", "size": {}}}]}}"#,
        l.0,
        l.1.len()
    )
    .into_bytes();
    let list = format!(
        r#"{{"schemaVersion": 2, "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": [{{"mediaType": "application/vnd.docker.distribution.manifest.v2+json",
              "digest": "{}", "size": {}, "platform": {{"architecture": "amd64", "os": "linux"}}}}]}}"#,
        Digest::of(&amd),
        amd.len()
    )
    .into_bytes();
    let http = Arc::new(
        FakeHttp::new()
            .answer(&format!("{REPO}/manifests/1"), 200, &[], &list)
            .answer(
                &format!("{REPO}/manifests/{}", Digest::of(&amd)),
                200,
                &[],
                &amd,
            )
            .answer(&format!("{REPO}/blobs/{}", l.0), 200, &[], &l.1),
    );
    let (result, left) = pull(&http, FakeCredentials::new(), Some(Arch::X86_64));
    result.unwrap();
    assert_eq!(left, vec![l.0.file_name()]);
}
