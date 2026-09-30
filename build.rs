use std::{path::Path, process::Command};

use cxx_qt_build::{CppFile, CxxQtBuilder, MocArguments, QmlModule};

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
    }

    let native_flags = tool_output(
        Command::new("pkg-config").args(["--cflags", "x11", "xcb", "xcb-shape"]),
        "X11/XCB/XCB-SHAPE development prerequisites",
    );
    let builder =
        CxxQtBuilder::new_qml_module(QmlModule::new("dev.antho.furami").qml_file("qml/Main.qml"))
            .file("src/ui/bridge.rs")
            .qt_module("Quick")
            .cpp_file("src/native_host/host.cpp")
            .cpp_file(
                CppFile::from("src/native_host/host.h")
                    .compile(false)
                    .moc(true)
                    .moc_arguments(MocArguments::default().uri("dev.antho.furami")),
            )
            .include_dir("src/native_host");
    // SAFETY: With CXX-Qt pinned to 0.10.0, this closure only appends pkg-config's
    // native prerequisite flags. It does not replace generated inputs or change
    // CXX-Qt's compiler language, C++ standard, or ownership configuration.
    let builder = unsafe {
        builder.cc_builder(move |compiler| {
            for flag in native_flags.split_whitespace() {
                compiler.flag(flag);
            }
        })
    };
    builder.build();
    for library in ["X11", "xcb", "xcb-shape"] {
        println!("cargo:rustc-link-lib={library}");
    }
}
