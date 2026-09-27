use std::{path::Path, process::Command};

use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn tool_output(command: &mut Command, name: &str) -> String {
    let result = command
        .output()
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    assert!(
        result.status.success(),
        "{name} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap_or_else(|error| panic!("{name} returned non-UTF-8 output: {error}"))
        .trim()
        .to_owned()
}

fn main() {
    println!("cargo:rerun-if-env-changed=QMAKE");
    println!("cargo:rerun-if-env-changed=FURAMI_MEDIA_PREFIX");
    println!("cargo:rerun-if-changed=packaging/media/stack.lock.json");

    let lock: serde_json::Value =
        serde_json::from_str(include_str!("packaging/media/stack.lock.json"))
            .expect("invalid pinned media stack lock");
    let qt_version = lock["sources"]["qt"]["version"]
        .as_str()
        .expect("media stack lock has no Qt version");
    let qmake = std::env::var_os("QMAKE").unwrap_or_else(|| "qmake6".into());
    let actual_qt = tool_output(Command::new(&qmake).args(["-query", "QT_VERSION"]), "qmake");
    assert_eq!(
        actual_qt, qt_version,
        "Qt kit differs from the frozen media stack"
    );

    if let Some(prefix) = std::env::var_os("FURAMI_MEDIA_PREFIX") {
        let prefix = Path::new(&prefix);
        let expected_qmake = prefix.join("bin/qmake6");
        let actual_qmake = Path::new(&qmake);
        assert!(
            actual_qmake.is_absolute()
                && actual_qmake.canonicalize().ok() == expected_qmake.canonicalize().ok()
                && expected_qmake.is_file(),
            "QMAKE must point to the frozen prefix's bin/qmake6"
        );
        let api = lock["sources"]["mpv"]["client_api"]
            .as_str()
            .expect("media stack lock has no libmpv client API");
        let pkgconfig = prefix.join("lib/pkgconfig");
        let actual_api = tool_output(
            Command::new("pkg-config")
                .args(["--modversion", "mpv"])
                .env("PKG_CONFIG_LIBDIR", pkgconfig)
                .env_remove("PKG_CONFIG_PATH")
                .env_remove("PKG_CONFIG_SYSROOT_DIR"),
            "frozen libmpv pkg-config",
        );
        assert_eq!(actual_api, api, "libmpv client API differs from stack lock");
        println!(
            "cargo:rustc-link-search=native={}",
            prefix.join("lib").display()
        );
        println!("cargo:rustc-link-lib=dylib=mpv");
    }

    CxxQtBuilder::new_qml_module(QmlModule::new("dev.antho.furami").qml_file("qml/Main.qml"))
        .build();
}
