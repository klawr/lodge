use super::credentials::fake::FakeCredentials;
use super::digest::Digest;
use super::http::fake::FakeHttp;
use super::*;
use crate::tools::OciFetcher;
use std::sync::Arc;

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
    struct Shared(Arc<FakeHttp>);
    impl Http for Shared {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<http::Response> {
            self.0.get(url, headers)
        }
    }
    NativeOci::new(Box::new(Shared(http.clone())), Box::new(creds))
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
    let http = Arc::new(FakeHttp::new().answer(&format!("{REPO}/manifests/1"), 500, &[], b""));
    let err = pull(&http, FakeCredentials::new(), None).0.unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
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
