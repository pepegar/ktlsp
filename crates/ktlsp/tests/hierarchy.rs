use ktlsp::workspace::Workspace;

#[test]
fn implementation_finds_direct_subtypes() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\ninterface Greeter\nclass ConsoleGreeter : Greeter\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.implementation(&key, src.find("Greeter").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "ConsoleGreeter");
}

#[test]
fn implementation_finds_overriding_kotlin_member() {
    let mut ws = Workspace::new();
    let key = "mem:///Registry.kt".to_string();
    let src = "package app\n\
               interface Registry {\n\
                   fun describeTable()\n\
               }\n\
               class RegistryImpl : Registry {\n\
                   override fun describeTable() {}\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.implementation(&key, src.find("describeTable").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "describeTable");
    assert_eq!(defs[0].start_byte, src.rfind("describeTable").unwrap());
}

#[test]
fn implementation_finds_overriding_kotlin_constructor_property() {
    let mut ws = Workspace::new();
    let key = "mem:///Registry.kt".to_string();
    let src = "package app\n\
               interface Registry {\n\
                   val metricsService: String\n\
               }\n\
               class RegistryImpl(\n\
                   override val metricsService: String\n\
               ) : Registry\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.implementation(&key, src.find("metricsService").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "metricsService");
    assert_eq!(defs[0].start_byte, src.rfind("metricsService").unwrap());
}

#[test]
fn implementation_disambiguates_overloaded_kotlin_members() {
    let mut ws = Workspace::new();
    let key = "mem:///Worker.kt".to_string();
    let src = "package app\n\
               interface Worker {\n\
                   fun work(value: String)\n\
                   fun work(value: Int)\n\
               }\n\
               class WorkerImpl : Worker {\n\
                   override fun work(value: String) {}\n\
                   override fun work(value: Int) {}\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.implementation(&key, src.find("work(value: String)").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(
        defs[0].start_byte,
        src.rfind("work(value: String)").unwrap()
    );
}

#[test]
fn type_definition_uses_inferred_constructor_type() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nclass GreeterImpl\nfun main() {\n    val g = GreeterImpl()\n    println(g)\n}\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.type_definition(&key, src.rfind("g)").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "GreeterImpl");
}

#[test]
fn java_type_definition_resolves_value_identifier_type() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\nclass Helper {}\nclass Main {\n    Helper helper;\n    void run() { helper.toString(); }\n}\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.type_definition(&key, src.rfind("helper.toString").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "Helper");
}

#[test]
fn java_type_definition_resolves_type_reference() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\nclass Helper {}\nclass Main {\n    Helper helper;\n}\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.type_definition(&key, src.rfind("Helper helper").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "Helper");
}

#[test]
fn call_hierarchy_reports_incoming_and_outgoing_calls() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nfun helper(): Int = 1\nfun caller(): Int = helper()\n";
    ws.open(key.clone(), src.to_string());

    let helper = ws
        .hierarchy_item_at(&key, src.find("helper").unwrap())
        .expect("helper hierarchy item");
    let incoming = ws.incoming_calls(&helper);
    assert_eq!(incoming.len(), 1, "{incoming:?}");
    assert_eq!(incoming[0].from.name, "caller");

    let caller = ws
        .hierarchy_item_at(&key, src.find("caller").unwrap())
        .expect("caller hierarchy item");
    let outgoing = ws.outgoing_calls(&caller);
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    assert_eq!(outgoing[0].to.name, "helper");
}

#[test]
fn java_call_hierarchy_reports_incoming_and_outgoing_calls() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src =
        "package app;\nclass Helper {\n    void assist() {}\n    void run() { assist(); }\n}\n";
    ws.open(key.clone(), src.to_string());

    let assist = ws
        .hierarchy_item_at(&key, src.find("assist").unwrap())
        .expect("assist hierarchy item");
    let incoming = ws.incoming_calls(&assist);
    assert_eq!(incoming.len(), 1, "{incoming:?}");
    assert_eq!(incoming[0].from.name, "run");

    let run = ws
        .hierarchy_item_at(&key, src.find("run").unwrap())
        .expect("run hierarchy item");
    let outgoing = ws.outgoing_calls(&run);
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    assert_eq!(outgoing[0].to.name, "assist");
}

#[test]
fn type_hierarchy_returns_direct_neighbors() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nopen class Base\nclass Child : Base\n";
    ws.open(key.clone(), src.to_string());

    let base = ws
        .hierarchy_item_at(&key, src.find("Base").unwrap())
        .expect("base item");
    let child = ws
        .hierarchy_item_at(&key, src.find("Child").unwrap())
        .expect("child item");

    assert_eq!(ws.type_subtypes(&base)[0].name, "Child");
    assert_eq!(ws.type_supertypes(&child)[0].name, "Base");
}

#[test]
fn java_implementation_finds_direct_subtypes() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src = "package app;\ninterface Named {}\nclass Child implements Named {}\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.implementation(&key, src.find("Named").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "Child");
}

#[test]
fn implementation_finds_overriding_java_member() {
    let mut ws = Workspace::new();
    let key = "mem:///Registry.java".to_string();
    let src = "package app;\n\
               interface Registry { void describeTable(); }\n\
               class RegistryImpl implements Registry {\n\
                   public void describeTable() {}\n\
               }\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.implementation(&key, src.find("describeTable").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "describeTable");
    assert_eq!(defs[0].start_byte, src.rfind("describeTable").unwrap());
}

#[test]
fn java_type_hierarchy_returns_direct_neighbors() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.java".to_string();
    let src =
        "package app;\ninterface Named {}\nclass Base {}\nclass Child extends Base implements Named {}\n";
    ws.open(key.clone(), src.to_string());

    let base = ws
        .hierarchy_item_at(&key, src.find("Base").unwrap())
        .expect("base item");
    let named = ws
        .hierarchy_item_at(&key, src.find("Named").unwrap())
        .expect("named item");
    let child = ws
        .hierarchy_item_at(&key, src.find("Child").unwrap())
        .expect("child item");

    assert_eq!(ws.type_subtypes(&base)[0].name, "Child");
    assert_eq!(ws.type_subtypes(&named)[0].name, "Child");

    let supertypes = ws
        .type_supertypes(&child)
        .into_iter()
        .map(|item| item.name)
        .collect::<Vec<_>>();
    assert_eq!(supertypes, vec!["Base".to_string(), "Named".to_string()]);
}
