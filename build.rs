use std::{path::Path, path::PathBuf, process::Command};

use cxx_qt_build::{
    CppFile, CxxQtBuilder, MocArguments, QResource, QResources, QmlFile, QmlModule,
};

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
    println!("cargo:rerun-if-changed=assets/logo.svg");

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

    // Diagnostics build provenance: the FUR-023 report embeds two bounded
    // projections generated here, so the runtime never reads the media
    // manifest, the packaging lock or any CLI for an environment report.
    write_diagnostics_provenance();

    cxx_build::bridge("src/capture/linux/ffi.rs")
        .file("src/capture/linux/v4l2.cpp")
        .include("src/capture/linux")
        .std("c++17")
        .compile("furami_capture_linux");
    for source in [
        "src/capture/linux/ffi.rs",
        "src/capture/linux/v4l2.h",
        "src/capture/linux/v4l2.cpp",
    ] {
        println!("cargo:rerun-if-changed={source}");
    }

    let native_flags = tool_output(
        Command::new("pkg-config").args(["--cflags", "x11", "xcb", "xcb-shape"]),
        "X11/XCB/XCB-SHAPE development prerequisites",
    );
    // Every QML file in qml/ is registered explicitly in the module. The three
    // token, projection and chrome-visibility files are singletons.
    let mut qml_files: Vec<std::path::PathBuf> = std::fs::read_dir("qml")
        .expect("qml directory")
        .map(|entry| entry.expect("qml entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "qml"))
        .collect();
    qml_files.sort();
    assert!(!qml_files.is_empty(), "no QML files found to register");
    let singletons = ["qml/Brume.qml", "qml/Chrome.qml", "qml/Projection.qml"];
    let qml_module = QmlModule::new("dev.antho.furami").qml_files(qml_files.iter().map(|path| {
        let file = QmlFile::from(path.clone());
        if path.to_str().is_some_and(|name| singletons.contains(&name)) {
            file.singleton(true)
        } else {
            file
        }
    }));

    // Bundled chrome icons and their attribution ship inside the binary; no
    // icon is ever loaded from a URL or from an OS theme at runtime.
    let mut resources: Vec<String> = std::fs::read_dir("qml/icons")
        .expect("qml/icons directory")
        .map(|entry| entry.expect("qml/icons entry").path())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    resources.sort();
    // The Furami brand mark is embedded as the scalable SVG and set as the
    // application window icon; the preflighted Qt SVG icon engine rasterizes it
    // into the standard icon sizes, so no raster copy is pushed per window.
    resources.push("assets/logo.svg".to_owned());

    let builder = CxxQtBuilder::new_qml_module(qml_module)
        .qrc_resources(QResources::new().resource(QResource::new().files(resources)))
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
    // --require-defined forces GNU ld to extract the root CXX-Qt initializer
    // (cxx_qt_init_crate_furami) from the rlibs archives: it sits semantically
    // before archive traversal, and pulls in the provider archives that bundle
    // the QML module, cxx_qt_lib and Qt glue for all final targets
    // (bin, tests and examples alike).
    println!("cargo:rustc-link-arg=-Wl,--require-defined=cxx_qt_init_crate_furami");
    // The generated CXX-Qt static archive (libfurami_cxxqt_generated.a) is emitted
    // before any Qt6Quick reference is resolved. With --as-needed (the default
    // final-campaign link command), a shared library is retained only if it
    // satisfies an undefined symbol from objects already seen at its position on
    // the link line — so the .qt_module("Quick") directive emitted by
    // builder.build() is dropped before the archive that needs it is scanned.
    // Re-emitting Qt6Quick here places it after the archive, so the linker sees
    // the archive's undefined Quick symbols first and retains the library.
    println!("cargo:rustc-link-lib=Qt6Quick");
    for library in ["X11", "xcb", "xcb-shape"] {
        println!("cargo:rustc-link-lib={library}");
    }
}

/// FIPS 180-4 SHA-256 over a byte slice, as lowercase hex. The build script
/// cannot take a dependency, so this mirrors the compact implementation the
/// FUR-002 fixture tests already use.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for (i, &ki) in K.iter().enumerate() {
            let t1 = h
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ ((!e) & g))
                .wrapping_add(ki)
                .wrapping_add(w[i]);
            let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    state.iter().map(|word| format!("{word:08x}")).collect()
}

fn unavailable(reason: &str) -> serde_json::Value {
    serde_json::json!({ "status": "unavailable", "reason": reason })
}

// One shared, dependency-free validation vocabulary governs both this build
// projection and the inventory tests, so a validator can never drift between
// what is enforced and what is asserted.
include!("src/diagnostics/provenance_validation.rs");

/// Project one optional text field. `Err(())` means present-but-invalid and is
/// always turned into a typed unavailability by the caller.
fn text_field(
    value: Option<&serde_json::Value>,
    valid: fn(&str) -> bool,
) -> Result<Option<serde_json::Value>, ()> {
    match value {
        None => Ok(None),
        Some(item) => match item.as_str() {
            Some(text) if valid(text) => Ok(Some(serde_json::Value::String(text.to_owned()))),
            _ => Err(()),
        },
    }
}

fn json_u64(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    value
        .and_then(|item| item.as_u64())
        .map(serde_json::Value::from)
}

/// Keep only the allowed scalar provenance fields of the media stack sources:
/// validated versions, commits and archive hashes. Any unknown source key, any
/// non-object source, and any present-but-invalid or non-string field is a
/// typed unavailability, so private text can never survive.
fn source_projection(sources: Option<&serde_json::Value>) -> Result<serde_json::Value, ()> {
    let map = sources.and_then(|value| value.as_object()).ok_or(())?;
    if map.is_empty() {
        return Err(());
    }
    let mut out = serde_json::Map::new();
    for (name, source) in map {
        if !is_source_name(name) {
            return Err(());
        }
        let object = source.as_object().ok_or(())?;
        let mut entry = serde_json::Map::new();
        for key in ["version", "commit", "sha256", "client_api", "kit"] {
            match object.get(key) {
                None => {}
                Some(serde_json::Value::String(text)) if is_source_scalar(key, text) => {
                    entry.insert(key.to_owned(), serde_json::Value::String(text.to_owned()));
                }
                Some(_) => return Err(()),
            }
        }
        out.insert(name.clone(), serde_json::Value::Object(entry));
    }
    Ok(serde_json::Value::Object(out))
}

/// The digest of one real (non-symlink) media artifact, keyed by its manifest
/// relative path. Symlinks are refused so their target-string hashes can never
/// be mistaken for the artifact's content identity.
fn file_artifact(manifest: &serde_json::Value, relative: &str) -> Option<serde_json::Value> {
    let entry = manifest.get("artifacts")?.get(relative)?;
    if entry.get("type").and_then(|value| value.as_str()) != Some("file") {
        return None;
    }
    let sha256 = entry.get("sha256").and_then(|value| value.as_str())?;
    if !hex_range(sha256, 64, 64) {
        return None;
    }
    Some(serde_json::json!({ "path": relative, "sha256": sha256 }))
}

/// Projection of the frozen media provider's `manifest.json` for the report's
/// `build_provider`. Absent or malformed input is a typed unavailability, never
/// a guessed identity. The historical `stack_identity.txt` hash is unrelated to
/// this manifest's `lock_sha256` and is never compared to it.
fn media_provider_projection() -> serde_json::Value {
    let Some(prefix) = std::env::var_os("FURAMI_MEDIA_PREFIX") else {
        return unavailable("manifest_absent");
    };
    let prefix = PathBuf::from(prefix);
    let Some(manifest_path) = prefix.parent().map(|parent| parent.join("manifest.json")) else {
        return unavailable("manifest_absent");
    };
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let Ok(bytes) = std::fs::read(&manifest_path) else {
        return unavailable("manifest_absent");
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return unavailable("manifest_malformed");
    };
    if manifest.get("schema").and_then(|value| value.as_u64()) != Some(1) {
        return unavailable("manifest_malformed");
    }
    let Some(lock_sha256) = manifest
        .get("lock_sha256")
        .and_then(|value| value.as_str())
        .filter(|value| hex_range(value, 64, 64))
    else {
        return unavailable("manifest_malformed");
    };
    let Ok(sources) = source_projection(manifest.get("sources")) else {
        return unavailable("manifest_malformed");
    };
    let Ok(container) = text_field(manifest.get("container"), is_container) else {
        return unavailable("manifest_malformed");
    };
    let Ok(apt_snapshot) = text_field(manifest.get("apt_snapshot"), is_snapshot) else {
        return unavailable("manifest_malformed");
    };
    let client_api = match text_field(
        manifest.pointer("/sources/mpv/client_api"),
        is_dotted_digits,
    ) {
        Ok(value) => value,
        Err(()) => return unavailable("manifest_malformed"),
    };
    // Only the three packaged media files bind the qualified reference, and the
    // libmpv file name can only be built from a dotted-digit client API.
    let mut artifacts = serde_json::Map::new();
    for (logical, relative) in [("mpv", "bin/mpv"), ("ffmpeg", "bin/ffmpeg")] {
        if let Some(entry) = file_artifact(&manifest, relative) {
            artifacts.insert(logical.to_owned(), entry);
        }
    }
    if let Some(client_api) = client_api.as_ref().and_then(|value| value.as_str()) {
        let relative = format!("lib/libmpv.so.{client_api}");
        if let Some(entry) = file_artifact(&manifest, &relative) {
            artifacts.insert("libmpv".to_owned(), entry);
        }
    }
    let mut projection = serde_json::Map::new();
    projection.insert(
        "status".to_owned(),
        serde_json::Value::String("observed".to_owned()),
    );
    projection.insert(
        "lock_sha256".to_owned(),
        serde_json::Value::String(lock_sha256.to_owned()),
    );
    for (key, value) in [("container", container), ("apt_snapshot", apt_snapshot)] {
        if let Some(value) = value {
            projection.insert(key.to_owned(), value);
        }
    }
    projection.insert("sources".to_owned(), sources);
    projection.insert("artifacts".to_owned(), serde_json::Value::Object(artifacts));
    serde_json::Value::Object(projection)
}

/// Projection of the currently selected media stack recipe. This is the wanted
/// source provenance; it never certifies the provider that actually produced the
/// running build.
fn source_recipe_projection() -> serde_json::Value {
    println!("cargo:rerun-if-changed=packaging/media/stack.lock.json");
    let Ok(bytes) = std::fs::read("packaging/media/stack.lock.json") else {
        return unavailable("recipe_absent");
    };
    let Ok(lock) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return unavailable("recipe_malformed");
    };
    if lock.get("schema").and_then(|value| value.as_u64()) != Some(1) {
        return unavailable("recipe_malformed");
    }
    // A malformed recipe lock, or any unvalidated source key or scalar, is a
    // typed unavailability: no private text and no guessed identity.
    let Ok(sources) = source_projection(lock.get("sources")) else {
        return unavailable("recipe_malformed");
    };
    let Ok(container) = text_field(lock.get("container"), is_container) else {
        return unavailable("recipe_malformed");
    };
    let Ok(platform) = text_field(lock.get("platform"), is_platform) else {
        return unavailable("recipe_malformed");
    };
    let Ok(apt_snapshot) = text_field(lock.get("apt_snapshot"), is_snapshot) else {
        return unavailable("recipe_malformed");
    };
    let mut projection = serde_json::Map::new();
    projection.insert(
        "status".to_owned(),
        serde_json::Value::String("observed".to_owned()),
    );
    projection.insert(
        "lock_sha256".to_owned(),
        serde_json::Value::String(sha256_hex(&bytes)),
    );
    for (key, value) in [
        ("schema", json_u64(lock.get("schema"))),
        ("container", container),
        ("platform", platform),
        ("apt_snapshot", apt_snapshot),
        ("source_date_epoch", json_u64(lock.get("source_date_epoch"))),
    ] {
        if let Some(value) = value {
            projection.insert(key.to_owned(), value);
        }
    }
    projection.insert("sources".to_owned(), sources);
    serde_json::Value::Object(projection)
}

fn write_diagnostics_provenance() {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set for build.rs"));
    for (name, projection) in [
        (
            "diagnostics_media_provider.json",
            media_provider_projection(),
        ),
        ("diagnostics_source_recipe.json", source_recipe_projection()),
    ] {
        let mut text = serde_json::to_string_pretty(&projection)
            .expect("diagnostics provenance projection serializes");
        text.push('\n');
        std::fs::write(out_dir.join(name), text).expect("diagnostics provenance projection writes");
    }
}
