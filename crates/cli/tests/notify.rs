//! `appimg notify` as a user runs it, and `appimg notify check` as the timer
//! runs it. Stand-ins for `systemctl`, `notify-send` and `gdbus` on `PATH`
//! record every call, and a local HTTP server stands in for the GitHub API.
//! No test in here talks to the real user manager, the real notification
//! server or any real host.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// Executing a file while any process still holds it open for writing fails
/// with `ETXTBSY`, and a `fork` in another test thread can hold one for a
/// moment. The tests run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An HTTP server on a random port that answers each path it was given a
/// body for with a 200 and that body, and anything else with a 404. It
/// stands in for the GitHub API.
struct Server {
    base: String,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let routed = Arc::clone(&routes);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                // The request is read to its end first: a socket closed with
                // unread bytes in it resets the connection.
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    request.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&request).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let route = path.split('?').next().unwrap_or(&path);
                let (status, body) = match routed.lock().unwrap().get(route) {
                    Some(body) => ("200 OK", body.clone()),
                    None => ("404 Not Found", b"not found".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&[head.as_bytes(), &body].concat());
                let _ = stream.shutdown(Shutdown::Write);
            }
        });
        Self { base, routes }
    }

    /// Makes `github:o/{repo}` offer one release, `tag`, with the AppImage
    /// `Github_App-{version}.AppImage` in it.
    fn release(&self, repo: &str, tag: &str) {
        let version = tag.trim_start_matches('v');
        let name = format!("Github_App-{version}.AppImage");
        let body = format!(
            "[{{\"tag_name\":\"{tag}\",\"draft\":false,\"prerelease\":false,\
             \"published_at\":\"2026-10-01T10:00:00Z\",\"assets\":[{{\"name\":\"{name}\",\
             \"size\":1,\"browser_download_url\":\"{}/files/{name}\"}}]}}]",
            self.base
        );
        self.routes.lock().unwrap().insert(format!("/repos/o/{repo}/releases"), body.into_bytes());
    }

    /// Makes `github:o/{repo}` a 404, which fails a check of it.
    fn forget(&self, repo: &str) {
        self.routes.lock().unwrap().remove(&format!("/repos/o/{repo}/releases"));
    }
}

/// Which stand-ins `tools/` holds.
#[derive(Clone, Copy)]
enum Notifier {
    NotifySend,
    Gdbus,
    Neither,
}

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    github_api: String,
}

impl Home {
    fn new(server: &Server, notifier: Notifier) -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["home/Downloads", "data", "config", "tmp", "tools", "show"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let home = Self { _dir: dir, root, github_api: server.base.clone() };

        // The fixtures only start like an AppImage, so they extract through
        // a stand-in for `unsquashfs`, called as `unsquashfs -no-progress -o
        // OFFSET -d ROOT FILE`: it copies the payload the file names.
        home.tool(
            "unsquashfs",
            "payload=$(sed -n 's/^payload=//p' \"$6\")\nmkdir -p \"$5\"\ncp -R \"$payload/.\" \"$5/\"\n",
        );
        // Every stand-in below uses shell builtins only, so it works with
        // nothing but `tools/` on `PATH`. `systemctl` records its arguments
        // on one line, prints what `show/<unit>` holds when asked to show
        // that unit, and fails the way it does without a user manager while
        // `systemctl.fail` exists.
        let root = home.root.display().to_string();
        home.tool(
            "systemctl",
            &format!(
                "printf '%s\\n' \"$*\" >> '{root}/systemctl.log'\n\
                 if [ -f '{root}/systemctl.fail' ]; then\n\
                 \x20 printf 'Failed to connect to bus: No medium found\\n' >&2\n  exit 1\nfi\n\
                 if [ \"$2\" = show ] && [ -f \"{root}/show/$3\" ]; then\n\
                 \x20 while IFS= read -r line; do printf '%s\\n' \"$line\"; done < \"{root}/show/$3\"\n\
                 fi\n"
            ),
        );
        // A notification tool records each argument on a line of its own,
        // and `--end--` after the last.
        let record = |name: &str| {
            format!(
                "for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done >> '{root}/{name}.log'\n\
                 printf '%s\\n' --end-- >> '{root}/{name}.log'\n"
            )
        };
        match notifier {
            Notifier::NotifySend => home.tool("notify-send", &record("notify-send")),
            Notifier::Gdbus => home.tool("gdbus", &record("gdbus")),
            Notifier::Neither => {}
        }
        home
    }

    fn tool(&self, name: &str, script: &str) {
        let path = self.root.join("tools").join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `appimg` with the environment of a user: the stand-ins in front of
    /// the real `PATH`, unless `only_tools` leaves the real one out, and the
    /// home and GitHub API of this test.
    fn command(&self, binary: &Path, only_tools: bool) -> Command {
        let tools = self.root.join("tools").display().to_string();
        let path = if only_tools {
            tools
        } else {
            format!("{tools}:{}", std::env::var("PATH").unwrap_or_default())
        };
        let mut command = Command::new(binary);
        command
            .arg("--no-color")
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("APPIMG_GITHUB_API", &self.github_api)
            .env_remove("APPIMG_DIR")
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_appimg")), false).args(args).output().unwrap()
    }

    /// Runs `appimg` with nothing but the stand-ins on `PATH`.
    fn run_with_tools_only(&self, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_appimg")), true).args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}\n{}\n{}", stdout(&output), stderr(&output));
        output
    }

    /// Runs what the service unit says, the way the user manager would:
    /// its command line, with its environment over that of the user.
    fn run_service(&self) -> Output {
        let unit = self.unit("appimg-notify.service");
        let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart=\"")).unwrap();
        let (binary, args) = exec.split_once("\" ").unwrap();
        let mut command = self.command(Path::new(binary), false);
        for line in unit.lines().filter_map(|l| l.strip_prefix("Environment=")) {
            let (key, value) = line.trim_matches('"').split_once('=').unwrap();
            command.env(key, value);
        }
        command.args(args.split(' ')).output().unwrap()
    }

    fn unit(&self, name: &str) -> String {
        fs::read_to_string(self.unit_path(name)).unwrap()
    }

    fn unit_path(&self, name: &str) -> PathBuf {
        self.root.join("config/systemd/user").join(name)
    }

    /// The lines a stand-in recorded, or none when it never ran.
    fn log(&self, tool: &str) -> Vec<String> {
        fs::read_to_string(self.root.join(format!("{tool}.log")))
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The calls `systemctl` got, apart from the questions `show` asks.
    fn systemctl_calls(&self) -> Vec<String> {
        self.log("systemctl").into_iter().filter(|call| !call.starts_with("--user show ")).collect()
    }

    /// Each notification `notify-send` was asked to show, as its arguments.
    fn notifications(&self) -> Vec<Vec<String>> {
        self.log("notify-send")
            .split(|line| line == "--end--")
            .filter(|call| !call.is_empty())
            .map(<[String]>::to_vec)
            .collect()
    }

    /// The data, appimages and state directories of a user who keeps them
    /// somewhere else than this home does by default.
    fn elsewhere(&self) -> [(&'static str, PathBuf); 3] {
        [
            ("XDG_DATA_HOME", self.root.join("elsewhere/data")),
            ("APPIMG_DIR", self.root.join("elsewhere/apps")),
            ("XDG_STATE_HOME", self.root.join("elsewhere/state")),
        ]
    }

    /// Runs `appimg` with the directories of [`Home::elsewhere`].
    fn run_elsewhere(&self, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_appimg")), false)
            .envs(self.elsewhere().iter().map(|(key, value)| (key, value)))
            .args(args)
            .output()
            .unwrap()
    }

    /// Installs a fake application at `version` from a local file, updating
    /// from `github:o/{repo}`, with these variables over the environment.
    fn install(&self, name: &str, repo: &str, version: &str, env: &[(&str, PathBuf)]) {
        let payload = self.root.join(format!("payloads/{name}-{version}"));
        fs::create_dir_all(&payload).unwrap();
        fs::write(
            payload.join("fakeapp.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec=AppRun\nIcon=fakeapp\n\
                 Categories=Utility;\nX-AppImage-Version={version}\n"
            ),
        )
        .unwrap();
        let file = self.root.join(format!("home/Downloads/Github_App-{version}.AppImage"));
        fs::write(&file, format!("\x7fELF\npayload={}\n# {name}\nhsqs\n", payload.display()))
            .unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();

        let mut command = self.command(Path::new(env!("CARGO_BIN_EXE_appimg")), false);
        command.envs(env.iter().map(|(key, value)| (key, value)));
        let source = format!("github:o/{repo}");
        let args = ["--yes", "install", file.to_str().unwrap(), "--name", name];
        let output = command.args(args).args(["--update-source", &source]).output().unwrap();
        assert!(output.status.success(), "{}\n{}", stdout(&output), stderr(&output));
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn this_appimg() -> PathBuf {
    fs::canonicalize(env!("CARGO_BIN_EXE_appimg")).unwrap()
}

#[test]
fn enable_writes_both_units_and_starts_the_timer_in_the_user_manager() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::NotifySend);

    let output = home.run_elsewhere(&["notify", "enable"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Update notifications are on"), "{text}");
    assert!(text.contains("through notify-send"), "{text}");

    let service = home.unit("appimg-notify.service");
    let expected = [
        "Type=oneshot".to_string(),
        format!("Environment=\"XDG_DATA_HOME={}\"", home.root.join("elsewhere/data").display()),
        format!("Environment=\"APPIMG_DIR={}\"", home.root.join("elsewhere/apps").display()),
        format!("Environment=\"XDG_STATE_HOME={}\"", home.root.join("elsewhere/state").display()),
        format!("ExecStart=\"{}\" notify check", this_appimg().display()),
    ];
    for line in expected {
        assert!(service.lines().any(|l| l == line), "{line}\n{service}");
    }
    let timer = home.unit("appimg-notify.timer");
    for line in
        ["OnCalendar=daily", "Persistent=true", "RandomizedDelaySec=1h", "WantedBy=timers.target"]
    {
        assert!(timer.lines().any(|l| l == line), "{line}\n{timer}");
    }

    // Through the user manager only, and nothing system-wide.
    assert_eq!(
        home.systemctl_calls(),
        ["--user daemon-reload", "--user enable --now appimg-notify.timer"]
    );
    assert!(home.log("systemctl").iter().all(|call| call.starts_with("--user ")));

    // The appimg under test lives in a cargo target directory.
    let warning = stderr(&output);
    assert!(warning.contains(&format!("the timer runs {}", this_appimg().display())), "{warning}");
    assert!(warning.contains("which is in a cargo target directory"), "{warning}");
    assert!(warning.contains("`appimg notify status` says so"), "{warning}");

    // Enabling again rewrites the units and asks for the same.
    home.ok(&["notify", "enable"]);
    assert_eq!(home.systemctl_calls().len(), 4);
    assert!(home.unit("appimg-notify.service").contains(&format!(
        "Environment=\"APPIMG_DIR={}\"",
        home.root.join("data/appimages").display()
    )));
}

#[test]
fn a_user_manager_that_cannot_be_reached_fails_enable() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::NotifySend);
    fs::write(home.root.join("systemctl.fail"), "").unwrap();

    let output = home.run(&["notify", "enable"]);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("`systemctl --user daemon-reload` failed: Failed to connect to bus"),
        "{message}"
    );
    assert!(message.contains("`appimg notify disable` removes them"), "{message}");
}

#[test]
fn the_timer_names_each_new_version_once_and_only_when_there_is_one() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::NotifySend);
    // Installed into directories that only the run of enable names.
    let elsewhere = home.elsewhere();
    home.install("Github App", "r", "1.0.0", &elsewhere);
    home.install("Other App", "other", "1.0.0", &elsewhere);
    let enabled = home.run_elsewhere(&["notify", "enable"]);
    assert!(enabled.status.success(), "{}", stderr(&enabled));

    // The user manager knows nothing of them, so without what the unit sets
    // there is nothing to check.
    let plain = home.run(&["update", "--all", "--check"]);
    assert_eq!(plain.status.code(), Some(3), "{}", stderr(&plain));
    assert!(stdout(&plain).contains("Nothing installed yet."), "{}", stdout(&plain));

    // Everything current: nothing to show.
    server.release("r", "v1.0.0");
    server.release("other", "v1.0.0");
    let current = home.run_service();
    assert_eq!(current.status.code(), Some(0), "{}", stderr(&current));
    assert!(stdout(&current).contains("No updates available"), "{}", stdout(&current));
    assert!(home.notifications().is_empty(), "{:?}", home.notifications());

    // One update: one notification, naming that app and no other.
    server.release("r", "v2.0.0");
    let found = home.run_service();
    assert_eq!(found.status.code(), Some(0), "{}", stderr(&found));
    assert!(
        stdout(&found).contains("Notified about updates for Github App."),
        "{}",
        stdout(&found)
    );
    assert_eq!(
        home.notifications(),
        [[
            "--app-name=appimg",
            "--icon=system-software-update",
            "An AppImage update is available",
            "Github App 1.0.0 → 2.0.0",
            "appimg update --all installs them",
        ]]
    );

    // Remembered under the XDG_STATE_HOME enable saw, not the one the user
    // manager has.
    let record = home.root.join("elsewhere/state/appimg/announced");
    assert_eq!(fs::read_to_string(&record).unwrap(), "github-app\t2.0.0\n");
    assert!(!home.root.join("state/appimg").exists());

    // The next day the same update is still pending, which is no news.
    let again = home.run_service();
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("Nothing new to notify about: Github App announced already."),
        "{}",
        stdout(&again)
    );
    assert_eq!(home.notifications().len(), 1, "{:?}", home.notifications());

    // Another app's update is news, the pending one beside it is not.
    server.release("other", "v1.1.0");
    assert_eq!(home.run_service().status.code(), Some(0));
    let notifications = home.notifications();
    assert_eq!(notifications.len(), 2, "{notifications:?}");
    assert_eq!(
        notifications[1][2..],
        [
            "An AppImage update is available",
            "Other App 1.0.0 → 1.1.0",
            "appimg update --all installs them",
        ]
    );

    // A newer version of one announced before is news again.
    server.release("r", "v3.0.0");
    assert_eq!(home.run_service().status.code(), Some(0));
    let notifications = home.notifications();
    assert_eq!(notifications.len(), 3, "{notifications:?}");
    assert_eq!(notifications[2][3], "Github App 1.0.0 → 3.0.0");

    // Two new ones at once: one notification for the run, naming both, and
    // a quiet day after it.
    server.release("r", "v4.0.0");
    server.release("other", "v1.2.0");
    assert_eq!(home.run_service().status.code(), Some(0));
    assert_eq!(home.run_service().status.code(), Some(0));
    let notifications = home.notifications();
    assert_eq!(notifications.len(), 4, "{notifications:?}");
    assert_eq!(notifications[3][2], "2 AppImage updates are available");
    assert_eq!(notifications[3][3..5], ["Github App 1.0.0 → 4.0.0", "Other App 1.0.0 → 1.2.0"]);
    assert_eq!(fs::read_to_string(&record).unwrap(), "github-app\t4.0.0\nother-app\t1.2.0\n");
}

#[test]
fn a_failed_check_goes_to_the_journal_and_shows_no_notification() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::NotifySend);
    home.install("Github App", "r", "1.0.0", &[]);
    home.ok(&["notify", "enable"]);

    // The server knows no release of o/r: the check fails.
    let failed = home.run_service();
    assert_eq!(failed.status.code(), Some(1));
    let journal = stderr(&failed);
    assert!(journal.contains("warning: Github App:"), "{journal}");
    assert!(journal.contains("error: 1 of 1 checks failed"), "{journal}");
    assert!(home.notifications().is_empty(), "{:?}", home.notifications());

    // Another app's update is still worth showing, the failure never is.
    home.install("Other App", "other", "1.0.0", &[]);
    server.release("other", "v2.0.0");
    let partly = home.run_service();
    assert_eq!(partly.status.code(), Some(1));
    assert!(stderr(&partly).contains("1 of 2 checks failed"), "{}", stderr(&partly));
    let notifications = home.notifications();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert_eq!(
        notifications[0][3..],
        ["Other App 1.0.0 → 2.0.0", "appimg update --all installs them"]
    );

    // Once the check works, its update is named, and only that one.
    server.release("r", "v2.0.0");
    assert_eq!(home.run_service().status.code(), Some(0));
    let notifications = home.notifications();
    assert_eq!(notifications.len(), 2, "{notifications:?}");
    assert_eq!(notifications[1][3], "Github App 1.0.0 → 2.0.0");

    // A day it fails again forgets nothing: when it works the day after,
    // the same version is still no news.
    server.forget("r");
    assert_eq!(home.run_service().status.code(), Some(1));
    server.release("r", "v2.0.0");
    assert_eq!(home.run_service().status.code(), Some(0));
    assert_eq!(home.notifications().len(), 2, "{:?}", home.notifications());
}

#[test]
fn disable_stops_and_removes_both_units_and_the_record_of_announced_updates() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::NotifySend);
    let enabled = home.run_elsewhere(&["notify", "enable"]);
    assert!(enabled.status.success(), "{}", stderr(&enabled));
    let enabled = home.systemctl_calls().len();
    // What the timer remembered, below the XDG_STATE_HOME the unit names.
    let record = home.root.join("elsewhere/state/appimg/announced");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    fs::write(&record, "github-app\t2.0.0\n").unwrap();

    // Disabled from a shell whose XDG_STATE_HOME is another one.
    let output = home.ok(&["notify", "disable"]);
    let text = stdout(&output);
    assert!(text.contains("Update notifications are off, removed"), "{text}");
    assert!(text.contains(&format!(" and {}.", record.display())), "{text}");
    assert!(!home.unit_path("appimg-notify.service").exists());
    assert!(!home.unit_path("appimg-notify.timer").exists());
    assert!(!record.exists());
    assert!(!home.root.join("elsewhere/state/appimg").exists());
    assert_eq!(
        home.systemctl_calls()[enabled..],
        [
            "--user disable --now appimg-notify.timer",
            "--user stop appimg-notify.service",
            "--user daemon-reload",
        ]
    );

    let again = home.run(&["notify", "disable"]);
    assert_eq!(again.status.code(), Some(3));
    assert!(stdout(&again).contains("off already"), "{}", stdout(&again));
    assert_eq!(home.systemctl_calls().len(), enabled + 3);
}

#[test]
fn without_notify_send_or_gdbus_enable_and_test_say_so() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::Neither);

    for action in ["enable", "test"] {
        let output = home.run_with_tools_only(&["notify", action]);
        assert_eq!(output.status.code(), Some(1), "{action}");
        let message = stderr(&output);
        assert!(
            message.contains("error: neither notify-send nor gdbus is installed, so there is no way to show a notification"),
            "{action}: {message}"
        );
        assert!(message.contains("install notify-send (libnotify) or gdbus (GLib)"), "{message}");
    }
    assert!(!home.root.join("config/systemd").exists());
    assert!(home.log("systemctl").is_empty(), "{:?}", home.log("systemctl"));
}

#[test]
fn test_shows_one_sample_notification_right_away() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::NotifySend);

    let output = home.run_with_tools_only(&["notify", "test"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Sent a test notification through notify-send."));
    let notifications = home.notifications();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    assert_eq!(notifications[0][2], "appimg update notifications work");
    // Nothing else was needed for it.
    assert!(home.log("systemctl").is_empty());
}

#[test]
fn gdbus_calls_the_notification_service_when_notify_send_is_missing() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::Gdbus);

    let output = home.run_with_tools_only(&["notify", "test"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("through gdbus"), "{}", stdout(&output));
    let call = home.log("gdbus");
    let expected = [
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        "--method",
        "org.freedesktop.Notifications.Notify",
        "\"appimg\"",
        "uint32 0",
        "\"system-software-update\"",
        "\"appimg update notifications work\"",
    ];
    assert_eq!(call[..expected.len()], expected);
    assert_eq!(call[expected.len() + 1..], ["@as []", "@a{sv} {}", "int32 -1", "--end--"]);
}

#[test]
fn status_reports_the_timer_the_last_check_and_a_binary_gone_missing() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server, Notifier::NotifySend);

    let off = home.ok(&["notify", "status"]);
    assert!(stdout(&off).contains("Update notifications are off."), "{}", stdout(&off));

    // Enabled from a copy, which can then go away.
    let copy = home.root.join("bin/appimg");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_appimg"), &copy).unwrap();
    let enabled = home.command(&copy, false).args(["notify", "enable"]).output().unwrap();
    assert!(enabled.status.success(), "{}", stderr(&enabled));
    assert!(stderr(&enabled).contains("which is in a temporary directory"));

    fs::write(
        home.root.join("show/appimg-notify.timer"),
        "LoadState=loaded\nActiveState=active\nUnitFileState=enabled\n\
         NextElapseUSecRealtime=Mon 2026-10-05 00:23:11 CEST\n\
         LastTriggerUSec=Sun 2026-10-04 00:41:02 CEST\n",
    )
    .unwrap();
    fs::write(
        home.root.join("show/appimg-notify.service"),
        "ActiveState=inactive\nResult=exit-code\nExecMainStatus=1\n\
         ExecMainStartTimestamp=Sun 2026-10-04 00:41:02 CEST\n\
         ExecMainExitTimestamp=Sun 2026-10-04 00:41:03 CEST\n",
    )
    .unwrap();

    let status = home.ok(&["notify", "status"]);
    let text = stdout(&status);
    for line in [
        "Timer          on".to_string(),
        "Next check     Mon 2026-10-05 00:23:11 CEST".to_string(),
        "Last check     Sun 2026-10-04 00:41:02 CEST, failed with exit code 1, see \
         `journalctl --user -u appimg-notify.service`"
            .to_string(),
        format!("Runs           {}", copy.display()),
        format!("Apps in        {}", home.root.join("data/appimages").display()),
        "Notifies with  notify-send".to_string(),
    ] {
        assert!(text.lines().any(|l| l == line), "{line}\n{text}");
    }

    fs::remove_file(&copy).unwrap();
    let missing = home.run(&["notify", "status"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stdout(&missing).contains(&format!("Runs           {}, which is missing", copy.display())),
        "{}",
        stdout(&missing)
    );
    assert!(
        stderr(&missing)
            .contains(&format!("the timer runs {}, which no longer exists", copy.display())),
        "{}",
        stderr(&missing)
    );

    // A timer that was never fired and a check that is not known yet.
    fs::write(
        home.root.join("show/appimg-notify.timer"),
        "LoadState=loaded\nActiveState=inactive\nUnitFileState=disabled\n\
         NextElapseUSecRealtime=\nLastTriggerUSec=\n",
    )
    .unwrap();
    fs::write(home.root.join("show/appimg-notify.service"), "ActiveState=inactive\n").unwrap();
    let text = stdout(&home.run(&["notify", "status"]));
    assert!(text.contains("Timer          off (disabled), `appimg notify enable` turns it on"));
    assert!(text.contains("Next check     none"), "{text}");
    assert!(text.contains("Last check     never"), "{text}");
}
