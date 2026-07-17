use std::fs;
use std::io::Write;

use ktlsp::deps;
use ktlsp::index::Tier;
use ktlsp::java::JavaParser;
use ktlsp::parser::KotlinParser;
use ktlsp::workspace::Workspace;
use zip::write::SimpleFileOptions;

fn unique_tmp(prefix: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("{prefix}-{pid}-{n}"));
    fs::create_dir_all(&tmp).unwrap();
    tmp
}

#[test]
fn goto_java_class_in_same_project() {
    let root = unique_tmp("java_goto");
    let app = root.join("app");
    fs::create_dir_all(&app).unwrap();
    let helper_text =
        "package app;\n\npublic class Helper {\n    public Helper() { }\n    public void assist() { }\n}\n";
    fs::write(app.join("Helper.java"), helper_text).unwrap();
    fs::write(
        app.join("Main.java"),
        "package app;\n\npublic class Main {\n    public void run() {\n        Helper helper = new Helper();\n        helper.assist();\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        app.join("UseImport.java"),
        "package app;\n\nimport app.Helper;\n\npublic class UseImport {\n    Helper helper;\n}\n",
    )
    .unwrap();

    let mut ws = Workspace::new();
    ws.assume_index_complete_for_tests();
    let n = ws.scan(&root);
    assert_eq!(n, 3, "expected three java files indexed");

    let main = app.join("Main.java").to_string_lossy().to_string();
    let main_text = fs::read_to_string(&main).unwrap();
    let constructor_offset = main_text.find("new Helper").unwrap() + 8; // cursor on 'H'

    // Test both the closed-file and open-file paths.
    let defs = ws.goto_definition(&main, constructor_offset);
    assert!(
        !defs.is_empty(),
        "goto_definition (closed) should find Helper, got empty"
    );

    ws.open(main.clone(), main_text.clone());
    let defs = ws.goto_definition(&main, constructor_offset);
    assert!(
        !defs.is_empty(),
        "goto_definition (open) should find Helper, got empty"
    );
    let helper = app.join("Helper.java").to_string_lossy().to_string();
    let class_name_start = helper_text.find("class Helper").unwrap() + "class ".len();
    assert_eq!(
        defs,
        vec![ktlsp::symbol::Def {
            file: helper,
            start_byte: class_name_start,
            end_byte: class_name_start + "Helper".len(),
        }],
        "constructor calls should resolve to the class name only"
    );

    let type_offset = main_text.find("Helper helper").unwrap(); // cursor on 'H'
    let defs = ws.goto_definition(&main, type_offset);
    let helper = app.join("Helper.java").to_string_lossy().to_string();
    assert_eq!(
        defs,
        vec![ktlsp::symbol::Def {
            file: helper,
            start_byte: class_name_start,
            end_byte: class_name_start + "Helper".len(),
        }],
        "type references should resolve to the class name only"
    );

    let import_file = app.join("UseImport.java").to_string_lossy().to_string();
    let import_text = fs::read_to_string(&import_file).unwrap();
    let import_offset = import_text.find("import app.Helper").unwrap() + "import app.".len();
    ws.open(import_file.clone(), import_text.clone());
    let defs = ws.goto_definition(&import_file, import_offset);
    let helper = app.join("Helper.java").to_string_lossy().to_string();
    assert_eq!(
        defs,
        vec![ktlsp::symbol::Def {
            file: helper,
            start_byte: class_name_start,
            end_byte: class_name_start + "Helper".len(),
        }],
        "imported class names should resolve to the class name only"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn goto_java_class_from_local_binary_jar_stub() {
    let root = unique_tmp("java_local_jar_goto");
    let jar_path = root.join("libs/datatester-java-sdk-2.0.24.jar");
    fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
    let mut zip = zip::ZipWriter::new(fs::File::create(&jar_path).unwrap());
    for entry in [
        "com/bytedance/tester/AbClient.class",
        "com/bytedance/tester/AbClient$Builder.class",
        "com/bytedance/tester/model/User.class",
        "com/bytedance/tester/model/common/Variable.class",
    ] {
        zip.start_file(entry, SimpleFileOptions::default()).unwrap();
        zip.write_all(&[]).unwrap();
    }
    zip.finish().unwrap();

    let mut kotlin = KotlinParser::new();
    let mut java = JavaParser::new();
    let batches =
        deps::resolve_local_jar_stubs(&jar_path, &root.join("extracted"), &mut kotlin, &mut java);
    assert!(
        batches.iter().any(|batch| batch
            .symbols
            .iter()
            .any(|sym| { sym.package == "com.bytedance.tester.model" && sym.name == "User" })),
        "local jar stubs should index com.bytedance.tester.model.User"
    );

    let mut ws = Workspace::new();
    ws.assume_index_complete_for_tests();
    for batch in batches {
        ws.index
            .replace_file(&batch.file, batch.symbols, Tier::Durable);
    }

    let app = root.join("App.java").to_string_lossy().to_string();
    let src = "package app;\n\nimport com.bytedance.tester.model.User;\n\npublic class App {\n    User user;\n}\n";
    ws.open(app.clone(), src.to_string());
    let offset = src.find("import com.bytedance.tester.model.User").unwrap()
        + "import com.bytedance.tester.model.".len();
    let defs = ws.goto_definition(&app, offset);
    assert_eq!(defs.len(), 1, "goto on local jar import should resolve");
    assert!(
        defs[0]
            .file
            .ends_with("com/bytedance/tester/model/User.java"),
        "goto should land in the generated User.java stub, got {}",
        defs[0].file
    );

    let target = fs::read_to_string(&defs[0].file).unwrap();
    assert_eq!(&target[defs[0].start_byte..defs[0].end_byte], "User");

    let _ = fs::remove_dir_all(&root);
}
