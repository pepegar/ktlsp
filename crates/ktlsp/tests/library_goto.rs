//! End-to-end test of library goto-definition: locate a `-sources.jar`, extract it, index its
//! Kotlin + Java sources, and resolve goto from a user Kotlin file into the extracted library
//! source. The hermetic test builds a fake Gradle cache (no network); the `#[ignore]`d test
//! exercises the real Maven Central download path.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ktlsp::artifacts::{self, Repos};
use ktlsp::coords::Coordinate;
use ktlsp::deps;
use ktlsp::java::JavaParser;
use ktlsp::parser::KotlinParser;
use ktlsp::workspace::Workspace;

use zip::write::SimpleFileOptions;

fn unique_tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ktlsp_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a sources jar containing the given (entry-name, body) pairs.
fn write_sources_jar(path: &Path, entries: &[(&str, &str)]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, body) in entries {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

/// Index a coordinate's sources into a fresh workspace; return it plus the extraction root.
fn index_into_workspace(coord: &Coordinate, repos: &Repos, extract_root: &Path) -> Workspace {
    let mut kotlin = KotlinParser::new();
    let mut java = JavaParser::new();
    let batches = deps::resolve_coordinate(coord, repos, extract_root, &mut kotlin, &mut java);
    assert!(
        !batches.is_empty(),
        "expected indexed library files for {}",
        coord.label()
    );
    let mut ws = Workspace::new();
    for batch in batches {
        ws.index
            .replace_file(&batch.file, batch.symbols, ktlsp::index::Tier::Durable);
    }
    ws
}

fn index_coordinate_closure_into_workspace(
    coord: &Coordinate,
    repos: &Repos,
    extract_root: &Path,
) -> Workspace {
    let mut queue = VecDeque::from([coord.clone()]);
    let mut seen = BTreeSet::new();
    let mut kotlin = KotlinParser::new();
    let mut java = JavaParser::new();
    let mut ws = Workspace::new();
    while let Some(next) = queue.pop_front() {
        if !seen.insert(next.clone()) {
            continue;
        }
        for batch in deps::resolve_coordinate(&next, repos, extract_root, &mut kotlin, &mut java) {
            ws.index
                .replace_file(&batch.file, batch.symbols, ktlsp::index::Tier::Durable);
        }
        for dep in artifacts::dependency_coordinates(repos, &next) {
            if !seen.contains(&dep) && !queue.iter().any(|queued| queued == &dep) {
                queue.push_back(dep);
            }
        }
    }
    ws
}

fn index_coordinates_with_selection(
    coords: &[Coordinate],
    repos: &Repos,
    extract_root: &Path,
) -> Workspace {
    let mut selector = deps::CoordinateSelector::new();
    let mut indexed_files: BTreeMap<Coordinate, Vec<String>> = BTreeMap::new();
    let mut kotlin = KotlinParser::new();
    let mut java = JavaParser::new();
    let mut ws = Workspace::new();

    for coord in coords {
        match selector.consider(coord.clone()) {
            deps::CoordinateDecision::Selected => {}
            deps::CoordinateDecision::Replaces(previous) => {
                if let Some(files) = indexed_files.remove(&previous) {
                    for file in files {
                        ws.index.remove_file(&file);
                    }
                }
            }
            deps::CoordinateDecision::ShadowedBy(_) => continue,
        }

        let mut files = Vec::new();
        for batch in deps::resolve_coordinate(coord, repos, extract_root, &mut kotlin, &mut java) {
            files.push(batch.file.clone());
            ws.index
                .replace_file(&batch.file, batch.symbols, ktlsp::index::Tier::Durable);
        }
        indexed_files.insert(coord.clone(), files);
    }

    ws
}

/// Assert goto at the (last) usage of `token` lands on `token` in a file ending with `suffix`.
fn assert_goto_into_library(ws: &mut Workspace, key: &str, src: &str, token: &str, suffix: &str) {
    let offset = src.rfind(token).expect("token present in source");
    let defs = ws.goto_definition(key, offset);
    assert!(!defs.is_empty(), "goto on `{token}` returned nothing");
    let d = &defs[0];
    assert!(
        d.file.ends_with(suffix),
        "goto on `{token}` -> {} (expected a file ending in {suffix})",
        d.file
    );
    let target = fs::read_to_string(&d.file).expect("target file readable");
    assert_eq!(
        &target[d.start_byte..d.end_byte],
        token,
        "goto on `{token}` landed on the wrong identifier in {}",
        d.file
    );
}

#[test]
fn goto_uses_newest_selected_library_version() {
    let tmp = unique_tmp("libgoto_version_conflict");
    let older = Coordinate::parse("acme:demo:1.10.2").unwrap();
    let newer = Coordinate::parse("acme:demo:1.11.0").unwrap();

    let gradle_cache = tmp.join("gradle/caches");
    for (coord, value) in [(&older, "10"), (&newer, "11")] {
        let jar = gradle_cache
            .join("modules-2/files-2.1")
            .join(&coord.group)
            .join(&coord.artifact)
            .join(&coord.version)
            .join("deadbeef")
            .join(coord.sources_jar_name());
        write_sources_jar(
            &jar,
            &[(
                "acme/lib/Lib.kt",
                &format!("package acme.lib\n\nfun shared(): Int = {value}\n"),
            )],
        );
    }

    let repos = Repos {
        gradle_cache,
        m2: tmp.join("m2"),
        central_base: "http://127.0.0.1:0/unused".to_string(),
        download_dir: tmp.join("dl"),
        allow_download: false,
    };
    let extract_root = tmp.join("extracted");
    let mut ws = index_coordinates_with_selection(&[older, newer], &repos, &extract_root);

    let key = tmp.join("app/Main.kt").to_string_lossy().into_owned();
    let src = "package app\n\
               import acme.lib.shared\n\
               \n\
               fun main() {\n\
               \x20\x20\x20\x20shared()\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let offset = src.rfind("shared").expect("token present");
    let defs = ws.goto_definition(&key, offset);
    assert_eq!(
        defs.len(),
        1,
        "older library version should be shadowed: {defs:?}"
    );
    assert!(
        defs[0].file.contains("/1.11.0/"),
        "goto should land in the newer source tree, got {}",
        defs[0].file
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn goto_into_indexed_library_kotlin_and_java() {
    let tmp = unique_tmp("libgoto");
    let coord = Coordinate::parse("acme:demo:1.0").unwrap();

    // Fake Gradle cache: modules-2/files-2.1/{group}/{artifact}/{version}/{sha1}/{name}
    let gradle_cache = tmp.join("gradle/caches");
    let jar = gradle_cache
        .join("modules-2/files-2.1")
        .join(&coord.group)
        .join(&coord.artifact)
        .join(&coord.version)
        .join("deadbeef")
        .join(coord.sources_jar_name());
    write_sources_jar(
        &jar,
        &[
            (
                "acme/lib/Lib.kt",
                "package acme.lib\n\nfun libFunc(): Int = 1\n\nclass Widget(val size: Int)\n",
            ),
            (
                "acme/jlib/JThing.java",
                "package acme.jlib;\n\npublic class JThing {\n    public void run() {}\n}\n",
            ),
        ],
    );

    let repos = Repos {
        gradle_cache,
        m2: tmp.join("m2"),
        central_base: "http://127.0.0.1:0/unused".to_string(),
        download_dir: tmp.join("dl"),
        allow_download: false, // cache-only: no network in this test
    };
    let extract_root = tmp.join("extracted");
    let mut ws = index_into_workspace(&coord, &repos, &extract_root);

    // A user Kotlin file referencing the library symbols.
    let key = tmp.join("app/Main.kt").to_string_lossy().into_owned();
    let src = "package app\n\
               import acme.lib.Widget\n\
               import acme.lib.libFunc\n\
               import acme.jlib.JThing\n\
               \n\
               fun main() {\n\
               \x20\x20\x20\x20val w = Widget(3)\n\
               \x20\x20\x20\x20libFunc()\n\
               \x20\x20\x20\x20val j = JThing()\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    // Kotlin class (type via constructor call), Kotlin top-level function, and Java class.
    assert_goto_into_library(&mut ws, &key, src, "Widget", "Lib.kt");
    assert_goto_into_library(&mut ws, &key, src, "libFunc", "Lib.kt");
    assert_goto_into_library(&mut ws, &key, src, "JThing", "JThing.java");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn goto_into_jvm_variant_sources_for_multiplatform_coordinate() {
    let tmp = unique_tmp("libgoto_jvm_variant");
    let coord = Coordinate::parse("io.ktor:ktor-client-apache5:3.5.0").unwrap();
    let jvm_coord = Coordinate::parse("io.ktor:ktor-client-apache5-jvm:3.5.0").unwrap();

    let gradle_cache = tmp.join("gradle/caches");
    let empty_root_jar = gradle_cache
        .join("modules-2/files-2.1")
        .join(&coord.group)
        .join(&coord.artifact)
        .join(&coord.version)
        .join("feedface")
        .join(coord.sources_jar_name());
    write_sources_jar(
        &empty_root_jar,
        &[("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n")],
    );

    let jar = gradle_cache
        .join("modules-2/files-2.1")
        .join(&jvm_coord.group)
        .join(&jvm_coord.artifact)
        .join(&jvm_coord.version)
        .join("deadbeef")
        .join(jvm_coord.sources_jar_name());
    write_sources_jar(
        &jar,
        &[(
            "io/ktor/client/engine/apache5/Apache5.kt",
            "package io.ktor.client.engine.apache5\n\nobject Apache5\n",
        )],
    );

    let repos = Repos {
        gradle_cache,
        m2: tmp.join("m2"),
        central_base: "http://127.0.0.1:0/unused".to_string(),
        download_dir: tmp.join("dl"),
        allow_download: false,
    };
    let extract_root = tmp.join("extracted");
    let mut ws = index_into_workspace(&coord, &repos, &extract_root);

    let key = tmp.join("app/Main.kt").to_string_lossy().into_owned();
    let src = "package app\n\
               import io.ktor.client.engine.apache5.Apache5\n\
               \n\
               fun main() {\n\
               \x20\x20\x20\x20val engine = Apache5\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    assert_goto_into_library(&mut ws, &key, src, "Apache5", "Apache5.kt");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn goto_into_transitive_java_sources_discovered_from_metadata() {
    let tmp = unique_tmp("libgoto_transitive_java");
    let root_coord = Coordinate::parse("io.ktor:ktor-client-apache5:3.5.0").unwrap();
    let jvm_coord = Coordinate::parse("io.ktor:ktor-client-apache5-jvm:3.5.0").unwrap();
    let httpclient =
        Coordinate::parse("org.apache.httpcomponents.client5:httpclient5:5.5.1").unwrap();
    let httpcore = Coordinate::parse("org.apache.httpcomponents.core5:httpcore5:5.3.6").unwrap();

    let gradle_cache = tmp.join("gradle/caches");
    let module_dir = gradle_cache
        .join("modules-2/files-2.1")
        .join(&jvm_coord.group)
        .join(&jvm_coord.artifact)
        .join(&jvm_coord.version)
        .join("module");
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(
        module_dir.join(format!("{}-{}.module", jvm_coord.artifact, jvm_coord.version)),
        r#"{
  "variants": [
    { "name": "jvmApiElements", "dependencies": [
      { "group": "org.apache.httpcomponents.client5", "module": "httpclient5", "version": { "requires": "5.5.1" } }
    ] }
  ]
}"#,
    )
    .unwrap();

    let client_jar = gradle_cache
        .join("modules-2/files-2.1")
        .join(&httpclient.group)
        .join(&httpclient.artifact)
        .join(&httpclient.version)
        .join("client")
        .join(httpclient.sources_jar_name());
    write_sources_jar(
        &client_jar,
        &[
            (
                "org/apache/hc/client5/http/config/ConnectionConfig.java",
                "package org.apache.hc.client5.http.config;\n\npublic class ConnectionConfig {}\n",
            ),
            (
                "META-INF/maven/org.apache.httpcomponents.client5/httpclient5/pom.xml",
                "<project><dependencies><dependency><groupId>org.apache.httpcomponents.core5</groupId><artifactId>httpcore5</artifactId></dependency></dependencies></project>",
            ),
        ],
    );

    let core_jar = gradle_cache
        .join("modules-2/files-2.1")
        .join(&httpcore.group)
        .join(&httpcore.artifact)
        .join(&httpcore.version)
        .join("core")
        .join(httpcore.sources_jar_name());
    write_sources_jar(
        &core_jar,
        &[(
            "org/apache/hc/core5/util/TimeValue.java",
            "package org.apache.hc.core5.util;\n\npublic class TimeValue {}\n",
        )],
    );

    let repos = Repos {
        gradle_cache,
        m2: tmp.join("m2"),
        central_base: "http://127.0.0.1:0/unused".to_string(),
        download_dir: tmp.join("dl"),
        allow_download: false,
    };
    let extract_root = tmp.join("extracted");
    let mut ws = index_coordinate_closure_into_workspace(&root_coord, &repos, &extract_root);

    let key = tmp.join("app/Main.kt").to_string_lossy().into_owned();
    let src = "package app\n\
               import org.apache.hc.client5.http.config.ConnectionConfig\n\
               import org.apache.hc.core5.util.TimeValue\n\
               \n\
               fun main(config: ConnectionConfig, ttl: TimeValue) {}\n";
    ws.open(key.clone(), src.to_string());

    assert_goto_into_library(
        &mut ws,
        &key,
        src,
        "ConnectionConfig",
        "ConnectionConfig.java",
    );
    assert_goto_into_library(&mut ws, &key, src, "TimeValue", "TimeValue.java");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn goto_into_indexed_jdk_source() {
    let tmp = unique_tmp("jdkgoto");
    let src_zip = tmp.join("jdk/lib/src.zip");
    write_sources_jar(
        &src_zip,
        &[(
            "java.sql/java/sql/Connection.java",
            "package java.sql;\n\npublic interface Connection {\n    void close();\n}\n",
        )],
    );

    let extract_root = tmp.join("extracted");
    let mut kotlin = KotlinParser::new();
    let mut java = JavaParser::new();
    let batches = deps::resolve_jdk_sources(&src_zip, &extract_root, &mut kotlin, &mut java);
    assert!(!batches.is_empty(), "expected indexed JDK source files");

    let mut ws = Workspace::new();
    for batch in batches {
        ws.index
            .replace_file(&batch.file, batch.symbols, ktlsp::index::Tier::Durable);
    }

    let key = tmp.join("app/Main.kt").to_string_lossy().into_owned();
    let src = "package app\n\
               import java.sql.Connection\n\
               \n\
               class UsesConnection(private val conn: Connection)\n";
    ws.open(key.clone(), src.to_string());

    assert_goto_into_library(&mut ws, &key, src, "Connection", "Connection.java");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn goto_into_android_sdk_sources_from_kotlin_import() {
    let tmp = unique_tmp("androidgoto");
    let gradle = tmp.join("apps/mobile/android");
    let catalog = gradle.join("gradle/libs.versions.toml");
    let sdk = tmp.join("fake-android-sdk");
    let bitmap = sdk.join("sources/android-36/android/graphics/Bitmap.java");
    fs::create_dir_all(catalog.parent().unwrap()).unwrap();
    fs::create_dir_all(bitmap.parent().unwrap()).unwrap();
    fs::write(gradle.join("settings.gradle"), "").unwrap();
    fs::write(gradle.join("gradlew"), "").unwrap();
    fs::write(&catalog, "[versions]\ncompileSdk = \"36\"\n").unwrap();
    fs::write(
        gradle.join("local.properties"),
        format!("sdk.dir={}\n", sdk.display()),
    )
    .unwrap();
    fs::write(
        &bitmap,
        "package android.graphics; public final class Bitmap { public int getWidth() { return 0; } }",
    )
    .unwrap();

    let mut java = JavaParser::new();
    let batches = deps::resolve_android_imports(
        &tmp,
        &["android.graphics.Bitmap".to_string()],
        &tmp.join("extracted"),
        &mut java,
    );
    assert_eq!(batches.len(), 1);

    let mut ws = Workspace::new();
    for batch in batches {
        ws.index
            .replace_file(&batch.file, batch.symbols, ktlsp::index::Tier::Durable);
    }
    let key = tmp.join("Main.kt").to_string_lossy().into_owned();
    let src = "package app\nimport android.graphics.Bitmap\nfun use(value: Bitmap) = value\n";
    ws.open(key.clone(), src.to_string());

    let import_offset = src.find("Bitmap").unwrap();
    let import_defs = ws.goto_definition(&key, import_offset);
    assert_eq!(import_defs.len(), 1);
    assert!(import_defs[0]
        .file
        .ends_with("android/graphics/Bitmap.java"));
    assert_goto_into_library(&mut ws, &key, src, "Bitmap", "android/graphics/Bitmap.java");

    let _ = fs::remove_dir_all(&tmp);
}

/// Stage B: supertype + extension data recorded by the indexer must survive into the **Durable**
/// tier, so member completion on a user-file receiver of a library type offers inherited members
/// and library extensions. This proves the supertype/extension index is populated for libraries
/// (parsed through the same `extract_symbols` as project files).
#[test]
fn member_completion_into_indexed_library() {
    let tmp = unique_tmp("libcomplete");
    let coord = Coordinate::parse("acme:widgets:2.0").unwrap();

    let gradle_cache = tmp.join("gradle/caches");
    let jar = gradle_cache
        .join("modules-2/files-2.1")
        .join(&coord.group)
        .join(&coord.artifact)
        .join(&coord.version)
        .join("cafebabe")
        .join(coord.sources_jar_name());
    // A base class + a subclass + a top-level extension on the base, all in the library.
    write_sources_jar(
        &jar,
        &[(
            "acme/ui/Widgets.kt",
            "package acme.ui\n\
             \n\
             open class View {\n\
             \x20\x20\x20\x20fun render() {}\n\
             }\n\
             \n\
             class Button : View() {\n\
             \x20\x20\x20\x20fun click() {}\n\
             }\n\
             \n\
             fun View.highlight() {}\n",
        )],
    );

    let repos = Repos {
        gradle_cache,
        m2: tmp.join("m2"),
        central_base: "http://127.0.0.1:0/unused".to_string(),
        download_dir: tmp.join("dl"),
        allow_download: false,
    };
    let extract_root = tmp.join("extracted");
    let mut ws = index_into_workspace(&coord, &repos, &extract_root);

    // A user file constructing the library `Button` and completing after a dot.
    let key = tmp.join("app/Main.kt").to_string_lossy().into_owned();
    // `b.` is a bare trailing dot — exercises the placeholder-recovery path.
    let src = "package app\n\
               import acme.ui.Button\n\
               \n\
               fun main() {\n\
               \x20\x20\x20\x20val b = Button()\n\
               \x20\x20\x20\x20b.\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let dot = src.rfind("b.").unwrap() + "b.".len();
    let labels: std::collections::HashSet<String> = ws
        .complete(&key, dot, true)
        .expect("member completion should resolve the library type")
        .items
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert!(labels.contains("click"), "own member: {labels:?}");
    assert!(
        labels.contains("render"),
        "inherited from View (Durable supertype walk): {labels:?}"
    );
    assert!(
        labels.contains("highlight"),
        "library extension on View: {labels:?}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

/// Real download path: fetch a small sources jar from Maven Central, index it, and resolve goto.
/// Network-dependent, so ignored by default. Run with: `cargo test -- --ignored download`.
#[test]
#[ignore]
fn download_from_maven_central_and_goto() {
    let tmp = unique_tmp("libdownload");
    // org.jetbrains:annotations publishes a small sources jar containing Java sources.
    let coord = Coordinate::parse("org.jetbrains:annotations:24.1.0").unwrap();
    let repos = Repos {
        gradle_cache: tmp.join("empty-gradle"), // force a cache miss
        m2: tmp.join("empty-m2"),
        central_base: "https://repo1.maven.org/maven2".to_string(),
        download_dir: tmp.join("dl"),
        allow_download: true,
    };
    let extract_root = tmp.join("extracted");
    let mut ws = index_into_workspace(&coord, &repos, &extract_root);

    let key = tmp.join("app/Main.kt").to_string_lossy().into_owned();
    // `@NotNull` is the canonical annotation in this artifact (package org.jetbrains.annotations).
    let src = "package app\n\
               import org.jetbrains.annotations.NotNull\n\
               \n\
               fun use(x: NotNull) {}\n";
    ws.open(key.clone(), src.to_string());
    assert_goto_into_library(&mut ws, &key, src, "NotNull", ".java");

    let _ = fs::remove_dir_all(&tmp);
}
