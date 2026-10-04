//! Release sources on every forge: GitHub as 0.4.x followed it, GitLab and
//! Forgejo. A local server stands in for each forge's API and downloads;
//! no test in here ever talks to a real host.

mod common;

use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use appimg_core::desktop_entry::{DesktopEntry, KEY_RELEASE, KEY_UPDATE_SOURCE};
use appimg_core::digest::{self, Verified};
use appimg_core::install::InstallRequest;
use appimg_core::list::InstalledApp;
use appimg_core::metadata::Reading;
use appimg_core::update::{self, Repo, UpdateSource};
use appimg_core::{download, install, list, metadata, Error};

use common::{read, with_unsquashfs_stand_in, FakeAppImage, Sandbox};

/// How the server answers one path.
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Whether a request with a `Range` gets a 200 and no body, the way
    /// the CDN of dl.librewolf.net answers one for its checksum files.
    breaks_on_range: bool,
}

impl Reply {
    fn json(body: &str) -> Self {
        Self::file(body.as_bytes().to_vec())
    }

    fn file(body: Vec<u8>) -> Self {
        Self { status: 200, headers: Vec::new(), body, breaks_on_range: false }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// One request the server saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    /// The path, query included, exactly as it arrived.
    path: String,
    accept: Option<String>,
    authorization: Option<String>,
}

/// A forge: its API and its downloads on one local server, each path
/// answered the way a test says.
struct Forge {
    address: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Forge {
    fn start() -> Self {
        let port =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let server = tiny_http::Server::http(address).unwrap();
        let routes: Arc<Mutex<HashMap<String, Reply>>> = Arc::default();
        let routed = Arc::clone(&routes);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let (stop, stopped) = channel();

        let handle = thread::spawn(move || loop {
            match server.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(Some(request)) => {
                    let path = request.url().to_string();
                    recorded.lock().unwrap().push(Seen {
                        path: path.clone(),
                        accept: header(&request, "Accept"),
                        authorization: header(&request, "Authorization"),
                    });
                    let route = path.split('?').next().unwrap_or(&path).to_string();
                    let reply = routed.lock().unwrap().get(&route).cloned();
                    let mut reply = reply.unwrap_or(Reply {
                        status: 404,
                        headers: Vec::new(),
                        body: b"{\"message\":\"404 Not found\"}".to_vec(),
                        breaks_on_range: false,
                    });
                    if reply.breaks_on_range && header(&request, "Range").is_some() {
                        reply.body.clear();
                    }
                    let headers = reply
                        .headers
                        .iter()
                        .map(|(name, value)| {
                            tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
                                .unwrap()
                        })
                        .collect();
                    let length = reply.body.len();
                    let _ = request.respond(tiny_http::Response::new(
                        tiny_http::StatusCode(reply.status),
                        headers,
                        Cursor::new(reply.body),
                        Some(length),
                        None,
                    ));
                }
                Ok(None) => {
                    if stopped.try_recv().is_ok() {
                        return;
                    }
                }
                Err(_) => return,
            }
        });
        Self { address, routes, seen, stop, handle: Some(handle) }
    }

    /// Where it is, `http://127.0.0.1:<port>`, which a self-hosted source
    /// names and a stand-in for a public forge's API takes.
    fn base(&self) -> String {
        format!("http://{}", self.address)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base())
    }

    fn route(&self, path: &str, reply: Reply) {
        self.routes.lock().unwrap().insert(path.to_string(), reply);
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The paths asked for since the first `since` requests.
    fn paths_since(&self, since: usize) -> Vec<String> {
        self.seen()[since..].iter().map(|seen| seen.path.clone()).collect()
    }
}

impl Drop for Forge {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn header(request: &tiny_http::Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

/// Runs something with `variable` set to `value`. The tests run one at a
/// time, which is what makes this safe.
fn with_env<T>(variable: &str, value: &str, run: impl FnOnce() -> T) -> T {
    let previous = std::env::var_os(variable);
    std::env::set_var(variable, value);
    let result = run();
    match previous {
        Some(previous) => std::env::set_var(variable, previous),
        None => std::env::remove_var(variable),
    }
    result
}

/// An AppImage that passes the checks an update holds a download to, told
/// apart from the others by `marker`.
fn build(sandbox: &Sandbox, marker: &str) -> Vec<u8> {
    let file = FakeAppImage::new("Forge App")
        .marker(marker)
        .elf()
        .build(&sandbox.root, &format!("{marker}.AppImage"));
    std::fs::read(file).unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), bytes).unwrap();
    digest::sha256_file(file.path()).unwrap()
}

/// Installs `bytes` from the local file `name` at `version`, updating from
/// `source`, with `release` recorded as the one it came out of.
fn install(
    sandbox: &Sandbox,
    name: &str,
    bytes: &[u8],
    version: &str,
    source: &str,
    release: Option<&str>,
) -> InstalledApp {
    let file = sandbox.downloads.join(name);
    std::fs::write(&file, bytes).unwrap();
    with_unsquashfs_stand_in(sandbox, || {
        let info = metadata::inspect(&file, None, Reading::MayRun).unwrap();
        let mut request = InstallRequest::from_info(&file, &file.to_string_lossy(), &info);
        request.version = Some(version.to_string());
        request.update_source = Some(update::parse_update_source(source).unwrap());
        request.release = release.map(str::to_string);
        install::install(&sandbox.paths, &request).unwrap();
    });
    list::find(&sandbox.paths, "forge-app").unwrap()
}

fn entry(app: &InstalledApp) -> DesktopEntry {
    DesktopEntry::read(&app.desktop_entry_path).unwrap()
}

fn update(sandbox: &Sandbox, app: &InstalledApp) -> appimg_core::Result<update::UpdateOutcome> {
    with_unsquashfs_stand_in(sandbox, || update::update(&sandbox.paths, app, None))
}

/// A release as the GitHub API returns one.
fn github_release(tag: &str, assets: &[(&str, Option<&str>)]) -> String {
    let assets: Vec<String> = assets
        .iter()
        .map(|(url, digest)| {
            let name = url.rsplit('/').next().unwrap();
            let digest = digest.map_or("null".to_string(), |d| format!("\"sha256:{d}\""));
            format!(
                "{{\"url\":\"https://api.github.com/assets/1\",\"name\":\"{name}\",\
                 \"uploader\":{{\"login\":\"bot\"}},\"size\":1,\"digest\":{digest},\
                 \"browser_download_url\":\"{url}\"}}"
            )
        })
        .collect();
    format!(
        "{{\"tag_name\":\"{tag}\",\"target_commitish\":\"main\",\"draft\":false,\
         \"prerelease\":false,\"published_at\":\"2026-10-01T10:00:00Z\",\"assets\":[{}],\
         \"body\":\"notes\"}}",
        assets.join(",")
    )
}

/// What 0.4.x wrote stays what it was. Every `github:` value reads back
/// unchanged, `@tag` and `#pattern` included, a recorded `X-AppImg-Release`
/// still says which release the installed file came out of, the API is
/// asked at the same addresses with the same `Accept`, and an update
/// records the release in the same spelling.
#[test]
fn github_sources_written_by_0_4_read_back_and_behave_exactly_as_before() {
    let _serial = common::serial();
    for stored in [
        "github:o/r",
        "github:Some-Owner/Repo.Name_2",
        "github:o/r@continuous",
        "github:o/r@v1.0.0",
        "github:o/r#Forge_App-*-x86_64.AppImage",
        "github:o/r@nightly#Forge_App-*.AppImage",
    ] {
        assert_eq!(update::parse_update_source(stored).unwrap(), stored);
        let (repo, tag, pattern) = Repo::parse_source(stored).unwrap();
        assert_eq!(
            repo,
            Repo::github(
                repo.path.split('/').next().unwrap(),
                repo.path.split('/').nth(1).unwrap()
            )
        );
        assert_eq!(repo.setting(tag.as_deref(), pattern.as_deref()), stored);
    }

    let sandbox = Sandbox::new();
    let github = Forge::start();
    let v1 = build(&sandbox, "v1");
    let v2 = build(&sandbox, "v2");
    let asset = |tag: &str, name: &str| github.url(&format!("/download/{tag}/{name}"));
    let (v1_url, v2_url) = (
        asset("v1.0.0", "Forge_App-1.0.0-x86_64.AppImage"),
        asset("v2.0.0", "Forge_App-2.0.0-x86_64.AppImage"),
    );
    let other = asset("v2.0.0", "Forge_App-2.0.0-aarch64.AppImage");

    // An entry as 0.4.x left it: a pattern with the source, and the release
    // the installed file came out of.
    let app = install(
        &sandbox,
        "Forge_App-1.0.0-x86_64.AppImage",
        &v1,
        "1.0.0",
        "github:o/r#Forge_App-*-x86_64.AppImage",
        Some("github:o/r@v1.0.0"),
    );
    assert_eq!(entry(&app).get(KEY_UPDATE_SOURCE), Some("github:o/r#Forge_App-*-x86_64.AppImage"));
    assert_eq!(entry(&app).get(KEY_RELEASE), Some("github:o/r@v1.0.0"));
    let source = update::source_for(&app);
    assert_eq!(
        source,
        UpdateSource::ForgeRelease {
            repo: Repo::github("o", "r"),
            tag: None,
            asset: Some("Forge_App-1.0.0-x86_64.AppImage".to_string()),
            pattern: Some("Forge_App-*-x86_64.AppImage".to_string()),
        }
    );
    assert_eq!(source.describe(), "github:o/r#Forge_App-*-x86_64.AppImage");

    let listing = |releases: &[String]| Reply::json(&format!("[{}]", releases.join(",")));
    github.route("/repos/o/r/releases", listing(&[github_release("v1.0.0", &[(&v1_url, None)])]));
    with_env("APPIMG_GITHUB_API", &github.base(), || {
        // The recorded release is the one offered: up to date.
        let status = update::check(&app).unwrap();
        assert!(status.nothing_to_do(), "{status:?}");
        assert_eq!(
            github.seen(),
            vec![Seen {
                path: "/repos/o/r/releases?per_page=30".to_string(),
                accept: Some("application/vnd.github+json".to_string()),
                authorization: None,
            }]
        );

        github.route(
            "/repos/o/r/releases",
            listing(&[
                github_release("v2.0.0", &[(&other, None), (&v2_url, Some(&sha256(&v2)))]),
                github_release("v1.0.0", &[(&v1_url, None)]),
            ]),
        );
        github.route("/download/v2.0.0/Forge_App-2.0.0-x86_64.AppImage", Reply::file(v2.clone()));
        let status = update::check(&app).unwrap();
        assert!(status.available, "{status:?}");
        assert_eq!(status.latest_version.as_deref(), Some("2.0.0"));

        let outcome = update(&sandbox, &app).unwrap();
        assert_eq!(outcome.digest, Some(Verified::Matches(sha256(&v2))));
        update::confirm(&sandbox.paths, "forge-app").unwrap();
        let app = list::find(&sandbox.paths, "forge-app").unwrap();
        assert_eq!(entry(&app).get(KEY_RELEASE), Some("github:o/r@v2.0.0"));
        assert_eq!(
            entry(&app).get(KEY_UPDATE_SOURCE),
            Some("github:o/r#Forge_App-*-x86_64.AppImage")
        );
        assert!(update::check(&app).unwrap().nothing_to_do());

        // A tag is followed at the address 0.4.x asked.
        update::set_update_source(&app, Some("github:o/r@continuous")).unwrap();
        let app = list::find(&sandbox.paths, "forge-app").unwrap();
        let before = github.seen().len();
        let _ = update::check(&app);
        assert_eq!(github.paths_since(before), ["/repos/o/r/releases/tags/continuous"]);
    });
}

/// A GitLab project with subgroups on a host of its own. GitLab marks no
/// pre-release, so the tag says; its assets are links whose name is what
/// the author typed; and the checksum comes from the package registry of
/// the project, at update time only.
#[test]
fn a_gitlab_project_is_followed_and_its_package_registry_checks_the_file() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let gitlab = Forge::start();
    let source = format!("gitlab:{}/group/sub/project", gitlab.base());
    let v1 = build(&sandbox, "v1");
    let v2 = build(&sandbox, "v2");
    let app = install(&sandbox, "Forge_App-1.0-x86_64.AppImage", &v1, "1.0", &source, None);
    assert_eq!(app.update_source.as_deref(), Some(source.as_str()));

    let package = |file: &str| {
        gitlab.url(&format!("/api/v4/projects/7/packages/generic/forge-app/2.0/{file}"))
    };
    let link = |name: &str, url: &str| {
        format!("{{\"id\":1,\"name\":\"{name}\",\"url\":\"{url}\",\"direct_asset_url\":\"{url}\",\"link_type\":\"other\"}}")
    };
    let release = |tag: &str, upcoming: bool, links: &[String]| {
        format!(
            "{{\"name\":\"{tag}\",\"tag_name\":\"{tag}\",\"description\":\"notes\",\
             \"released_at\":\"2026-09-30T09:21:15.488Z\",\"upcoming_release\":{upcoming},\
             \"author\":{{\"id\":1,\"name\":\"Someone\"}},\
             \"commit\":{{\"id\":\"bb03eaa1234567890abcdef\",\"short_id\":\"bb03eaa1\"}},\
             \"assets\":{{\"count\":2,\"sources\":[{{\"format\":\"zip\",\"url\":\"{}/archive.zip\"}}],\
             \"links\":[{}]}},\"_links\":{{\"self\":\"x\"}}}}",
            gitlab.base(),
            links.join(",")
        )
    };
    let listing = format!(
        "[{},{},{}]",
        release(
            "v3.0",
            true,
            &[link("Forge_App-3.0-x86_64.AppImage", &package("Forge_App-3.0-x86_64.AppImage"))]
        ),
        release(
            "v2.1-rc1",
            false,
            &[link("Forge_App-2.1-x86_64.AppImage", &package("Forge_App-2.1-x86_64.AppImage"))]
        ),
        release(
            "v2.0",
            false,
            &[
                link("Forge App 2.0 (x86_64)", &package("Forge_App-2.0-x86_64.AppImage")),
                link("Forge_App-2.0-aarch64.AppImage", &package("Forge_App-2.0-aarch64.AppImage")),
            ]
        ),
    );
    gitlab.route("/api/v4/projects/group%2Fsub%2Fproject/releases", Reply::json(&listing));
    gitlab.route(
        "/api/v4/projects/7/packages/generic/forge-app/2.0/Forge_App-2.0-x86_64.AppImage",
        Reply::file(v2.clone()),
    );
    let registry = |sha256: &str| {
        gitlab.route(
            "/api/v4/projects/7/packages",
            Reply::json("[{\"id\":42,\"name\":\"forge-app\",\"version\":\"2.0\",\"package_type\":\"generic\"}]"),
        );
        gitlab.route(
            "/api/v4/projects/7/packages/42/package_files",
            Reply::json(&format!(
                "[{{\"id\":1,\"file_name\":\"Forge_App-2.0-x86_64.AppImage\",\"file_sha256\":\"{}\"}},\
                  {{\"id\":2,\"file_name\":\"Forge_App-2.0-x86_64.AppImage\",\"file_sha256\":\"{sha256}\"}}]",
                "0".repeat(64)
            )),
        );
    };

    // A check reads the listing and nothing else: not the release in the
    // future, not the release candidate, and no checksum.
    let status = update::check(&app).unwrap();
    assert!(status.available, "{status:?}");
    assert_eq!(status.latest_version.as_deref(), Some("2.0"));
    assert_eq!(
        gitlab.seen(),
        vec![Seen {
            path: "/api/v4/projects/group%2Fsub%2Fproject/releases?per_page=30".to_string(),
            accept: Some("application/json".to_string()),
            authorization: None,
        }]
    );

    // A registry that names another file refuses the download.
    registry(&"f".repeat(64));
    let error = update(&sandbox, &app).unwrap_err();
    assert!(matches!(error, Error::DigestMismatch { .. }), "{error}");
    assert!(read(&app.appimage_path).contains("v1"));

    registry(&sha256(&v2));
    let before = gitlab.seen().len();
    let outcome = update(&sandbox, &app).unwrap();
    assert_eq!(
        outcome.digest,
        Some(Verified::MatchesChecksum {
            sha256: sha256(&v2),
            by: "the GitLab package registry".to_string()
        })
    );
    assert_eq!(
        gitlab.paths_since(before),
        [
            "/api/v4/projects/group%2Fsub%2Fproject/releases?per_page=30".to_string(),
            "/api/v4/projects/7/packages?package_type=generic&package_name=forge-app&package_version=2.0"
                .to_string(),
            "/api/v4/projects/7/packages/42/package_files?per_page=100".to_string(),
            "/api/v4/projects/7/packages/generic/forge-app/2.0/Forge_App-2.0-x86_64.AppImage"
                .to_string(),
        ]
    );
    update::confirm(&sandbox.paths, "forge-app").unwrap();
    let app = list::find(&sandbox.paths, "forge-app").unwrap();
    assert!(read(&app.appimage_path).contains("v2"));
    assert_eq!(entry(&app).get(KEY_RELEASE), Some(format!("{source}@v2.0").as_str()));
    assert!(update::check(&app).unwrap().nothing_to_do());

    // The release candidate is followed when its tag is named.
    update::set_update_source(&app, Some(&format!("{source}@v2.1-rc1"))).unwrap();
    let app = list::find(&sandbox.paths, "forge-app").unwrap();
    gitlab.route(
        "/api/v4/projects/group%2Fsub%2Fproject/releases/v2.1-rc1",
        Reply::json(&release(
            "v2.1-rc1",
            false,
            &[link("Forge_App-2.1-x86_64.AppImage", &package("Forge_App-2.1-x86_64.AppImage"))],
        )),
    );
    let status = update::check(&app).unwrap();
    assert!(status.available, "{status:?}");
    assert_eq!(status.latest_version.as_deref(), Some("2.1"));
}

/// A GitLab package file downloads from `.../download`, a URL that names
/// nothing. The name the server gave the download when it was installed
/// is what picks the file out of each release, not a guess among the
/// builds for this machine.
#[test]
fn a_gitlab_download_that_names_nothing_is_matched_by_the_name_it_arrived_under() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let gitlab = Forge::start();
    let v1 = build(&sandbox, "v1");
    let v2 = build(&sandbox, "v2");
    let steamdeck = build(&sandbox, "steamdeck");
    gitlab.route(
        "/es-de/es-de/-/package_files/1/download",
        Reply::file(v1)
            .header("Content-Disposition", "attachment; filename=\"ES-DE_x64.AppImage\""),
    );
    gitlab.route("/es-de/es-de/-/package_files/2/download", Reply::file(v2));
    gitlab.route("/es-de/es-de/-/package_files/3/download", Reply::file(steamdeck));
    let link = |name: &str, file: u32| {
        format!(
            "{{\"name\":\"{name}\",\"url\":\"{}\",\"link_type\":\"other\"}}",
            gitlab.url(&format!("/es-de/es-de/-/package_files/{file}/download"))
        )
    };
    gitlab.route(
        "/api/v4/projects/es-de%2Fes-de/releases",
        Reply::json(&format!(
            "[{{\"tag_name\":\"v3.5.0\",\"upcoming_release\":false,\"released_at\":\"2026-09-30T09:21:15Z\",\
               \"assets\":{{\"links\":[{},{}]}}}}]",
            link("ES-DE_x64_SteamDeck.AppImage", 3),
            link("ES-DE_x64.AppImage", 2)
        )),
    );

    // Installed the way `appimg install <url>` does it: the server's name
    // for the download is kept.
    let url = gitlab.url("/es-de/es-de/-/package_files/1/download");
    let scratch = tempfile::tempdir().unwrap();
    let file = scratch.path().join("download.AppImage");
    let fetched = download::appimage_or_archive(&url, &file, &|_| Ok(None), None).unwrap();
    assert_eq!(fetched.remote.name.as_deref(), Some("ES-DE_x64.AppImage"));
    with_unsquashfs_stand_in(&sandbox, || {
        let info = metadata::inspect(&file, None, Reading::MayRun).unwrap();
        let mut request = InstallRequest::from_info(&file, &url, &info);
        request.name = "Forge App".to_string();
        request.version = Some("3.4.1".to_string());
        request.remote = Some(fetched.remote.clone());
        request.update_source = Some(format!("gitlab:{}/es-de/es-de", gitlab.base()));
        install::install(&sandbox.paths, &request).unwrap();
    });
    let app = list::find(&sandbox.paths, "forge-app").unwrap();

    let status = update::check(&app).unwrap();
    assert!(status.available, "{status:?}");
    let outcome = update(&sandbox, &app).unwrap();
    assert!(read(&outcome.appimage_path).contains("v2"));
    assert!(!read(&outcome.appimage_path).contains("steamdeck"));
}

/// A Forgejo on a host of its own, and Codeberg: drafts and pre-releases
/// are passed over as on GitHub, a moving tag is followed when named, and
/// a checksum file in the release checks the download.
#[test]
fn a_forgejo_repository_is_followed_and_a_checksum_file_checks_the_file() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let forgejo = Forge::start();
    let source = format!("forgejo:{}/o/r", forgejo.base());
    let v1 = build(&sandbox, "v1");
    let v2 = build(&sandbox, "v2");
    let nightly = build(&sandbox, "nightly");
    let app = install(&sandbox, "Forge_App-1.0-x86_64.AppImage", &v1, "1.0", &source, None);

    let asset = |tag: &str, name: &str| {
        format!(
            "{{\"id\":1,\"name\":\"{name}\",\"size\":1,\"browser_download_url\":\"{}\",\"type\":\"attachment\"}}",
            forgejo.url(&format!("/o/r/releases/download/{tag}/{name}"))
        )
    };
    let release = |tag: &str, draft: bool, prerelease: bool, assets: &[String]| {
        format!(
            "{{\"id\":1,\"tag_name\":\"{tag}\",\"target_commitish\":\"main\",\"draft\":{draft},\
             \"prerelease\":{prerelease},\"published_at\":\"2026-09-28T18:58:00+02:00\",\
             \"author\":{{\"login\":\"o\"}},\"assets\":[{}]}}",
            assets.join(",")
        )
    };
    let v2_assets = [
        asset("v2.0", "Forge_App-2.0-x86_64.AppImage"),
        asset("v2.0", "Forge_App-2.0-x86_64.AppImage.sha256sum"),
    ];
    forgejo.route(
        "/api/v1/repos/o/r/releases",
        Reply::json(&format!(
            "[{},{},{}]",
            release("v3.0", true, false, &[asset("v3.0", "Forge_App-3.0-x86_64.AppImage")]),
            release(
                "nightly",
                false,
                true,
                &[asset("nightly", "Forge_App-2.1-dev-x86_64.AppImage")]
            ),
            release("v2.0", false, false, &v2_assets),
        )),
    );
    forgejo.route(
        "/o/r/releases/download/v2.0/Forge_App-2.0-x86_64.AppImage",
        Reply::file(v2.clone()),
    );
    let checksum_file = |sha256: &str| {
        forgejo.route(
            "/o/r/releases/download/v2.0/Forge_App-2.0-x86_64.AppImage.sha256sum",
            Reply::file(format!("{sha256}  Forge_App-2.0-x86_64.AppImage\n").into_bytes()),
        );
    };

    let status = update::check(&app).unwrap();
    assert!(status.available, "{status:?}");
    assert_eq!(status.latest_version.as_deref(), Some("2.0"));
    assert_eq!(forgejo.paths_since(0), ["/api/v1/repos/o/r/releases?limit=30"]);

    checksum_file(&"e".repeat(64));
    let error = update(&sandbox, &app).unwrap_err();
    assert!(matches!(error, Error::DigestMismatch { .. }), "{error}");
    assert!(read(&app.appimage_path).contains("v1"));

    checksum_file(&sha256(&v2));
    let outcome = update(&sandbox, &app).unwrap();
    assert_eq!(
        outcome.digest,
        Some(Verified::MatchesChecksum {
            sha256: sha256(&v2),
            by: "Forge_App-2.0-x86_64.AppImage.sha256sum".to_string()
        })
    );
    update::confirm(&sandbox.paths, "forge-app").unwrap();
    let app = list::find(&sandbox.paths, "forge-app").unwrap();
    assert_eq!(entry(&app).get(KEY_RELEASE), Some(format!("{source}@v2.0").as_str()));
    assert!(update::check(&app).unwrap().nothing_to_do());

    // The nightly pre-release is followed when its tag is named, and with
    // no checksum anywhere the download is not refused, only not checked.
    update::set_update_source(&app, Some(&format!("{source}@nightly"))).unwrap();
    let app = list::find(&sandbox.paths, "forge-app").unwrap();
    forgejo.route(
        "/api/v1/repos/o/r/releases/tags/nightly",
        Reply::json(&release(
            "nightly",
            false,
            true,
            &[asset("nightly", "Forge_App-2.1-dev-x86_64.AppImage")],
        )),
    );
    forgejo.route(
        "/o/r/releases/download/nightly/Forge_App-2.1-dev-x86_64.AppImage",
        Reply::file(nightly),
    );
    let outcome = update(&sandbox, &app).unwrap();
    assert_eq!(
        outcome.digest,
        Some(Verified::Unchecked("the release publishes no checksum for it".to_string()))
    );
    assert!(read(&outcome.appimage_path).contains("nightly"));
    assert!(forgejo.seen().iter().all(|seen| seen.authorization.is_none()));
}

/// `codeberg:` and `gitlab:` name the public hosts, and ask their APIs at
/// the addresses those have; a local server stands in for each.
#[test]
fn the_public_hosts_are_asked_where_their_apis_are() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let server = Forge::start();
    let v1 = build(&sandbox, "v1");
    let app = install(&sandbox, "Forge_App-1.0-x86_64.AppImage", &v1, "1.0", "codeberg:o/r", None);
    assert_eq!(app.update_source.as_deref(), Some("codeberg:o/r"));
    server.route("/api/v1/repos/o/r/releases", Reply::json("[]"));
    server.route("/api/v4/projects/g%2Fp/releases", Reply::json("[]"));

    with_env("APPIMG_CODEBERG_URL", &server.base(), || {
        let error = update::check(&app).unwrap_err();
        assert!(error.to_string().contains("codeberg:o/r"), "{error}");
    });
    update::set_update_source(&app, Some("gitlab:g/p")).unwrap();
    let app = list::find(&sandbox.paths, "forge-app").unwrap();
    with_env("APPIMG_GITLAB_URL", &server.base(), || {
        let error = update::check(&app).unwrap_err();
        assert!(error.to_string().contains("follow one with @tag"), "{error}");
    });
    assert_eq!(
        server.paths_since(0),
        ["/api/v1/repos/o/r/releases?limit=30", "/api/v4/projects/g%2Fp/releases?per_page=30"]
    );
}

/// LibreWolf on Codeberg: the release links its files on another host,
/// whose checksum files hold the hash and nothing else, and whose CDN
/// answers a ranged request for one with no body at all. The checksum file
/// is asked for whole, and its one hash is the one.
#[test]
fn a_checksum_file_elsewhere_with_a_bare_hash_is_read_whole() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let codeberg = Forge::start();
    let cdn = Forge::start();
    let v1 = build(&sandbox, "v1");
    let v2 = build(&sandbox, "v2");
    let source = format!("forgejo:{}/librewolf/bsys6", codeberg.base());
    let app = install(
        &sandbox,
        "librewolf-156.0.1-1-linux-x86_64-appimage.AppImage",
        &v1,
        "156.0.1-1",
        &source,
        None,
    );
    let name = "librewolf-157.0-1-linux-x86_64-appimage.AppImage";
    let external = |file: &str| {
        format!(
            "{{\"id\":1,\"name\":\"{file}\",\"size\":0,\"browser_download_url\":\"{}\",\"type\":\"external\"}}",
            cdn.url(&format!("/librewolf/157.0-1/{file}"))
        )
    };
    codeberg.route(
        "/api/v1/repos/librewolf/bsys6/releases",
        Reply::json(&format!(
            "[{{\"tag_name\":\"157.0-1\",\"draft\":false,\"prerelease\":false,\
               \"published_at\":\"2026-09-28T18:58:00+02:00\",\"target_commitish\":\"\",\"assets\":[{},{},{}]}}]",
            external(&format!("{name}.sha256sum")),
            external(&format!("{name}.sig")),
            external(name)
        )),
    );
    cdn.route(&format!("/librewolf/157.0-1/{name}"), Reply::file(v2.clone()));
    cdn.route(
        &format!("/librewolf/157.0-1/{name}.sha256sum"),
        Reply { breaks_on_range: true, ..Reply::file(format!("{}\n", sha256(&v2)).into_bytes()) },
    );

    let outcome = update(&sandbox, &app).unwrap();
    assert_eq!(
        outcome.digest,
        Some(Verified::MatchesChecksum { sha256: sha256(&v2), by: format!("{name}.sha256sum") })
    );
    assert!(read(&outcome.appimage_path).contains("v2"));
}
