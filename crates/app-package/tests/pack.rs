//! Packing a folder: what goes in, what is left out, and the same bytes
//! on every platform (the hash below is checked on macOS and on Linux).

use std::path::{Path, PathBuf};

fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dreamer-pack-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn put(dir: &Path, path: &str, text: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("parent");
    std::fs::write(p, text).expect("write");
}

/// A package with a script, a nested file, an executable, and everything
/// that is left out. Files are written in an order unlike the package's.
fn app(name: &str) -> PathBuf {
    let dir = folder(name);
    put(&dir, "web/index.html", "<p>hi</p>\n");
    put(&dir, "app.ts", "export default {};\n");
    put(
        &dir,
        "app.toml",
        "schema_version = 2\nid = \"local.hello\"\nversion = \"0.1.0\"\ntitle = \"Hello\"\n",
    );
    put(&dir, "bin/run", "#!/bin/sh\necho hi\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("bin/run"), std::fs::Permissions::from_mode(0o700))
            .expect("chmod");
    }
    put(&dir, ".git/HEAD", "ref\n");
    put(&dir, ".env", "SECRET=1\n");
    put(&dir, "node_modules/x/index.js", "x\n");
    put(&dir, "build/app.js", "built\n");
    put(&dir, "notes.log", "log\n");
    put(&dir, "drafts/a.md", "draft\n");
    put(&dir, ".dreamerignore", "*.log\ndrafts/\n");
    dir
}

/// The package of [`app`]: recorded once, the same everywhere.
const HELLO_SHA256: &str = "7841d16165d31ca9070e1f0fe16ba90436aa78dce1c96632ee72534bfc4bfada";

#[test]
fn the_same_folder_packs_to_the_same_recorded_bytes() {
    let a = app_package::pack(&app("a")).expect("pack");
    assert_eq!(a.files, ["app.toml", "app.ts", "bin/run", "web/index.html"]);
    assert_eq!(a.sha256, HELLO_SHA256);
    let b = app_package::pack(&app("b")).expect("pack again");
    assert_eq!(a.bytes, b.bytes);

    let dir = app("c");
    put(&dir, "app.ts", "export default {} ;\n");
    assert_ne!(app_package::pack(&dir).expect("pack").sha256, a.sha256);
}

#[test]
fn unpack_writes_what_pack_read() {
    let packed = app_package::pack(&app("d")).expect("pack");
    let into = folder("d-out").join("x");
    let out = app_package::unpack(&packed.bytes, &into).expect("unpack");
    assert_eq!(out.files, packed.files);
    assert!(out.manifest.contains("local.hello"));
    assert_eq!(
        std::fs::read_to_string(into.join("web/index.html")).expect("read"),
        "<p>hi</p>\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| {
            std::fs::metadata(into.join(p))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!((mode("bin/run"), mode("app.ts")), (0o755, 0o644));
    }
    assert!(app_package::unpack(&packed.bytes, &into).is_err(), "exists");
    // The unpacked folder packs to the same package.
    assert_eq!(
        app_package::pack(&into).expect("repack").sha256,
        packed.sha256
    );
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_is_refused_unless_left_out() {
    let dir = app("e");
    std::os::unix::fs::symlink("/etc/hosts", dir.join("hosts")).expect("link");
    let e = app_package::pack(&dir).expect_err("link");
    assert!(e.contains("symbolic link"), "{e}");
    put(&dir, ".dreamerignore", "hosts\n");
    app_package::pack(&dir).expect("left out");
}
