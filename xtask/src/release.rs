//! `cargo xtask release vX.Y.Z`: one command sequence for a release with
//! artifacts from both lanes and the build report.
//!
//! Run on the development Mac, from a clean `main` at `origin/main` whose
//! version is `X.Y.Z` and whose tree has passed the full macOS gate:
//!
//! 1. Build the macOS zip and the Dreamer Web zip, Developer ID signed,
//!    notarized, and stapled; draw pages with them; and add Dreamer Web
//!    from its zip away from the tree; build the ARM64 Linux desktop
//!    tarball and server archive in an ARM64 container; run the ARM64
//!    archives on fresh containers.
//! 2. Create the tag on GitHub, which starts `release.yml`: it builds the
//!    x86-64 archives, runs them on fresh containers, and drafts the
//!    release with them.
//! 3. Wait for that run, take its x86-64 archives and fresh-check record
//!    back from the draft, write the build report with the run attached,
//!    and add the Mac's artifacts and the report to the draft.
//!
//! The release stays a draft: publishing it is the owner's decision.

use std::path::Path;
use std::process::Command;

use crate::util::repo_root;

/// The notarytool keychain profile a release uses unless told otherwise.
pub const DEFAULT_NOTARY_PROFILE: &str = "dreamer-notary";

/// The keychain's Developer ID Application identity, when there is exactly
/// one.
fn developer_id_identity() -> Result<String, String> {
    let out = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .map_err(|e| format!("security: {e}"))?;
    let found: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split('"').nth(1))
        .filter(|n| n.starts_with("Developer ID Application:"))
        .map(str::to_owned)
        .collect();
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(
            "no Developer ID Application identity in the keychain; a release is signed with one"
                .into(),
        ),
        _ => Err(format!(
            "several Developer ID Application identities; name one with --sign: {}",
            found.join(", ")
        )),
    }
}

fn sh(root: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `gh` with `args`, tried three times: a dropped connection to GitHub
/// should not end a release halfway.
fn gh(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 1..=3 {
        match sh(root, "gh", args) {
            Ok(out) => return Ok(out),
            Err(e) => {
                println!("release: gh failed (attempt {attempt} of 3): {e}");
                last = e;
                std::thread::sleep(std::time::Duration::from_secs(10 * attempt));
            }
        }
    }
    Err(last)
}

/// Wait for workflow run `run` to finish, polling its status; whether it
/// succeeded. Failed polls (the network, not the run) are retried for up
/// to ten minutes in a row.
fn wait_for_run(root: &Path, run: &str) -> Result<bool, String> {
    let path = format!("repos/{{owner}}/{{repo}}/actions/runs/{run}");
    let mut failing_since: Option<std::time::Instant> = None;
    loop {
        match sh(
            root,
            "gh",
            &[
                "api",
                &path,
                "--jq",
                r#".status + " " + (.conclusion // "")"#,
            ],
        ) {
            Ok(state) => {
                failing_since = None;
                if let Some(conclusion) = state.strip_prefix("completed ") {
                    return Ok(conclusion == "success");
                }
            }
            Err(e) => {
                let since = *failing_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > std::time::Duration::from_secs(600) {
                    return Err(format!("cannot read run {run} for ten minutes: {e}"));
                }
                println!("release: reading run {run} failed, retrying: {e}");
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

fn step(root: &Path, args: &[&str]) -> Result<(), String> {
    println!("release: cargo xtask {}", args.join(" "));
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let ok = Command::new(exe)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if ok {
        Ok(())
    } else {
        Err(format!("`cargo xtask {}` failed", args.join(" ")))
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: cargo xtask release vX.Y.Z [--sign IDENTITY] [--notarize PROFILE] [--resume]";
    let (tag, rest) = match args.split_first() {
        Some((t, rest)) if t.starts_with('v') => (t.clone(), rest),
        _ => return Err(USAGE.into()),
    };
    // Every release is Developer ID signed and notarized. The identity is
    // the keychain's one Developer ID Application identity unless named;
    // the notary profile is `dreamer-notary` unless named.
    let mut identity = None;
    let mut profile = DEFAULT_NOTARY_PROFILE.to_owned();
    let mut resume = false;
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--resume" => resume = true,
            "--sign" => identity = Some(iter.next().ok_or(USAGE)?.clone()),
            "--notarize" => profile = iter.next().ok_or(USAGE)?.clone(),
            _ => return Err(USAGE.into()),
        }
    }
    if !cfg!(target_os = "macos") {
        return Err("the release runs on the development Mac (it builds the macOS zip)".into());
    }
    let root = repo_root();

    // Preconditions: nothing about them is assumed.
    let version = crate::package::version(&root)?;
    if tag != format!("v{version}") {
        return Err(format!(
            "the workspace version is {version}; bump it first, or release v{version}"
        ));
    }
    if sh(&root, "git", &["branch", "--show-current"])? != "main" {
        return Err("release from main".into());
    }
    if !sh(
        &root,
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        return Err("the work tree has uncommitted changes".into());
    }
    sh(&root, "git", &["fetch", "-q", "origin", "main"])?;
    let commit = sh(&root, "git", &["rev-parse", "HEAD"])?;
    if commit != sh(&root, "git", &["rev-parse", "origin/main"])? {
        return Err("main is not at origin/main; pull or push first".into());
    }
    // `--resume` continues a release whose tag is made: the tag must be
    // this commit, and the Mac's artifacts already built.
    let tagged = sh(
        &root,
        "git",
        &["ls-remote", "origin", &format!("refs/tags/{tag}")],
    )?;
    let out = root.join("target/package");
    if resume {
        if tagged.split_whitespace().next() != Some(commit.as_str()) {
            return Err(format!("--resume: {tag} is not on GitHub at {commit}"));
        }
        let zip = out.join(format!("dreamer-{version}-aarch64-apple-darwin.zip"));
        let notarized = std::fs::read_to_string(zip.with_extension("zip.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .is_some_and(|r| r["signing"]["notarized"] == true);
        if !zip.is_file() || !notarized || !out.join("fresh-arm64.json").is_file() {
            return Err(format!(
                "--resume: target/package has no notarized {} or ARM64 fresh record from this release",
                zip.display()
            ));
        }
    } else if !tagged.is_empty() {
        return Err(format!(
            "{tag} already exists on GitHub (`--resume` continues a release after its tag)"
        ));
    }
    let tree = sh(&root, "git", &["rev-parse", "HEAD^{tree}"])?;
    let gate = root.join(format!("target/gate/mac/{tree}-full.json"));
    let passed = std::fs::read_to_string(&gate)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|g| g["result"] == "passed");
    if !passed {
        return Err(format!(
            "no passed full macOS gate for tree {tree}; run `cargo xtask gate --full` first"
        ));
    }

    if !resume {
        build_and_tag(&root, &out, &tag, &commit, identity, &profile)?;
    }
    finish(&root, &out, &tag, &commit)
}

/// Steps 1 and 2: the Mac's artifacts, signed and checked, then the tag.
fn build_and_tag(
    root: &Path,
    out: &Path,
    tag: &str,
    commit: &str,
    identity: Option<String>,
    profile: &str,
) -> Result<(), String> {
    let root = root.to_path_buf();
    let identity = match identity {
        Some(id) => id,
        None => developer_id_identity()?,
    };
    // The credentials are checked before anything is built or tagged.
    sh(
        &root,
        "xcrun",
        &["notarytool", "history", "--keychain-profile", profile],
    )
    .map_err(|e| {
        format!(
            "notary profile `{profile}` does not work ({e}); store it once with \
             `xcrun notarytool store-credentials {profile} --apple-id APPLE_ID --team-id TEAMID`"
        )
    })?;
    println!("release: signing with {identity}, notarizing with profile {profile}");

    // 1. The Mac's artifacts, from nothing.
    let _ = std::fs::remove_dir_all(out);
    step(
        &root,
        &[
            "package",
            "--platform",
            "macos",
            "--sign",
            &identity,
            "--notarize",
            profile,
        ],
    )?;
    // Chromium under the hardened runtime: the signed component draws pages.
    crate::web::smoke_packaged(true)?;
    step(&root, &["package", "--fresh", "--platform", "macos"])?;
    step(
        &root,
        &[
            "package",
            "--platform",
            "linux",
            "--container",
            "--arch",
            "arm64",
        ],
    )?;
    step(&root, &["package", "--fresh", "--arch", "arm64"])?;

    // 2. The tag, through the API: the push hook runs only for pushes.
    println!("release: tagging {commit} as {tag}");
    gh(
        &root,
        &[
            "api",
            "repos/{owner}/{repo}/git/refs",
            "-f",
            &format!("ref=refs/tags/{tag}"),
            "-f",
            &format!("sha={commit}"),
        ],
    )?;

    Ok(())
}

/// Step 3: the x86-64 lane's archives taken back, the build report, and
/// the Mac's artifacts added to the draft.
fn finish(root: &Path, out: &Path, tag: &str, commit: &str) -> Result<(), String> {
    let root = root.to_path_buf();
    let out = out.to_path_buf();
    // 3. The x86-64 lane.
    let mut run = String::new();
    for _ in 0..60 {
        run = gh(
            &root,
            &[
                "run",
                "list",
                "--workflow",
                "release.yml",
                "--commit",
                commit,
                "--json",
                "databaseId",
                "--jq",
                ".[0].databaseId",
            ],
        )?;
        if !run.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    if run.is_empty() {
        return Err(format!("release.yml never started for {tag}"));
    }
    println!("release: waiting for release.yml run {run}");
    if !wait_for_run(&root, &run)? {
        return Err(format!(
            "release.yml run {run} failed; the draft has no x86-64 archives"
        ));
    }
    let out_s = out.display().to_string();
    for (pattern, dir) in [
        (
            "dreamer-x86_64-unknown-linux-gnu.tar.gz*",
            format!("{out_s}/linux"),
        ),
        (
            "dreamer-server-*-x86_64-unknown-linux-gnu.tar.gz*",
            format!("{out_s}/server"),
        ),
        (
            "dreamer-*-x86_64-unknown-linux-gnu.deb*",
            format!("{out_s}/linux"),
        ),
        (
            "dreamer-*-x86_64-unknown-linux-gnu.rpm*",
            format!("{out_s}/linux"),
        ),
        ("fresh-amd64.json", out_s.clone()),
    ] {
        gh(
            &root,
            &[
                "release",
                "download",
                tag,
                "--clobber",
                "--pattern",
                pattern,
                "--dir",
                &dir,
            ],
        )?;
    }
    step(&root, &["build-report", "--ci-run", &run])?;

    let mut upload: Vec<String> = Vec::new();
    for (dir, keep) in [
        (
            out.clone(),
            &[
                // The base zip and the Dreamer Web zip, each with its
                // `.sha256` and `.json`.
                "aarch64-apple-darwin.zip",
                "build-report.",
                "fresh-arm64.json",
                "fresh-macos.json",
            ][..],
        ),
        (
            out.join("linux"),
            &[
                "aarch64-unknown-linux-gnu.tar.gz",
                "aarch64-unknown-linux-gnu.deb",
                "aarch64-unknown-linux-gnu.rpm",
            ][..],
        ),
        (
            out.join("server"),
            &["aarch64-unknown-linux-gnu.tar.gz"][..],
        ),
    ] {
        for e in std::fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_file() && keep.iter().any(|k| name.contains(k)) {
                upload.push(e.path().display().to_string());
            }
        }
    }
    upload.sort();
    // One file at a time, each retried: a large zip that fails to upload
    // does not send the others again.
    for file in &upload {
        gh(&root, &["release", "upload", tag, "--clobber", file])?;
    }
    let url = gh(
        &root,
        &["release", "view", tag, "--json", "url", "--jq", ".url"],
    )?;
    println!("release: draft {url}");
    println!(
        "release: {} files added from the Mac; review the build report, then publish",
        upload.len()
    );
    Ok(())
}
